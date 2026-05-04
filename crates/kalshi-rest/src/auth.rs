//! Kalshi RSA-PSS signature generation.
//!
//! Per docs.kalshi.com, every authenticated request carries three headers:
//! - `KALSHI-ACCESS-KEY`: the key id (UUID-like)
//! - `KALSHI-ACCESS-TIMESTAMP`: unix epoch milliseconds
//! - `KALSHI-ACCESS-SIGNATURE`: base64(RSA-PSS-SHA256(timestamp + method + path))
//!
//! Path is the URL path from the API root (no query string, no host).
//! PSS uses MGF1-SHA256 with salt length = digest length (32 bytes) per the
//! Kalshi reference implementation; salt is random, so signatures are
//! non-deterministic — verify via the public key, not by string compare.

use crate::error::Error;
use base64::Engine as _;
use rsa::RsaPrivateKey;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::pss::SigningKey;
use rsa::signature::{RandomizedSigner, SignatureEncoding};
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};

/// Signs Kalshi requests with an RSA private key.
pub struct Signer {
    key_id: String,
    signing_key: SigningKey<Sha256>,
}

impl std::fmt::Debug for Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Signer")
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

impl Signer {
    /// Build a signer from PEM-encoded private key bytes (PKCS#1 or PKCS#8).
    pub fn from_pem(key_id: impl Into<String>, pem: &str) -> Result<Self, Error> {
        // Try PKCS#8 first (`-----BEGIN PRIVATE KEY-----`), fall back to PKCS#1
        // (`-----BEGIN RSA PRIVATE KEY-----`).
        let key = RsaPrivateKey::from_pkcs8_pem(pem)
            .or_else(|_| RsaPrivateKey::from_pkcs1_pem(pem))
            .map_err(|e| Error::Auth(format!("parse private key: {e}")))?;
        Ok(Self {
            key_id: key_id.into(),
            signing_key: SigningKey::<Sha256>::new(key),
        })
    }

    #[must_use]
    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    /// Produce `(timestamp_ms_string, signature_b64)` for `method` + `path`.
    /// `method` is upper-case (`GET`, `POST`, ...). `path` is `/trade-api/v2/...`.
    pub fn sign(&self, method: &str, path: &str) -> (String, String) {
        let ts = current_unix_ms();
        let sig = self.sign_with_ts(ts, method, path);
        (ts.to_string(), sig)
    }

    /// Sign with an explicit timestamp. Exposed for tests.
    pub fn sign_with_ts(&self, ts_ms: u128, method: &str, path: &str) -> String {
        let payload = format!("{ts_ms}{method}{path}");
        let mut rng = rand::thread_rng();
        let sig = self.signing_key.sign_with_rng(&mut rng, payload.as_bytes());
        base64::engine::general_purpose::STANDARD.encode(sig.to_bytes())
    }
}

fn current_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::RsaPublicKey;
    use rsa::pkcs8::EncodePrivateKey;
    use rsa::pss::{Signature, VerifyingKey};
    use rsa::signature::Verifier;

    fn gen_key() -> (String, RsaPublicKey) {
        let mut rng = rand::thread_rng();
        // 2048 keeps tests fast; production keys come from Kalshi at the size
        // they choose.
        let priv_key = RsaPrivateKey::new(&mut rng, 2048).unwrap();
        let pub_key = RsaPublicKey::from(&priv_key);
        let pem = priv_key.to_pkcs8_pem(rsa::pkcs8::LineEnding::LF).unwrap();
        (pem.to_string(), pub_key)
    }

    #[test]
    fn signature_verifies_with_public_key() {
        let (pem, pub_key) = gen_key();
        let signer = Signer::from_pem("test-key-id", &pem).unwrap();

        let ts = 1_700_000_000_000u128;
        let method = "GET";
        let path = "/trade-api/v2/markets";
        let sig_b64 = signer.sign_with_ts(ts, method, path);

        let sig_bytes = base64::engine::general_purpose::STANDARD
            .decode(&sig_b64)
            .unwrap();
        let signature = Signature::try_from(sig_bytes.as_slice()).unwrap();

        let verifier = VerifyingKey::<Sha256>::new(pub_key);
        let payload = format!("{ts}{method}{path}");
        verifier
            .verify(payload.as_bytes(), &signature)
            .expect("signature should verify");
    }

    #[test]
    fn pss_is_non_deterministic() {
        let (pem, _) = gen_key();
        let signer = Signer::from_pem("k", &pem).unwrap();
        let a = signer.sign_with_ts(1, "GET", "/x");
        let b = signer.sign_with_ts(1, "GET", "/x");
        // PSS salt is random → identical inputs produce different signatures.
        assert_ne!(a, b);
    }

    #[test]
    fn rejects_bad_pem() {
        assert!(Signer::from_pem("k", "not a pem").is_err());
    }
}
