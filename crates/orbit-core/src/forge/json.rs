//! Null-tolerant reads from host and tracker JSON.
//!
//! APIs send `null` for absent values as often as they omit them, and add
//! fields freely; reading by path with defaults keeps parsing total, so one
//! odd record never fails a whole list.

use sonic_rs::{JsonContainerTrait, JsonValueTrait, Value};

use super::ForgeError;

pub(crate) fn parse(bytes: &[u8], what: &str) -> Result<Value, ForgeError> {
    sonic_rs::from_slice(bytes).map_err(|e| ForgeError::Parse(format!("{what}: {e}")))
}

/// The value at a `/`-separated path (`author/login`), or null.
pub(crate) fn at<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = v;
    for key in path.split('/') {
        cur = cur.as_object()?.get(&key)?;
    }
    (!cur.is_null()).then_some(cur)
}

pub(crate) fn s(v: &Value, path: &str) -> String {
    at(v, path).and_then(|x| x.as_str()).unwrap_or_default().to_owned()
}

pub(crate) fn opt_s(v: &Value, path: &str) -> Option<String> {
    at(v, path).and_then(|x| x.as_str()).filter(|x| !x.is_empty()).map(str::to_owned)
}

pub(crate) fn u(v: &Value, path: &str) -> u64 {
    at(v, path).and_then(|x| x.as_u64().or_else(|| x.as_str().and_then(|s| s.parse().ok()))).unwrap_or(0)
}

pub(crate) fn b(v: &Value, path: &str) -> bool {
    at(v, path).and_then(|x| x.as_bool()).unwrap_or(false)
}

/// The array at `path`, or an empty slice.
pub(crate) fn arr<'a>(v: &'a Value, path: &str) -> &'a [Value] {
    at(v, path).and_then(|x| x.as_array()).map(|a| &a[..]).unwrap_or(&[])
}

/// The elements of a top-level array.
pub(crate) fn items(v: &Value) -> &[Value] {
    v.as_array().map(|a| &a[..]).unwrap_or(&[])
}

pub(crate) fn time(v: &Value, path: &str) -> u64 {
    super::time::parse_ms(&s(v, path))
}

/// A string for JSON output, escaped.
pub(crate) fn quote(text: &str) -> String {
    sonic_rs::to_string(text).unwrap_or_else(|_| "\"\"".into())
}
