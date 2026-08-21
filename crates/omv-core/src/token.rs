use anyhow::{bail, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Playback token: HMAC-signed, short-lived, scoped to one study's storage
/// prefix. One token covers every manifest/segment under that prefix, which
/// is what HLS needs (a single playback touches hundreds of objects).
#[derive(Debug, Serialize, Deserialize)]
pub struct PlaybackClaims {
    /// Storage prefix the bearer may read, e.g. "studies/1.2.840...".
    pub prefix: String,
    /// Unix expiry timestamp (seconds).
    pub exp: i64,
}

pub fn sign(claims: &PlaybackClaims, secret: &str) -> String {
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).expect("serialize claims"));
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(payload.as_bytes());
    let sig = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    format!("{payload}.{sig}")
}

pub fn verify(token: &str, secret: &str, now_unix: i64) -> Result<PlaybackClaims> {
    let Some((payload, sig)) = token.split_once('.') else {
        bail!("malformed token");
    };
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(payload.as_bytes());
    let expected = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    // Constant-time comparison to avoid signature-guessing via timing.
    if !constant_time_eq(expected.as_bytes(), sig.as_bytes()) {
        bail!("bad token signature");
    }
    let claims: PlaybackClaims = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload)?)?;
    if claims.exp < now_unix {
        bail!("token expired");
    }
    Ok(claims)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_expiry() {
        let claims = PlaybackClaims { prefix: "studies/1.2.3".into(), exp: 1_000 };
        let t = sign(&claims, "secret");
        let ok = verify(&t, "secret", 999).unwrap();
        assert_eq!(ok.prefix, "studies/1.2.3");
        assert!(verify(&t, "secret", 1_001).is_err(), "expired");
        assert!(verify(&t, "other", 999).is_err(), "wrong secret");
    }
}
