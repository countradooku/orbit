//! guard-rm: an `agent.tool.before` hook that blocks `rm -rf /`-style
//! commands, built for Orbit's plugin ABI (see `docs/PLUGINS.md`).
//!
//! No allocator, no std, no imports but `orbit.log`: it reads the hook's
//! JSON, finds the command, splits it into simple commands and refuses a
//! recursive delete of a root, home or system folder.

#![no_std]

use core::cell::UnsafeCell;

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

#[link(wasm_import_module = "orbit")]
unsafe extern "C" {
    #[link_name = "log"]
    fn orbit_log(ptr: *const u8, len: usize);
}

const ARENA: usize = 256 * 1024;

/// Memory the host writes inputs and reads answers in: a bump arena,
/// reset when the next call's input is allocated. All zeros, so it costs
/// nothing in the module file.
struct Bytes(UnsafeCell<[u8; ARENA]>);
struct Word(UnsafeCell<usize>);

// SAFETY: a WebAssembly instance runs one call at a time on one thread.
unsafe impl Sync for Bytes {}
unsafe impl Sync for Word {}

static HEAP: Bytes = Bytes(UnsafeCell::new([0; ARENA]));
static NEXT: Word = Word(UnsafeCell::new(0));
/// Non-zero once a call has returned: the next allocation starts over.
static DONE: Word = Word(UnsafeCell::new(0));

fn alloc(len: usize) -> Option<&'static mut [u8]> {
    // SAFETY: single-threaded; slices handed out never overlap.
    unsafe {
        if *DONE.0.get() != 0 {
            *NEXT.0.get() = 0;
            *DONE.0.get() = 0;
        }
        let start = *NEXT.0.get();
        let end = start.checked_add(len)?;
        if end > ARENA {
            return None;
        }
        *NEXT.0.get() = end;
        let bytes: &mut [u8; ARENA] = &mut *HEAP.0.get();
        Some(&mut bytes[start..end])
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn orbit_alloc(len: i32) -> i32 {
    match alloc(len.max(0) as usize) {
        Some(buf) => buf.as_mut_ptr() as i32,
        None => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn orbit_hook(ptr: i32, len: i32) -> i64 {
    // SAFETY: the host wrote `len` bytes at `ptr`, which `orbit_alloc` gave.
    let input = unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) };
    // The next allocation is the next call's input.
    unsafe { *DONE.0.get() = 1 };
    let mut command = [0u8; 16 * 1024];
    let Some(n) = command_of(input, &mut command) else { return 0 };
    match verdict(&command[..n]) {
        Some(target) => answer(target),
        None => 0,
    }
}

/// `{"deny":"…"}` in the arena, packed as `ptr << 32 | len`.
fn answer(target: &[u8]) -> i64 {
    const HEAD: &[u8] = b"{\"deny\":\"Recursively deletes ";
    const TAIL: &[u8] = b"; guard-rm blocks rm -rf / and its kin.\"}";
    let mut out = [0u8; 256];
    out[..HEAD.len()].copy_from_slice(HEAD);
    let mut n = HEAD.len();
    for &b in target.iter().take(out.len() - HEAD.len() - TAIL.len()) {
        // Keep the answer valid JSON whatever the target holds.
        out[n] = if b == b'"' || b == b'\\' || b < 0x20 || b > 0x7e { b'?' } else { b };
        n += 1;
    }
    out[n..n + TAIL.len()].copy_from_slice(TAIL);
    n += TAIL.len();
    unsafe { orbit_log(out.as_ptr(), n) };
    match alloc(n) {
        Some(buf) => {
            buf.copy_from_slice(&out[..n]);
            ((buf.as_ptr() as i64) << 32) | n as i64
        }
        None => 0,
    }
}

/// The JSON string value of `"command"` inside `"tool"`, unescaped.
fn command_of(json: &[u8], out: &mut [u8]) -> Option<usize> {
    let tool = find(json, b"\"tool\":")?;
    let rest = &json[tool..];
    if find(rest, b"\"kind\":\"command\"").is_none() {
        return None;
    }
    let at = find(rest, b"\"command\":\"")? + b"\"command\":\"".len();
    let mut n = 0;
    let mut i = at;
    let s = rest;
    while i < s.len() {
        let b = s[i];
        let c = match b {
            b'"' => return Some(n),
            b'\\' => {
                i += 1;
                match *s.get(i)? {
                    b'n' | b't' | b'r' => b' ',
                    b'u' => {
                        i += 4;
                        b'?'
                    }
                    other => other,
                }
            }
            other => other,
        };
        if n < out.len() {
            out[n] = c;
            n += 1;
        }
        i += 1;
    }
    None
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The dangerous target of the first simple command that deletes one.
fn verdict(command: &[u8]) -> Option<&[u8]> {
    let mut words: [&[u8]; 64] = [&[]; 64];
    let mut start = 0;
    let mut i = 0;
    while i <= command.len() {
        let end = i == command.len();
        let sep = !end && matches!(command[i], b';' | b'&' | b'|' | b'\n' | b'(' | b')' | b'`');
        if end || sep {
            let n = split_words(&command[start..i], &mut words);
            if let Some(target) = deletes_everything(&words[..n]) {
                return Some(target);
            }
            start = i + 1;
        }
        i += 1;
    }
    None
}

fn split_words<'a>(text: &'a [u8], words: &mut [&'a [u8]; 64]) -> usize {
    let mut n = 0;
    for word in text.split(|b| b.is_ascii_whitespace()).filter(|w| !w.is_empty()) {
        if n == words.len() {
            break;
        }
        words[n] = trim_quotes(word);
        n += 1;
    }
    n
}

fn trim_quotes(w: &[u8]) -> &[u8] {
    let mut w = w;
    while let [b'"' | b'\'', rest @ ..] = w {
        w = rest;
    }
    while let [rest @ .., b'"' | b'\''] = w {
        w = rest;
    }
    w
}

fn eq(a: &[u8], b: &str) -> bool {
    a.eq_ignore_ascii_case(b.as_bytes())
}

/// Recursive deletes of a root, home or system folder.
fn deletes_everything<'a>(words: &[&'a [u8]]) -> Option<&'a [u8]> {
    let mut words = words;
    // Wrappers that run the rest as a command.
    while let [first, rest @ ..] = words {
        let wrapper = ["sudo", "doas", "command", "exec", "nohup", "time", "nice", "env", "xargs", "busybox"].iter().any(|w| eq(first, w));
        let assignment = first.contains(&b'=') && !first.starts_with(b"-");
        if wrapper || assignment {
            words = rest;
        } else {
            break;
        }
    }
    let (program, args) = words.split_first()?;
    let program = program.rsplit(|b| *b == b'/' || *b == b'\\').next().unwrap_or(program);
    let program = program.strip_suffix(b".exe").unwrap_or(program);
    let mut recursive = false;
    let mut target = None;
    if eq(program, "rm") || eq(program, "remove-item") || eq(program, "ri") || eq(program, "del") || eq(program, "rmdir") || eq(program, "rd") {
        for arg in args {
            if arg.starts_with(b"--") {
                recursive |= eq(arg, "--recursive") || eq(arg, "--no-preserve-root");
            } else if arg.starts_with(b"-") && arg.len() > 1 {
                recursive |= arg[1..].iter().any(|c| *c == b'r' || *c == b'R') || eq(arg, "-recurse");
            } else if eq(arg, "/s") {
                recursive = true;
            } else if dangerous(arg) {
                target = Some(*arg);
            }
        }
    }
    if recursive { target } else { None }
}

fn dangerous(path: &[u8]) -> bool {
    let mut p = path;
    // `/`, `C:\` and `~/` keep their slash; `/usr/` does not.
    while p.len() > 1 && matches!(p.last(), Some(b'/' | b'\\')) && !(p.len() == 3 && p[1] == b':') {
        p = &p[..p.len() - 1];
    }
    const ROOTS: &[&str] = &[
        "/", "/*", "~", "~/", "~/*", "$HOME", "${HOME}", "$HOME/*", "$env:USERPROFILE", "$env:SystemDrive", "/home", "/Users",
        "/etc", "/usr", "/var", "/bin", "/sbin", "/lib", "/boot", "/System", "/Applications", "/opt", "/root", "C:", "C:\\",
        "C:/", "C:\\*", "C:\\Windows", "C:\\Users", "C:\\Program Files",
    ];
    ROOTS.iter().any(|r| eq(p, r))
}
