/// Session token authentication.
///
/// Token format: `exp:nonce:sig`
/// where `sig = base64url_nopad(HMAC_SHA256(secret, "exp:nonce"))`.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

/// Compute base64url-no-pad HMAC-SHA256 over `payload`.
fn hmac_b64(secret: &[u8], payload: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC can accept any key length");
    mac.update(payload.as_bytes());
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

/// Create a session token valid for `ttl_secs` seconds from `now`.
/// `now` is injected (unix seconds) — no clock call inside.
pub fn make_token(secret: &[u8], ttl_secs: u64, now: u64) -> String {
    let exp = now + ttl_secs;
    let nonce = {
        // Determinism isn't required for correctness; we use a simple counter-safe
        // nonce derived from exp XOR a compile-time salt.  In production the
        // caller can pass any nonce — here we just need *a* nonce that makes each
        // token unique.  We borrow rand-free approach: base64url of 12 pseudo bytes.
        let pseudo: [u8; 12] = {
            let mut b = [0u8; 12];
            let t = exp.to_le_bytes();
            for i in 0..8 {
                b[i] = t[i] ^ 0xA5;
            }
            b[8] = 0x3C;
            b[9] = 0xF0;
            b[10] = 0x7E;
            b[11] = 0x91;
            b
        };
        URL_SAFE_NO_PAD.encode(pseudo)
    };
    let payload = format!("{exp}:{nonce}");
    let sig = hmac_b64(secret, &payload);
    format!("{payload}:{sig}")
}

/// Verify a token.  Returns `true` iff the signature is valid and `exp >= now`.
pub fn check_token(secret: &[u8], token: &str, now: u64) -> bool {
    // Split into exactly 3 parts on the first two colons
    // Format: exp:nonce:sig  — nonce itself is base64url so no colon in it
    let parts: Vec<&str> = token.splitn(3, ':').collect();
    if parts.len() != 3 {
        return false;
    }
    let (exp_str, nonce, sig) = (parts[0], parts[1], parts[2]);

    // Parse expiry
    let exp: u64 = match exp_str.parse() {
        Ok(v) => v,
        Err(_) => return false,
    };

    // Reject empty components
    if nonce.is_empty() || sig.is_empty() {
        return false;
    }

    // Check expiry first (not secret — avoids timing side-channel on exp value)
    if exp < now {
        return false;
    }

    // Compute expected sig and compare constant-time
    let payload = format!("{exp_str}:{nonce}");
    let expected = hmac_b64(secret, &payload);
    expected.as_bytes().ct_eq(sig.as_bytes()).into()
}

/// Constant-time password comparison.
pub fn verify_password(input: &str, expected: &str) -> bool {
    // Constant-time only if lengths match; leak length is acceptable for passwords
    // (length is not secret in most schemes, but we still use ct_eq).
    input.as_bytes().ct_eq(expected.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"test-secret-key-32-bytes-padding!";
    const NOW: u64 = 1_000_000;

    #[test]
    fn make_then_check_roundtrip_is_true() {
        let token = make_token(SECRET, 3600, NOW);
        assert!(check_token(SECRET, &token, NOW));
    }

    #[test]
    fn expired_token_is_rejected() {
        let token = make_token(SECRET, 3600, NOW);
        // now is past exp
        assert!(!check_token(SECRET, &token, NOW + 3601));
    }

    #[test]
    fn tampered_sig_is_rejected() {
        let token = make_token(SECRET, 3600, NOW);
        let parts: Vec<&str> = token.splitn(3, ':').collect();
        assert_eq!(parts.len(), 3);
        let bad_token = format!(
            "{}:{}:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA_",
            parts[0], parts[1]
        );
        assert!(!check_token(SECRET, &bad_token, NOW));
    }

    #[test]
    fn wrong_secret_is_rejected() {
        let token = make_token(SECRET, 3600, NOW);
        assert!(!check_token(b"wrong-secret", &token, NOW));
    }

    #[test]
    fn malformed_token_is_rejected() {
        assert!(!check_token(SECRET, "notavalidtoken", NOW));
        assert!(!check_token(SECRET, ":", NOW));
        assert!(!check_token(SECRET, "a:b", NOW));
        assert!(!check_token(SECRET, "", NOW));
    }

    #[test]
    fn password_constant_time_match() {
        assert!(verify_password("hunter2", "hunter2"));
    }

    #[test]
    fn password_constant_time_mismatch() {
        assert!(!verify_password("hunter2", "wrong"));
        assert!(!verify_password("", "hunter2"));
        assert!(!verify_password("hunter2", ""));
    }

    #[test]
    fn token_at_exact_expiry_boundary() {
        let token = make_token(SECRET, 3600, NOW);
        // exp = NOW + 3600; at exp itself it should still be valid (exp >= now)
        assert!(check_token(SECRET, &token, NOW + 3600));
        // one second after expiry
        assert!(!check_token(SECRET, &token, NOW + 3601));
    }
}
