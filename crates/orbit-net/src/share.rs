//! Sharing this computer: its background host listens on the network for
//! devices paired with a short code (see `docs/REMOTE.md`, "Share this
//! computer", for the threat model).
//!
//! * Off by default. Turned on from Settings › Hosts, which picks an
//!   address: Tailscale's when there is one (WireGuard encrypts the
//!   traffic), else the local network's.
//! * **Pairing.** The host shows a code (`ABCD-EFGH`, 40 bits, ten
//!   minutes, five wrong tries). The other Orbit connects with it and a
//!   random nonce; the host answers with its own nonce, and both screens
//!   show the same six digits derived from the code and both nonces. The
//!   person at the host checks the digits match and allows the device; only
//!   then does the device get its own token (128 random bits). The host
//!   keeps only the token's SHA-256.
//! * **Devices** are listed with when they were last seen and revoked one
//!   by one; revoking closes their connections at once.
//! * The listener speaks the same protocol as the loopback endpoint, plain
//!   WebSocket: on a network that does not encrypt (anything but Tailscale
//!   or a VPN), traffic can be read and the token reused by someone on the
//!   path. SSH hosts don't have this problem.
//!
//! The pairing exchange is two JSON frames on a fresh connection: the
//! client sends a control request with `op: "pair"`; the host answers
//! `{"verify","nonce"}` at once and `{"token","device"}` (or `{"error"}`)
//! once the user decided.

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The file in the host's data folder: whether sharing is on, and the
/// paired devices (token digests only).
pub const FILE: &str = "share.json";
/// How long a pairing code works.
pub const CODE_TTL_MS: u64 = 10 * 60 * 1000;
/// Wrong codes before the current one stops working.
pub const CODE_TRIES: u32 = 5;
/// How long a device waits for the user at the host to decide.
pub const DECIDE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// Letters that read unambiguously (no 0/O, 1/I/L).
const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShareFile {
    pub enabled: bool,
    /// `ip:port`.
    pub listen: String,
    pub devices: Vec<DeviceRecord>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceRecord {
    pub id: String,
    pub name: String,
    /// Hex SHA-256 of the device's token.
    pub token_sha256: String,
    pub paired_ms: u64,
    pub last_seen_ms: u64,
}

impl ShareFile {
    pub fn load(data_dir: &Path) -> Self {
        std::fs::read_to_string(data_dir.join(FILE)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    /// Saves owner-only.
    pub fn save(&self, data_dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(data_dir)?;
        let text = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        crate::endpoint::write_private(&data_dir.join(FILE), &text)
    }

    /// The device a token belongs to.
    pub fn device_for(&self, token: &str) -> Option<&DeviceRecord> {
        let digest = sha256_hex(token.as_bytes());
        self.devices.iter().find(|d| constant_eq(&d.token_sha256, &digest))
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

fn constant_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `n` random bytes, hex.
pub fn random_hex(n: usize) -> io::Result<String> {
    let mut bytes = vec![0u8; n];
    getrandom::fill(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// A fresh pairing code, `ABCD-EFGH`.
pub fn new_code() -> io::Result<String> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
    let letters: String = bytes.iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect();
    Ok(format!("{}-{}", &letters[..4], &letters[4..]))
}

/// A code as typed, for comparing: upper case, no dashes or spaces.
pub fn normalize_code(code: &str) -> String {
    code.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect()
}

/// Whether what the user typed is a pairing code rather than a token.
pub fn looks_like_code(text: &str) -> bool {
    let code = normalize_code(text);
    code.len() == 8 && code.bytes().all(|b| ALPHABET.contains(&b)) && text.trim().len() <= 10
}

/// The six digits both screens show for one exchange.
pub fn verify_digits(code: &str, client_nonce: &str, host_nonce: &str) -> String {
    let digest = Sha256::digest(format!("orbit-pair\n{}\n{client_nonce}\n{host_nonce}", normalize_code(code)).as_bytes());
    let n = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]) % 1_000_000;
    format!("{:03} {:03}", n / 1000, n % 1000)
}

/// The host's first answer to a pairing request.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PairChallenge {
    pub verify: String,
    pub nonce: String,
    pub error: String,
}

/// The host's final answer.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PairResult {
    pub token: String,
    pub device: String,
    pub error: String,
}

/// Pairs with the host at `url` (`ws://ip:port/`) using `code`, naming
/// this device `device`. `on_verify` gets the digits to show while the
/// user at the host decides. Returns the device token. Blocking.
pub fn pair(url: &str, code: &str, device: &str, on_verify: impl FnOnce(String)) -> Result<String, String> {
    let nonce = random_hex(16).map_err(|e| e.to_string())?;
    let request = crate::host::ControlRequest {
        token: String::new(),
        op: "pair".into(),
        when: String::new(),
        code: normalize_code(code),
        device: device.trim().chars().take(64).collect(),
        nonce: nonce.clone(),
    };
    let body = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    runtime.block_on(async move {
        let mut link = crate::client::open_raw(url).await?;
        link.send(&body).await?;
        let first = tokio::time::timeout(std::time::Duration::from_secs(15), link.next())
            .await
            .map_err(|_| "The host didn’t answer.".to_owned())??
            .ok_or("The host closed the connection.")?;
        let challenge: PairChallenge = serde_json::from_slice(&first).map_err(|_| "The host doesn’t pair this way; update Orbit there.".to_owned())?;
        if !challenge.error.is_empty() {
            return Err(challenge.error);
        }
        let ours = verify_digits(code, &nonce, &challenge.nonce);
        if ours != challenge.verify {
            return Err("The host’s confirmation number doesn’t match this one; pairing stopped.".into());
        }
        on_verify(ours);
        let second = tokio::time::timeout(DECIDE_TIMEOUT + std::time::Duration::from_secs(10), link.next())
            .await
            .map_err(|_| "No one answered on the host.".to_owned())??
            .ok_or("The host closed the connection.")?;
        let result: PairResult = serde_json::from_slice(&second).map_err(|e| e.to_string())?;
        link.close().await;
        if !result.error.is_empty() {
            return Err(result.error);
        }
        Ok(result.token)
    })
}

/// This computer's name, as a paired device calls itself.
pub fn device_name() -> String {
    let from_env = std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).ok();
    let from_file = || std::fs::read_to_string("/etc/hostname").ok();
    from_env.or_else(from_file).map(|n| n.trim().to_owned()).filter(|n| !n.is_empty()).unwrap_or_else(|| "Orbit".into())
}

/// This computer's addresses other devices may reach, the most private
/// first: Tailscale's (100.64.0.0/10), then the local network's. Found
/// from the routes the system would use (no packet is sent).
pub fn addresses() -> Vec<orbit_proto::ShareAddress> {
    fn route_to(target: &str) -> Option<std::net::IpAddr> {
        let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
        socket.connect(target).ok()?;
        Some(socket.local_addr().ok()?.ip())
    }
    let tailscale = |ip: &std::net::IpAddr| match ip {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 100 && (64..128).contains(&o[1])
        }
        _ => false,
    };
    let mut out = Vec::new();
    if let Some(ip) = route_to("100.100.100.100:53").filter(tailscale) {
        out.push(orbit_proto::ShareAddress { ip: ip.to_string(), label: "Tailscale".into(), encrypted: true });
    }
    if let Some(ip) = route_to("192.0.2.1:53").or_else(|| route_to("8.8.8.8:53")).filter(|ip| !ip.is_loopback() && !tailscale(ip)) {
        out.push(orbit_proto::ShareAddress { ip: ip.to_string(), label: "Local network".into(), encrypted: false });
    }
    out
}

#[cfg(feature = "engine")]
pub use serve::Share;

#[cfg(feature = "engine")]
mod serve {
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use orbit_core::EngineHandle;
    use orbit_proto::{PairRequest, PairedDevice, PairingCode, ShareCommand, ShareStatus, now_ms};

    use super::*;
    use crate::host::HostControl;

    /// The host's side of sharing: the listener, the code, requests waiting
    /// for the user, and the paired devices.
    pub struct Share {
        data_dir: PathBuf,
        engine: EngineHandle,
        control: Arc<HostControl>,
        inner: Mutex<Inner>,
    }

    struct Inner {
        file: ShareFile,
        code: Option<(String, u64, u32)>,
        pending: Vec<(PairRequest, tokio::sync::oneshot::Sender<bool>)>,
        next_request: u64,
        listener: Option<tokio::sync::oneshot::Sender<()>>,
        error: Option<String>,
        /// Live connections per device, closed on revoke.
        connected: HashMap<String, (usize, tokio::sync::watch::Sender<bool>)>,
    }

    /// A paired device's connection; dropping it marks the device gone.
    pub struct DeviceGuard {
        share: Arc<Share>,
        device: String,
        /// Becomes `true` (or closes) when the device is revoked.
        pub revoked: tokio::sync::watch::Receiver<bool>,
    }

    impl Drop for DeviceGuard {
        fn drop(&mut self) {
            let mut inner = self.share.lock();
            if let Some((count, _)) = inner.connected.get_mut(&self.device) {
                *count -= 1;
                if *count == 0 {
                    inner.connected.remove(&self.device);
                }
            }
        }
    }

    impl Share {
        /// Reads `share.json` and listens when sharing is on.
        pub fn start(engine: EngineHandle, control: Arc<HostControl>, data_dir: &std::path::Path) -> Arc<Self> {
            let file = ShareFile::load(data_dir);
            let enabled = file.enabled;
            let share = Arc::new(Self {
                data_dir: data_dir.to_owned(),
                engine,
                control,
                inner: Mutex::new(Inner {
                    file,
                    code: None,
                    pending: Vec::new(),
                    next_request: 1,
                    listener: None,
                    error: None,
                    connected: HashMap::new(),
                }),
            });
            if enabled {
                share.listen();
            }
            share
        }

        fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
            self.inner.lock().unwrap_or_else(|e| e.into_inner())
        }

        fn save(&self, inner: &Inner) {
            if let Err(err) = inner.file.save(&self.data_dir) {
                log::warn!("couldn’t save {FILE}: {err}");
            }
        }

        pub fn status(&self) -> ShareStatus {
            let mut inner = self.lock();
            let now = now_ms();
            if inner.code.as_ref().is_some_and(|(_, expires, tries)| *expires <= now || *tries >= CODE_TRIES) {
                inner.code = None;
            }
            ShareStatus {
                enabled: inner.file.enabled,
                listen: inner.file.listen.clone(),
                error: inner.error.clone(),
                addresses: addresses(),
                code: inner.code.as_ref().map(|(code, expires, _)| PairingCode { code: code.clone(), expires_ms: *expires }),
                pending: inner.pending.iter().map(|(r, _)| r.clone()).collect(),
                devices: inner
                    .file
                    .devices
                    .iter()
                    .map(|d| PairedDevice {
                        id: d.id.clone(),
                        name: d.name.clone(),
                        paired_ms: d.paired_ms,
                        last_seen_ms: d.last_seen_ms,
                        connected: inner.connected.contains_key(&d.id),
                    })
                    .collect(),
            }
        }

        pub fn command(self: &Arc<Self>, command: ShareCommand) -> Result<ShareStatus, String> {
            match command {
                ShareCommand::Status => {}
                ShareCommand::Enable { listen } => {
                    let addr: SocketAddr = listen.trim().parse().map_err(|_| format!("“{listen}” is not an address and port."))?;
                    {
                        let mut inner = self.lock();
                        inner.file.enabled = true;
                        inner.file.listen = addr.to_string();
                        self.save(&inner);
                    }
                    self.listen();
                }
                ShareCommand::Disable => {
                    let mut inner = self.lock();
                    inner.file.enabled = false;
                    inner.code = None;
                    for (_, decide) in inner.pending.drain(..) {
                        let _ = decide.send(false);
                    }
                    if let Some(stop) = inner.listener.take() {
                        let _ = stop.send(());
                    }
                    inner.error = None;
                    self.save(&inner);
                    drop(inner);
                    self.control.set_sharing(false);
                }
                ShareCommand::NewCode => {
                    let code = new_code().map_err(|e| e.to_string())?;
                    self.lock().code = Some((code, now_ms() + CODE_TTL_MS, 0));
                }
                ShareCommand::Confirm { request, accept } => {
                    let mut inner = self.lock();
                    let ix = inner.pending.iter().position(|(r, _)| r.id == request).ok_or("That request is gone.")?;
                    let (_, decide) = inner.pending.remove(ix);
                    let _ = decide.send(accept);
                }
                ShareCommand::Revoke { device } => {
                    let mut inner = self.lock();
                    inner.file.devices.retain(|d| d.id != device);
                    if let Some((_, revoked)) = inner.connected.remove(&device) {
                        let _ = revoked.send(true);
                    }
                    self.save(&inner);
                }
            }
            Ok(self.status())
        }

        /// (Re)starts the listener on the saved address.
        fn listen(self: &Arc<Self>) {
            let addr = {
                let mut inner = self.lock();
                if let Some(stop) = inner.listener.take() {
                    let _ = stop.send(());
                }
                inner.file.listen.parse::<SocketAddr>()
            };
            let addr = match addr {
                Ok(addr) => addr,
                Err(_) => {
                    self.lock().error = Some("No address to listen on.".into());
                    return;
                }
            };
            let listener = match std::net::TcpListener::bind(addr).and_then(|l| l.set_nonblocking(true).map(|()| l)) {
                Ok(listener) => listener,
                Err(err) => {
                    self.lock().error = Some(format!("Couldn’t listen on {addr}: {err}"));
                    return;
                }
            };
            let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
            {
                let mut inner = self.lock();
                inner.listener = Some(stop_tx);
                inner.error = None;
            }
            self.control.set_sharing(true);
            let share = self.clone();
            let spawned = std::thread::Builder::new().name("orbit-share".into()).spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(runtime) => runtime,
                    Err(err) => {
                        share.lock().error = Some(err.to_string());
                        return;
                    }
                };
                runtime.block_on(async move {
                    let Ok(listener) = tokio::net::TcpListener::from_std(listener) else { return };
                    log::info!("sharing this computer on {addr}");
                    let engine = share.engine.clone();
                    let control = share.control.clone();
                    tokio::select! {
                        result = crate::server::serve_shared(engine, listener, share.clone(), control) => {
                            if let Err(err) = result {
                                share.lock().error = Some(err.to_string());
                            }
                        }
                        _ = stop_rx => log::info!("stopped sharing this computer"),
                    }
                });
            });
            if let Err(err) = spawned {
                self.lock().error = Some(err.to_string());
            }
        }

        /// A paired device's token: its id, and a guard for the
        /// connection.
        pub fn admit(self: &Arc<Self>, token: &str) -> Option<DeviceGuard> {
            let mut inner = self.lock();
            let id = inner.file.device_for(token)?.id.clone();
            if let Some(device) = inner.file.devices.iter_mut().find(|d| d.id == id) {
                device.last_seen_ms = now_ms();
            }
            self.save(&inner);
            let entry = inner.connected.entry(id.clone()).or_insert_with(|| (0, tokio::sync::watch::channel(false).0));
            entry.0 += 1;
            let revoked = entry.1.subscribe();
            Some(DeviceGuard { share: self.clone(), device: id, revoked })
        }

        /// A pairing request: checks the code and waits for the user.
        /// Returns the challenge to send now and the decision to await.
        pub fn begin_pair(
            &self,
            code: &str,
            device: &str,
            client_nonce: &str,
            from: String,
        ) -> Result<(PairChallenge, u64, tokio::sync::oneshot::Receiver<bool>), String> {
            let mut inner = self.lock();
            let now = now_ms();
            let Some((current, expires, tries)) = inner.code.as_mut() else {
                return Err("This computer isn’t pairing now. Show a new code on it and try again.".into());
            };
            if *expires <= now || *tries >= CODE_TRIES {
                inner.code = None;
                return Err("That code has expired. Show a new code on the other computer.".into());
            }
            if normalize_code(code) != normalize_code(current) {
                *tries += 1;
                return Err("That code is wrong.".into());
            }
            let code = current.clone();
            // One device per code.
            inner.code = None;
            let host_nonce = random_hex(16).map_err(|e| e.to_string())?;
            let verify = verify_digits(&code, client_nonce, &host_nonce);
            let id = inner.next_request;
            inner.next_request += 1;
            let (tx, rx) = tokio::sync::oneshot::channel();
            let name = if device.trim().is_empty() { "Unnamed device".to_owned() } else { device.trim().to_owned() };
            inner.pending.push((PairRequest { id, device: name, verify: verify.clone(), from, at_ms: now }, tx));
            Ok((PairChallenge { verify, nonce: host_nonce, error: String::new() }, id, rx))
        }

        /// The user allowed request `id`: a token for the device.
        pub fn finish_pair(&self, id: u64, device: &str) -> Result<PairResult, String> {
            let token = random_hex(16).map_err(|e| e.to_string())?;
            let device_id = random_hex(6).map_err(|e| e.to_string())?;
            let mut inner = self.lock();
            inner.pending.retain(|(r, _)| r.id != id);
            let now = now_ms();
            inner.file.devices.push(DeviceRecord {
                id: device_id.clone(),
                name: if device.trim().is_empty() { "Unnamed device".into() } else { device.trim().to_owned() },
                token_sha256: sha256_hex(token.as_bytes()),
                paired_ms: now,
                last_seen_ms: now,
            });
            self.save(&inner);
            Ok(PairResult { token, device: device_id, error: String::new() })
        }

        /// A request that timed out or whose connection closed.
        pub fn drop_request(&self, id: u64) {
            self.lock().pending.retain(|(r, _)| r.id != id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_read_well_and_compare_loosely() {
        let code = new_code().unwrap();
        assert_eq!(code.len(), 9);
        assert!(looks_like_code(&code));
        assert!(looks_like_code(&code.to_lowercase().replace('-', " ")));
        assert_eq!(normalize_code(" abcd-efgh "), "ABCDEFGH");
        assert!(!looks_like_code("0123456789abcdef0123456789abcdef"), "a token is not a code");
        assert!(!looks_like_code("ABCD-EFG0"), "0 is not in the alphabet");
    }

    #[test]
    fn both_sides_derive_the_same_digits() {
        let a = verify_digits("ABCD-EFGH", "c1", "h1");
        assert_eq!(a, verify_digits("abcdefgh", "c1", "h1"));
        assert_ne!(a, verify_digits("ABCD-EFGH", "c2", "h1"));
        assert_eq!(a.len(), 7);
    }

    #[test]
    fn tokens_are_kept_as_digests() {
        let dir = tempfile::tempdir().unwrap();
        let mut file = ShareFile { enabled: true, listen: "127.0.0.1:7840".into(), devices: vec![] };
        file.devices.push(DeviceRecord { id: "d1".into(), name: "Laptop".into(), token_sha256: sha256_hex(b"secret"), ..Default::default() });
        file.save(dir.path()).unwrap();
        let text = std::fs::read_to_string(dir.path().join(FILE)).unwrap();
        assert!(!text.contains("\"secret\""));
        let back = ShareFile::load(dir.path());
        assert_eq!(back.device_for("secret").map(|d| d.name.as_str()), Some("Laptop"));
        assert!(back.device_for("wrong").is_none());
    }
}
