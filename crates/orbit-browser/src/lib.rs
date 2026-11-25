//! Orbit's preview browser.
//!
//! This first step holds the pure address handling: what the address field
//! accepts and the dev-server URLs found in terminal or agent output.

mod address;

pub use address::{AddressError, detect_urls, host_label, is_loopback, parse_address, port_url};
pub use url::Url;
