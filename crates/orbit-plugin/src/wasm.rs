//! The WebAssembly sandbox: plugin logic with no ambient authority.
//!
//! Modules run on `wasmi`, an interpreter (see `docs/PLUGINS.md`, "Why
//! wasmi"). A module may import exactly two functions, both from the
//! `orbit` module, and nothing else: no WASI, so no files, clock, network,
//! environment or randomness unless the host API hands them out.
//!
//! ```text
//! import orbit.log(ptr: i32, len: i32)
//! import orbit.call(method_ptr: i32, method_len: i32, params_ptr: i32, params_len: i32) -> i64
//! export memory
//! export orbit_alloc(len: i32) -> i32
//! export orbit_hook(ptr: i32, len: i32) -> i64      (optional)
//! export orbit_command(ptr: i32, len: i32) -> i64   (optional)
//! export orbit_action(ptr: i32, len: i32) -> i64    (optional)
//! ```
//!
//! Inputs and outputs are UTF-8 JSON in the module's memory. An entry
//! point returns `ptr << 32 | len` of its answer, or 0 for none.
//! `orbit.call` answers `{"ok": …}` or `{"error": {"code", "message"}}`
//! in memory it asks the module to allocate.
//!
//! Every call has a fuel budget (WebAssembly instructions, so a loop ends),
//! a memory cap, a wall-clock budget, a cap on host calls and log lines,
//! and a size cap on what crosses the boundary. A call that fails leaves
//! the instance behind: the next one starts from a fresh instance.

use std::time::{Duration, Instant};

use wasmi::{Caller, Config, Engine, Extern, Instance, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};

pub const IMPORT_MODULE: &str = "orbit";
pub const ENTRY_HOOK: &str = "orbit_hook";
pub const ENTRY_COMMAND: &str = "orbit_command";
pub const ENTRY_ACTION: &str = "orbit_action";
const ALLOC: &str = "orbit_alloc";
/// What may cross the boundary in one message.
pub const MAX_MESSAGE: usize = 1024 * 1024;
const MAX_LOG_LINE: usize = 4 * 1024;
const MAX_LOGS_PER_CALL: u32 = 64;
const MAX_HOST_CALLS_PER_CALL: u32 = 256;

/// The host API a module reaches through `orbit.call`, for one plugin.
/// It owns what it needs (the call runs with no borrow of the engine).
pub trait HostApi: Send + std::any::Any {
    /// Runs `method` with JSON `params`; returns the JSON result.
    fn call(&mut self, method: &str, params: &str) -> Result<String, ApiError>;
    fn log(&mut self, line: &str);
}

/// A host API error: its code is one of a closed set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiError {
    /// `invalid_params`, `capability_denied`, `not_found`, `too_large`,
    /// `rate_limited`, `unavailable`.
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

/// How a call into a module failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallError {
    /// The module has no such entry point.
    Missing(String),
    OutOfFuel,
    /// It finished, but after its wall-clock budget; the answer is dropped.
    TooSlow(Duration),
    Trap(String),
    /// The answer is not UTF-8, out of bounds or too large.
    BadOutput(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Missing(entry) => write!(f, "the module has no {entry}"),
            CallError::OutOfFuel => write!(f, "it ran out of fuel (too much work for one call)"),
            CallError::TooSlow(d) => write!(f, "it took {} ms, over its budget", d.as_millis()),
            CallError::Trap(e) => write!(f, "it failed: {e}"),
            CallError::BadOutput(e) => write!(f, "its answer was unusable: {e}"),
        }
    }
}

/// Per-call and per-module limits.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub fuel: u64,
    pub memory_bytes: usize,
    pub timeout: Duration,
}

struct Data {
    limits: StoreLimits,
    api: Option<Box<dyn HostApi>>,
    host_calls: u32,
    logs: u32,
}

/// One plugin's module and its live instance.
pub struct Sandbox {
    engine: Engine,
    module: Module,
    linker: Linker<Data>,
    store: Store<Data>,
    instance: Option<Instance>,
    limits: Limits,
}

impl Sandbox {
    /// Compiles and checks a module (imports, exports), and instantiates it
    /// under the limits.
    pub fn new(bytes: &[u8], limits: Limits) -> Result<Self, String> {
        let mut config = Config::default();
        config.consume_fuel(true);
        let engine = Engine::new(&config);
        let module = Module::new(&engine, bytes).map_err(|e| format!("not a valid WebAssembly module ({e})"))?;
        for import in module.imports() {
            if import.module() != IMPORT_MODULE || !matches!(import.name(), "log" | "call") {
                return Err(format!(
                    "it imports {}.{}; plugins may only import orbit.log and orbit.call (no WASI)",
                    import.module(),
                    import.name()
                ));
            }
        }
        for needed in ["memory", ALLOC] {
            if module.get_export(needed).is_none() {
                return Err(format!("it doesn’t export {needed}"));
            }
        }
        let mut linker = Linker::<Data>::new(&engine);
        linker.func_wrap(IMPORT_MODULE, "log", host_log).map_err(|e| e.to_string())?;
        linker.func_wrap(IMPORT_MODULE, "call", host_call).map_err(|e| e.to_string())?;
        let store = new_store(&engine, limits);
        let mut sandbox = Self { engine, module, linker, store, instance: None, limits };
        sandbox.instantiate().map_err(|e| format!("it couldn’t start: {e}"))?;
        Ok(sandbox)
    }

    fn instantiate(&mut self) -> Result<(), CallError> {
        self.store = new_store(&self.engine, self.limits);
        self.store.set_fuel(self.limits.fuel).map_err(|e| CallError::Trap(e.to_string()))?;
        let instance = self.linker.instantiate_and_start(&mut self.store, &self.module).map_err(classify)?;
        self.instance = Some(instance);
        Ok(())
    }

    /// Whether the module exports `entry`.
    pub fn has(&self, entry: &str) -> bool {
        self.module.get_export(entry).is_some()
    }

    /// Calls `entry` with `input` (JSON). The API object is handed back
    /// whatever happens, with everything the call did recorded in it.
    pub fn invoke(&mut self, entry: &str, input: &str, api: Box<dyn HostApi>) -> (Box<dyn HostApi>, Result<Option<String>, CallError>) {
        let started = Instant::now();
        let result = self.invoke_inner(entry, input, api);
        let elapsed = started.elapsed();
        let api = self.store.data_mut().api.take().expect("the API object stays in the store during a call");
        let result = match result {
            Ok(_) if elapsed > self.limits.timeout => Err(CallError::TooSlow(elapsed)),
            other => other,
        };
        if result.is_err() {
            // Whatever state the module was left in is not trusted again.
            self.instance = None;
        }
        (api, result)
    }

    fn invoke_inner(&mut self, entry: &str, input: &str, api: Box<dyn HostApi>) -> Result<Option<String>, CallError> {
        if self.instance.is_none() && self.has(entry) {
            if let Err(e) = self.instantiate() {
                self.store.data_mut().api = Some(api);
                return Err(e);
            }
        }
        self.store.data_mut().api = Some(api);
        if !self.has(entry) {
            return Err(CallError::Missing(entry.to_owned()));
        }
        if input.len() > MAX_MESSAGE {
            return Err(CallError::BadOutput("the input is too large".into()));
        }
        let instance = self.instance.ok_or_else(|| CallError::Trap("no instance".into()))?;
        self.store.set_fuel(self.limits.fuel).map_err(|e| CallError::Trap(e.to_string()))?;
        let data = self.store.data_mut();
        data.host_calls = 0;
        data.logs = 0;
        let alloc = instance.get_typed_func::<i32, i32>(&self.store, ALLOC).map_err(|e| CallError::Trap(e.to_string()))?;
        let memory = instance.get_memory(&self.store, "memory").ok_or_else(|| CallError::Trap("no memory".into()))?;
        let ptr = alloc.call(&mut self.store, input.len() as i32).map_err(classify)?;
        if ptr == 0 && !input.is_empty() {
            return Err(CallError::Trap("orbit_alloc couldn’t allocate the input".into()));
        }
        memory
            .write(&mut self.store, ptr as u32 as usize, input.as_bytes())
            .map_err(|_| CallError::BadOutput("orbit_alloc returned memory out of bounds".into()))?;
        let func = instance
            .get_typed_func::<(i32, i32), i64>(&self.store, entry)
            .map_err(|e| CallError::Trap(format!("{entry} has the wrong signature: {e}")))?;
        let packed = func.call(&mut self.store, (ptr, input.len() as i32)).map_err(classify)?;
        if packed == 0 {
            return Ok(None);
        }
        let (ptr, len) = ((packed as u64 >> 32) as usize, (packed as u64 & 0xffff_ffff) as usize);
        if len > MAX_MESSAGE {
            return Err(CallError::BadOutput(format!("{len} bytes")));
        }
        let mut buf = vec![0u8; len];
        memory.read(&self.store, ptr, &mut buf).map_err(|_| CallError::BadOutput("out of bounds".into()))?;
        String::from_utf8(buf).map(Some).map_err(|_| CallError::BadOutput("not UTF-8".into()))
    }

    /// Memory the instance holds now, in bytes.
    pub fn memory_bytes(&self) -> usize {
        self.instance
            .and_then(|i| i.get_memory(&self.store, "memory"))
            .map(|m| m.data_size(&self.store))
            .unwrap_or(0)
    }
}

fn new_store(engine: &Engine, limits: Limits) -> Store<Data> {
    let store_limits = StoreLimitsBuilder::new()
        .memory_size(limits.memory_bytes)
        .memories(1)
        .tables(4)
        .table_elements(100_000)
        .instances(1)
        .build();
    let mut store = Store::new(engine, Data { limits: store_limits, api: None, host_calls: 0, logs: 0 });
    store.limiter(|data| &mut data.limits);
    store
}

fn classify(e: wasmi::Error) -> CallError {
    let text = e.to_string();
    if e.as_trap_code() == Some(wasmi::TrapCode::OutOfFuel) || text.contains("fuel") {
        CallError::OutOfFuel
    } else {
        CallError::Trap(text)
    }
}

fn read_str(caller: &Caller<'_, Data>, ptr: i32, len: i32, max: usize) -> Result<String, wasmi::Error> {
    let len = len as u32 as usize;
    if len > max {
        return Err(wasmi::Error::new("a message to the host is too large"));
    }
    let memory = caller.get_export("memory").and_then(Extern::into_memory).ok_or_else(|| wasmi::Error::new("no memory"))?;
    let mut buf = vec![0u8; len];
    memory.read(caller, ptr as u32 as usize, &mut buf).map_err(|_| wasmi::Error::new("out of bounds"))?;
    String::from_utf8(buf).map_err(|_| wasmi::Error::new("a message to the host isn’t UTF-8"))
}

fn host_log(mut caller: Caller<'_, Data>, ptr: i32, len: i32) -> Result<(), wasmi::Error> {
    if caller.data().logs >= MAX_LOGS_PER_CALL {
        return Ok(());
    }
    let len = (len as u32).min(MAX_LOG_LINE as u32) as i32;
    let line = read_str(&caller, ptr, len, MAX_LOG_LINE)?;
    let data = caller.data_mut();
    data.logs += 1;
    if let Some(api) = data.api.as_mut() {
        api.log(&line);
    }
    Ok(())
}

fn host_call(mut caller: Caller<'_, Data>, mptr: i32, mlen: i32, pptr: i32, plen: i32) -> Result<i64, wasmi::Error> {
    let method = read_str(&caller, mptr, mlen, 128)?;
    let params = read_str(&caller, pptr, plen, MAX_MESSAGE)?;
    let data = caller.data_mut();
    data.host_calls += 1;
    let answer = if data.host_calls > MAX_HOST_CALLS_PER_CALL {
        envelope(Err(ApiError::new("rate_limited", "Too many host calls in one call.")))
    } else {
        match data.api.as_mut() {
            Some(api) => envelope(api.call(&method, &params)),
            None => envelope(Err(ApiError::new("unavailable", "No host API."))),
        }
    };
    if answer.len() > MAX_MESSAGE {
        return Err(wasmi::Error::new("the host’s answer is too large"));
    }
    let alloc = caller
        .get_export(ALLOC)
        .and_then(Extern::into_func)
        .ok_or_else(|| wasmi::Error::new("no orbit_alloc"))?
        .typed::<i32, i32>(&caller)?;
    let ptr = alloc.call(&mut caller, answer.len() as i32)?;
    let memory = caller.get_export("memory").and_then(Extern::into_memory).ok_or_else(|| wasmi::Error::new("no memory"))?;
    memory.write(&mut caller, ptr as u32 as usize, answer.as_bytes()).map_err(|_| wasmi::Error::new("out of bounds"))?;
    Ok(((ptr as u32 as i64) << 32) | answer.len() as i64)
}

fn envelope(result: Result<String, ApiError>) -> String {
    match result {
        Ok(json) => format!("{{\"ok\":{}}}", if json.is_empty() { "null" } else { &json }),
        Err(e) => format!(
            "{{\"error\":{{\"code\":{},\"message\":{}}}}}",
            sonic_rs::to_string(e.code).unwrap_or_default(),
            sonic_rs::to_string(&e.message).unwrap_or_default()
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use parking_lot::Mutex;

    use super::*;

    /// A module with a bump allocator: `orbit_hook` logs and answers a fixed
    /// denial, `orbit_command` loops forever, `orbit_action` calls the host.
    pub const GUEST: &str = r#"
(module
  (import "orbit" "log" (func $log (param i32 i32)))
  (import "orbit" "call" (func $call (param i32 i32 i32 i32) (result i64)))
  (memory (export "memory") 1)
  (global $next (mut i32) (i32.const 1024))
  (data (i32.const 16) "{\"deny\":\"no\"}")
  (data (i32.const 64) "storage.get")
  (data (i32.const 96) "{\"key\":\"k\"}")
  (data (i32.const 128) "hello")
  (func (export "orbit_alloc") (param $len i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $next))
    (global.set $next (i32.add (global.get $next) (local.get $len)))
    (local.get $p))
  (func (export "orbit_hook") (param i32 i32) (result i64)
    (call $log (i32.const 128) (i32.const 5))
    (i64.or (i64.shl (i64.const 16) (i64.const 32)) (i64.const 13)))
  (func (export "orbit_command") (param i32 i32) (result i64)
    (loop $forever (br $forever))
    (i64.const 0))
  (func (export "orbit_action") (param i32 i32) (result i64)
    (call $call (i32.const 64) (i32.const 11) (i32.const 96) (i32.const 11))))
"#;

    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<(Vec<(String, String)>, Vec<String>)>>);

    impl HostApi for Recorder {
        fn call(&mut self, method: &str, params: &str) -> Result<String, ApiError> {
            self.0.lock().0.push((method.into(), params.into()));
            Ok("\"v\"".into())
        }
        fn log(&mut self, line: &str) {
            self.0.lock().1.push(line.into());
        }
    }

    fn limits() -> Limits {
        Limits { fuel: 1_000_000, memory_bytes: 1 << 20, timeout: Duration::from_secs(5) }
    }

    #[test]
    fn entry_points_answer_and_log() {
        let bytes = wat::parse_str(GUEST).unwrap();
        let mut sandbox = Sandbox::new(&bytes, limits()).unwrap();
        let recorder = Recorder::default();
        let (_, result) = sandbox.invoke(ENTRY_HOOK, "{}", Box::new(recorder.clone()));
        assert_eq!(result.unwrap().as_deref(), Some("{\"deny\":\"no\"}"));
        assert_eq!(recorder.0.lock().1, ["hello"]);
    }

    #[test]
    fn host_calls_go_through_the_api() {
        let bytes = wat::parse_str(GUEST).unwrap();
        let mut sandbox = Sandbox::new(&bytes, limits()).unwrap();
        let recorder = Recorder::default();
        let (_, result) = sandbox.invoke(ENTRY_ACTION, "{}", Box::new(recorder.clone()));
        // The action returns the host's answer as it got it.
        assert_eq!(result.unwrap().as_deref(), Some("{\"ok\":\"v\"}"));
        assert_eq!(recorder.0.lock().0, [("storage.get".to_owned(), "{\"key\":\"k\"}".to_owned())]);
    }

    #[test]
    fn endless_loops_run_out_of_fuel_and_the_next_call_starts_fresh() {
        let bytes = wat::parse_str(GUEST).unwrap();
        let mut sandbox = Sandbox::new(&bytes, limits()).unwrap();
        let started = Instant::now();
        let (api, result) = sandbox.invoke(ENTRY_COMMAND, "{}", Box::new(Recorder::default()));
        assert_eq!(result.unwrap_err(), CallError::OutOfFuel);
        assert!(started.elapsed() < Duration::from_secs(2));
        let (_, again) = sandbox.invoke(ENTRY_HOOK, "{}", api);
        assert!(again.is_ok(), "a fresh instance after the trap");
    }

    #[test]
    fn a_wall_clock_budget_drops_slow_answers() {
        let bytes = wat::parse_str(GUEST).unwrap();
        let mut sandbox = Sandbox::new(&bytes, Limits { timeout: Duration::ZERO, ..limits() }).unwrap();
        let (_, result) = sandbox.invoke(ENTRY_HOOK, "{}", Box::new(Recorder::default()));
        assert!(matches!(result, Err(CallError::TooSlow(_))));
    }

    #[test]
    fn memory_is_capped() {
        // Tries to grow by 100 pages (6.4 MiB) past the 1 MiB cap and
        // answers how many pages it has.
        let grow = r#"(module (memory (export "memory") 1)
            (func (export "orbit_alloc") (param i32) (result i32) (i32.const 0))
            (func (export "orbit_hook") (param i32 i32) (result i64)
              (drop (memory.grow (i32.const 100))) (i64.extend_i32_s (memory.size))))"#;
        let mut sandbox = Sandbox::new(&wat::parse_str(grow).unwrap(), limits()).unwrap();
        let (_, result) = sandbox.invoke(ENTRY_HOOK, "", Box::new(Recorder::default()));
        // One page: read as an answer of 1 byte at address 0.
        assert_eq!(result.unwrap().as_deref(), Some("\0"));
        assert!(sandbox.memory_bytes() <= 1 << 20);
    }

    #[test]
    fn wasi_and_other_imports_are_refused() {
        let wasi = r#"(module (import "wasi_snapshot_preview1" "fd_write" (func (param i32 i32 i32 i32) (result i32)))
            (memory (export "memory") 1) (func (export "orbit_alloc") (param i32) (result i32) (i32.const 0)))"#;
        let err = Sandbox::new(&wat::parse_str(wasi).unwrap(), limits()).err().unwrap();
        assert!(err.contains("no WASI"), "{err}");
        let no_alloc = r#"(module (memory (export "memory") 1))"#;
        assert!(Sandbox::new(&wat::parse_str(no_alloc).unwrap(), limits()).err().unwrap().contains("orbit_alloc"));
        assert!(Sandbox::new(b"\0asm junk", limits()).is_err());
    }
}
