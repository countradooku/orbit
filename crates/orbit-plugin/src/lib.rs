//! Orbit plugins: everything about a plugin that is not drawing it.
//!
//! A plugin is a folder with an `orbit-plugin.toml` manifest
//! ([`manifest`]) and the files it names. In order of trust it contributes:
//!
//! 1. **Data**: app and terminal themes ([`theme`]), snippets, slash
//!    commands, automation recipes, keybindings and project templates.
//! 2. **Agent manifests** ([`agent`]): an agent CLI described as data that
//!    Orbit runs over ACP, Claude Code's stream-json, or in a terminal.
//! 3. **Declarative panels** ([`panel`]): component trees bound to the
//!    plugin's storage, whose actions call the host API.
//! 4. **WebAssembly** (`wasm`, host only): hooks, commands and panel actions
//!    in a sandbox with no ambient authority, fuel and memory limits.
//!
//! The host keeps installed plugins in `<data>/plugins/` (`host`, behind the
//! `host` feature): installing from a folder, a `.orbitplugin` archive
//! ([`package`]), a git commit or a signed catalog ([`catalog`]), checking
//! content hashes ([`hash`]), signatures ([`sign`]) and the kill list
//! ([`killlist`]), asking consent for capabilities and recording every
//! host API call in an audit log.

pub mod agent;
pub mod catalog;
pub mod hash;
pub mod killlist;
pub mod manifest;
pub mod package;
pub mod panel;
pub mod sign;
pub mod template;
pub mod theme;

#[cfg(feature = "host")]
pub mod host;
#[cfg(feature = "host")]
pub mod secrets;
#[cfg(feature = "host")]
pub mod wasm;

pub use manifest::{MANIFEST, Manifest, Validated};
pub use package::Loaded;

/// The version plugins' `engines.orbit` requirement is checked against.
pub const ORBIT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Milliseconds since the Unix epoch.
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}
