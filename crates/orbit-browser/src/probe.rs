//! Whether anything listens at a local address, asked with a TCP connect.
//!
//! A refused connection to this machine is answered by the kernel, so a
//! dev server that is up accepts at once; one that is down is reported in
//! well under a second, before a browser engine would give up and draw its
//! own error page.

use std::{
    net::{SocketAddr, TcpStream, ToSocketAddrs as _},
    time::Duration,
};

use url::Url;

/// How long a connect may take before the port counts as closed.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(400);

/// The socket addresses `url` names, as `localhost` resolves (often both
/// `::1` and `127.0.0.1`, and a dev server may listen on only one of them).
pub(crate) fn addresses(url: &Url) -> Vec<SocketAddr> {
    let Some(host) = url.host_str() else { return Vec::new() };
    let Some(port) = url.port_or_known_default() else { return Vec::new() };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    (host, port).to_socket_addrs().map(Iterator::collect).unwrap_or_default()
}

/// Whether a server accepts connections at any of `addresses`. Blocking:
/// run it on a background executor.
pub(crate) fn is_listening(addresses: &[SocketAddr]) -> bool {
    addresses.iter().any(|address| TcpStream::connect_timeout(address, CONNECT_TIMEOUT).is_ok())
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;

    #[test]
    fn a_listening_port_answers_and_a_closed_one_does_not() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
        assert!(is_listening(&addresses(&url)));

        drop(listener);
        assert!(!is_listening(&addresses(&url)));
    }

    #[test]
    fn localhost_resolves() {
        let url = Url::parse("http://localhost:3000/").unwrap();
        assert!(addresses(&url).iter().all(|address| address.ip().is_loopback() && address.port() == 3000));
        assert!(!addresses(&url).is_empty());
    }
}
