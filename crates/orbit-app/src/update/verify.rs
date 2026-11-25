//! Is a downloaded update the release it claims to be?
//!
//! Two checks over one read of the file: its SHA-256 must equal the
//! manifest's, and its minisign signature (Ed25519 over the file's
//! BLAKE2b-512, the default since minisign 0.10) must verify with the public
//! key built into this binary. The signature's trusted comment, which the
//! signature also covers, must name this version and platform, so an older
//! signed bundle cannot be replayed as a newer one, nor one platform's as
//! another's.

use std::io::Read as _;
use std::path::Path;

use minisign_verify::{PublicKey, Signature};
use sha2::{Digest as _, Sha256};

/// The release signing key (`minisign.pub`'s second line). A placeholder:
/// until a real key is set here, this build never installs an update.
/// `docs/RELEASING.md` explains how to make one.
pub const RELEASE_PUBLIC_KEY: &str = "REPLACE-WITH-THE-ORBIT-RELEASE-PUBLIC-KEY";

/// The key updates must be signed with, if this build has one. Debug builds
/// accept `ORBIT_UPDATE_PUBLIC_KEY`, to test against a local fixture.
pub fn release_key() -> Option<PublicKey> {
    if cfg!(debug_assertions)
        && let Ok(key) = std::env::var("ORBIT_UPDATE_PUBLIC_KEY")
    {
        return PublicKey::from_base64(key.trim()).ok();
    }
    PublicKey::from_base64(RELEASE_PUBLIC_KEY).ok()
}

/// The trusted comment a release's signature must carry.
#[cfg(test)]
pub fn trusted_comment(version: &str, platform: &str) -> String {
    format!("orbit {version} {platform}")
}

/// Checks `path` against the manifest's checksum and signature.
pub fn verify_file(
    path: &Path,
    sha256: &str,
    signature: &str,
    version: &str,
    platform: &str,
    key: &PublicKey,
) -> Result<(), String> {
    let signature = Signature::decode(signature.trim()).map_err(|_| "The update’s signature is malformed.".to_owned())?;
    let comment: Vec<&str> = signature.trusted_comment().split_whitespace().take(3).collect();
    if comment != ["orbit", version, platform] {
        return Err(format!(
            "The update’s signature is for “{}”, not Orbit {version} for {platform}.",
            signature.trusted_comment()
        ));
    }
    let mut verifier = key.verify_stream(&signature).map_err(|e| match e {
        minisign_verify::Error::UnexpectedKeyId => "The update is signed with a key this build doesn’t trust.".to_owned(),
        minisign_verify::Error::UnsupportedLegacyMode => "The update uses a legacy signature; sign with minisign 0.10 or later.".to_owned(),
        other => format!("The update’s signature can’t be checked ({other})."),
    })?;
    let mut file = std::fs::File::open(path).map_err(|e| format!("Couldn’t read the download: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|e| format!("Couldn’t read the download: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        verifier.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    if !hex.eq_ignore_ascii_case(sha256.trim()) {
        return Err("The download doesn’t match its checksum; it may be incomplete or altered.".to_owned());
    }
    verifier.finalize().map_err(|_| "The update’s signature doesn’t match; it was not installed.".to_owned())
}

/// Lowercase hex SHA-256 of a file (tests and the fixture tools).
#[cfg(test)]
pub fn sha256_file(path: &Path) -> String {
    let bytes = std::fs::read(path).unwrap();
    Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// A minisign key pair and signer for tests: the same format `minisign -S`
/// writes, built with ring (Ed25519) and blake2 (the prehash).
#[cfg(test)]
pub mod testing {
    use base64_engine::encode;
    use blake2::Digest as _;
    use ring::signature::{Ed25519KeyPair, KeyPair as _};

    pub struct TestKey {
        pair: Ed25519KeyPair,
        key_id: [u8; 8],
    }

    impl TestKey {
        pub fn new(seed: u8) -> Self {
            let pair = Ed25519KeyPair::from_seed_unchecked(&[seed; 32]).unwrap();
            Self { pair, key_id: [seed, 1, 2, 3, 4, 5, 6, 7] }
        }

        /// The public key as `minisign.pub`'s second line.
        pub fn public_base64(&self) -> String {
            let mut bin = b"Ed".to_vec();
            bin.extend_from_slice(&self.key_id);
            bin.extend_from_slice(self.pair.public_key().as_ref());
            encode(&bin)
        }

        /// A prehashed minisign signature of `data` with `trusted` as its
        /// trusted comment.
        pub fn sign(&self, data: &[u8], trusted: &str) -> String {
            let hash = blake2::Blake2b512::digest(data);
            let sig = self.pair.sign(&hash);
            let mut bin = b"ED".to_vec();
            bin.extend_from_slice(&self.key_id);
            bin.extend_from_slice(sig.as_ref());
            let mut global = sig.as_ref().to_vec();
            global.extend_from_slice(trusted.as_bytes());
            let global = self.pair.sign(&global);
            format!(
                "untrusted comment: signature from a test key\n{}\ntrusted comment: {trusted}\n{}\n",
                encode(&bin),
                encode(global.as_ref())
            )
        }
    }

    /// Standard base64 without a crate (tests only).
    mod base64_engine {
        const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

        pub fn encode(bytes: &[u8]) -> String {
            let mut out = String::new();
            for chunk in bytes.chunks(3) {
                let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
                let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
                for i in 0..4 {
                    if i <= chunk.len() {
                        out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
                    } else {
                        out.push('=');
                    }
                }
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::TestKey;
    use super::*;

    fn fixture(data: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bundle.zip");
        std::fs::write(&path, data).unwrap();
        (dir, path)
    }

    #[test]
    fn a_signed_release_verifies() {
        let key = TestKey::new(7);
        let public = PublicKey::from_base64(&key.public_base64()).unwrap();
        let (_dir, path) = fixture(b"orbit bundle bytes");
        let sig = key.sign(b"orbit bundle bytes", &trusted_comment("0.2.0", "x86_64-pc-windows-msvc"));
        let sha = sha256_file(&path);
        verify_file(&path, &sha, &sig, "0.2.0", "x86_64-pc-windows-msvc", &public).unwrap();
    }

    #[test]
    fn tampering_is_refused() {
        let key = TestKey::new(7);
        let public = PublicKey::from_base64(&key.public_base64()).unwrap();
        let (_dir, path) = fixture(b"orbit bundle bytes");
        let sig = key.sign(b"orbit bundle bytes", &trusted_comment("0.2.0", "x86_64-pc-windows-msvc"));
        let sha = sha256_file(&path);
        // Altered bytes: the checksum catches it.
        std::fs::write(&path, b"orbit bundle bytez").unwrap();
        assert!(verify_file(&path, &sha, &sig, "0.2.0", "x86_64-pc-windows-msvc", &public).unwrap_err().contains("checksum"));
        // Altered bytes with a matching checksum: the signature catches it.
        let sha = sha256_file(&path);
        assert!(verify_file(&path, &sha, &sig, "0.2.0", "x86_64-pc-windows-msvc", &public).unwrap_err().contains("signature"));
    }

    #[test]
    fn replays_and_foreign_keys_are_refused() {
        let key = TestKey::new(7);
        let public = PublicKey::from_base64(&key.public_base64()).unwrap();
        let (_dir, path) = fixture(b"old bundle");
        let sha = sha256_file(&path);
        // An old release's signature offered as a newer version.
        let old = key.sign(b"old bundle", &trusted_comment("0.1.0", "x86_64-pc-windows-msvc"));
        assert!(verify_file(&path, &sha, &old, "0.2.0", "x86_64-pc-windows-msvc", &public).is_err());
        // Another platform's signature.
        let mac = key.sign(b"old bundle", &trusted_comment("0.2.0", "aarch64-apple-darwin"));
        assert!(verify_file(&path, &sha, &mac, "0.2.0", "x86_64-pc-windows-msvc", &public).is_err());
        // A key this build doesn't trust.
        let other = TestKey::new(9).sign(b"old bundle", &trusted_comment("0.2.0", "x86_64-pc-windows-msvc"));
        let err = verify_file(&path, &sha, &other, "0.2.0", "x86_64-pc-windows-msvc", &public).unwrap_err();
        assert!(err.contains("doesn’t trust"), "{err}");
        // Garbage.
        assert!(verify_file(&path, &sha, "nonsense", "0.2.0", "x86_64-pc-windows-msvc", &public).is_err());
    }

    #[test]
    fn the_placeholder_key_disables_updates() {
        assert!(PublicKey::from_base64(RELEASE_PUBLIC_KEY).is_err());
    }
}
