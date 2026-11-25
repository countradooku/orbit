//! Signatures on catalogs and the kill list: minisign, as for updates.
//!
//! The same verifier (`minisign-verify`), key format (`minisign.pub`'s
//! second line) and discipline as the updater (`orbit-app`'s
//! `update::verify`): the signature's trusted comment, which the signature
//! also covers, must name what is signed (`orbit-catalog …`,
//! `orbit-kill-list …`), so one signed document cannot be passed off as
//! another.
//!
//! The kill list and Orbit's own catalog are signed with the release key
//! ([`RELEASE_PUBLIC_KEY`], shared with the updater). A catalog someone
//! else publishes is checked against the key the user added it with.

use minisign_verify::{PublicKey, Signature};

/// The release signing key (`minisign.pub`'s second line), for updates,
/// the kill list and Orbit's catalog. A placeholder: until a real key is
/// set here, nothing signed with "the Orbit key" verifies. See
/// `docs/RELEASING.md`.
pub const RELEASE_PUBLIC_KEY: &str = "REPLACE-WITH-THE-ORBIT-RELEASE-PUBLIC-KEY";

/// The Orbit key, if this build has one. Debug builds accept
/// `ORBIT_PLUGIN_PUBLIC_KEY`, to test against local fixtures.
pub fn release_key() -> Option<String> {
    if cfg!(debug_assertions)
        && let Ok(key) = std::env::var("ORBIT_PLUGIN_PUBLIC_KEY")
    {
        return Some(key.trim().to_owned());
    }
    PublicKey::from_base64(RELEASE_PUBLIC_KEY).ok().map(|_| RELEASE_PUBLIC_KEY.to_owned())
}

/// Whether `key` is a minisign public key.
pub fn is_public_key(key: &str) -> bool {
    PublicKey::from_base64(key.trim()).is_ok()
}

/// Verifies `signature` (a whole `.minisig` file) over `data` with `key`.
/// The trusted comment must start with `what` followed by a space or end;
/// returns the trusted comment.
pub fn verify(data: &[u8], signature: &str, key: &str, what: &str) -> Result<String, String> {
    let key = PublicKey::from_base64(key.trim()).map_err(|_| "The public key isn’t a minisign key.".to_owned())?;
    let signature = Signature::decode(signature.trim()).map_err(|_| "The signature is malformed.".to_owned())?;
    let comment = signature.trusted_comment().to_owned();
    if comment.split_whitespace().next() != Some(what) {
        return Err(format!("The signature is for “{comment}”, not an {what}."));
    }
    key.verify(data, &signature, false).map_err(|e| match e {
        minisign_verify::Error::UnexpectedKeyId => "It is signed with a key Orbit doesn’t trust for it.".to_owned(),
        minisign_verify::Error::UnsupportedLegacyMode | minisign_verify::Error::UnexpectedAlgorithm => "It uses a legacy signature; sign with minisign 0.10 or later.".to_owned(),
        _ => "The signature doesn’t match; it may have been altered.".to_owned(),
    })?;
    Ok(comment)
}

/// A minisign key pair and signer for tests and fixtures: the format
/// `minisign -S` writes, built with ring (Ed25519) and blake2 (prehash).
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use blake2::Digest as _;
    use ring::signature::{Ed25519KeyPair, KeyPair as _};

    pub struct TestKey {
        pair: Ed25519KeyPair,
        key_id: [u8; 8],
    }

    impl TestKey {
        pub fn new(seed: u8) -> Self {
            let pair = Ed25519KeyPair::from_seed_unchecked(&[seed; 32]).expect("seed");
            Self { pair, key_id: [seed, 9, 8, 7, 6, 5, 4, 3] }
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

    /// Standard base64.
    fn encode(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
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

#[cfg(test)]
mod tests {
    use super::testing::TestKey;
    use super::*;

    #[test]
    fn signed_documents_verify_and_tampering_does_not() {
        let key = TestKey::new(3);
        let data = b"[[plugins]]\nid = \"x\"\n";
        let sig = key.sign(data, "orbit-catalog examples 2026-11-25");
        assert_eq!(verify(data, &sig, &key.public_base64(), "orbit-catalog").unwrap(), "orbit-catalog examples 2026-11-25");
        assert!(verify(b"[[plugins]]\nid = \"y\"\n", &sig, &key.public_base64(), "orbit-catalog").unwrap_err().contains("doesn’t match"));
        // A catalog signature presented as a kill list.
        assert!(verify(data, &sig, &key.public_base64(), "orbit-kill-list").unwrap_err().contains("not an orbit-kill-list"));
        // Another key.
        assert!(verify(data, &sig, &TestKey::new(4).public_base64(), "orbit-catalog").unwrap_err().contains("trust"));
        assert!(verify(data, "garbage", &key.public_base64(), "orbit-catalog").is_err());
        assert!(!is_public_key("nope"));
        assert!(is_public_key(&key.public_base64()));
    }

    #[test]
    fn the_placeholder_key_trusts_nothing() {
        assert!(!is_public_key(RELEASE_PUBLIC_KEY));
    }
}
