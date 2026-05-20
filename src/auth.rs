//! Pre-shared-key authentication for the wire protocol and HTTP gateway.
//!
//! Every accepted ring TCP connection's first line must be:
//!
//! ```text
//! AUTH <hmac_hex> <nonce_hex>
//! ```
//!
//! where `nonce` is 16 random bytes and `hmac = HMAC_SHA256(secret, nonce)`.
//! HMAC is inlined here (no extra dep) following RFC 2104:
//!   ipad = 0x36 repeated for the block length
//!   opad = 0x5c repeated for the block length
//!   HMAC(K, m) = H((K ⊕ opad) || H((K ⊕ ipad) || m))
//!
//! Tokens can be **disabled** (`AuthToken::disabled()`) to support tests that
//! pre-date the auth requirement; in disabled mode the server skips the
//! handshake and outbound calls don't send one. Production always configures
//! a real token via `--auth-token` or `OUROBOROS_AUTH_TOKEN`.

use rand::RngCore;
use sha2::{Digest, Sha256};

/// HMAC-SHA256 block size.
const BLOCK_SIZE: usize = 64;
const NONCE_LEN: usize = 16;
const HMAC_LEN: usize = 32;

#[derive(Clone)]
pub struct AuthToken {
    secret: Option<[u8; 32]>,
}

impl std::fmt::Debug for AuthToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.secret {
            None => f.write_str("AuthToken(disabled)"),
            Some(_) => f.write_str("AuthToken(<redacted>)"),
        }
    }
}

impl AuthToken {
    pub fn disabled() -> Self {
        Self { secret: None }
    }

    pub fn from_bytes(secret: [u8; 32]) -> Self {
        Self { secret: Some(secret) }
    }

    /// Parse a 64-character hex string into a token.
    pub fn from_hex(hex: &str) -> Result<Self, String> {
        let hex = hex.trim();
        if hex.len() != 64 {
            return Err(format!(
                "expected 64 hex chars (32 bytes), got {}",
                hex.len()
            ));
        }
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
                .map_err(|e| format!("invalid hex at byte {i}: {e}"))?;
        }
        Ok(Self::from_bytes(out))
    }

    pub fn is_enabled(&self) -> bool {
        self.secret.is_some()
    }

    /// Build the line to write as the first line of an outbound connection.
    /// Returns `None` when auth is disabled (caller should skip the write).
    pub fn make_auth_line(&self) -> Option<String> {
        let secret = self.secret?;
        let mut nonce = [0u8; NONCE_LEN];
        rand::thread_rng().fill_bytes(&mut nonce);
        let mac = hmac_sha256(&secret, &nonce);
        Some(format!("AUTH {} {}\n", hex_encode(&mac), hex_encode(&nonce)))
    }

    /// Verify a line received as the first line of an inbound connection.
    /// Returns `true` if auth is disabled (no handshake required) or if the
    /// HMAC matches.
    pub fn verify_auth_line(&self, line: &str) -> bool {
        let Some(secret) = self.secret else {
            return true;
        };
        let trimmed = line.trim_end_matches(['\r', '\n']);
        let Some(rest) = trimmed.strip_prefix("AUTH ") else {
            return false;
        };
        let mut parts = rest.splitn(2, ' ');
        let mac_hex = parts.next().unwrap_or("");
        let nonce_hex = parts.next().unwrap_or("");
        if mac_hex.len() != HMAC_LEN * 2 || nonce_hex.len() != NONCE_LEN * 2 {
            return false;
        }
        let Some(mac) = hex_decode(mac_hex) else { return false };
        let Some(nonce) = hex_decode(nonce_hex) else {
            return false;
        };
        let expected = hmac_sha256(&secret, &nonce);
        constant_time_eq(&mac, &expected)
    }

    /// Bearer token for the HTTP gateway. We use the hex-encoded secret as
    /// the bearer string directly; the HTTP path doesn't need the
    /// nonce-replay protection because it's already TLS-terminated in
    /// production (and even in dev, an attacker on the LAN who can sniff
    /// one POST already has access to everything else).
    pub fn bearer_value(&self) -> Option<String> {
        self.secret.map(|s| hex_encode(&s))
    }

    /// True if the bearer header value matches the configured token.
    /// Disabled tokens accept any value (including missing).
    pub fn verify_bearer(&self, header_value: Option<&str>) -> bool {
        let Some(secret) = self.secret else {
            return true;
        };
        let Some(v) = header_value else { return false };
        let v = v.trim();
        let Some(presented) = v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer "))
        else {
            return false;
        };
        let expected = hex_encode(&secret);
        constant_time_eq(presented.trim().as_bytes(), expected.as_bytes())
    }
}

/// HMAC-SHA256 per RFC 2104. Key is padded/truncated to BLOCK_SIZE.
fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; HMAC_LEN] {
    let mut k = [0u8; BLOCK_SIZE];
    if key.len() > BLOCK_SIZE {
        // Hash long keys down (per RFC).
        let h = Sha256::digest(key);
        k[..h.len()].copy_from_slice(&h);
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK_SIZE];
    let mut opad = [0x5cu8; BLOCK_SIZE];
    for i in 0..BLOCK_SIZE {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(msg);
    let inner_digest = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_digest);
    let out = outer.finalize();
    let mut result = [0u8; HMAC_LEN];
    result.copy_from_slice(&out);
    result
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for i in (0..s.len()).step_by(2) {
        out.push(u8::from_str_radix(&s[i..i + 2], 16).ok()?);
    }
    Some(out)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_token() -> AuthToken {
        AuthToken::from_bytes([0xAB; 32])
    }

    #[test]
    fn disabled_token_accepts_any_line() {
        let t = AuthToken::disabled();
        assert!(t.verify_auth_line(""));
        assert!(t.verify_auth_line("AUTH bogus bogus"));
        assert!(t.verify_auth_line("not an auth line at all"));
    }

    #[test]
    fn disabled_token_emits_no_auth_line() {
        assert!(AuthToken::disabled().make_auth_line().is_none());
    }

    #[test]
    fn enabled_token_round_trip_makes_verifiable_line() {
        let t = fixed_token();
        for _ in 0..16 {
            let line = t.make_auth_line().expect("enabled");
            assert!(t.verify_auth_line(&line), "fresh line failed verify: {line}");
        }
    }

    #[test]
    fn make_auth_line_uses_fresh_nonce() {
        let t = fixed_token();
        let a = t.make_auth_line().unwrap();
        let b = t.make_auth_line().unwrap();
        assert_ne!(a, b, "nonces should differ across calls");
    }

    #[test]
    fn rejects_wrong_secret() {
        let mine = fixed_token();
        let theirs = AuthToken::from_bytes([0xCD; 32]);
        let line = theirs.make_auth_line().unwrap();
        assert!(!mine.verify_auth_line(&line));
    }

    #[test]
    fn rejects_garbage_lines() {
        let t = fixed_token();
        assert!(!t.verify_auth_line(""));
        assert!(!t.verify_auth_line("\n"));
        assert!(!t.verify_auth_line("AUTH"));
        assert!(!t.verify_auth_line("AUTH abc"));
        assert!(!t.verify_auth_line("AUTH zzzz zzzz"));
        assert!(!t.verify_auth_line("HELLO 0011 2233"));
    }

    #[test]
    fn from_hex_round_trip() {
        let bytes = [0x12u8; 32];
        let hex = hex_encode(&bytes);
        let t = AuthToken::from_hex(&hex).unwrap();
        assert_eq!(t.bearer_value().unwrap(), hex);
    }

    #[test]
    fn from_hex_rejects_bad_length() {
        assert!(AuthToken::from_hex("ab").is_err());
        assert!(AuthToken::from_hex(&"a".repeat(63)).is_err());
        assert!(AuthToken::from_hex(&"a".repeat(65)).is_err());
    }

    #[test]
    fn bearer_verify_disabled_accepts_anything() {
        let t = AuthToken::disabled();
        assert!(t.verify_bearer(None));
        assert!(t.verify_bearer(Some("Bearer foo")));
        assert!(t.verify_bearer(Some("nonsense")));
    }

    #[test]
    fn bearer_verify_enabled_requires_match() {
        let t = fixed_token();
        let good = format!("Bearer {}", t.bearer_value().unwrap());
        assert!(t.verify_bearer(Some(&good)));
        assert!(!t.verify_bearer(None));
        assert!(!t.verify_bearer(Some("Bearer wrong")));
        assert!(!t.verify_bearer(Some(&t.bearer_value().unwrap()))); // missing prefix
    }

    #[test]
    fn hmac_known_answer_rfc4231_test_case_1() {
        // RFC 4231 §4.2 test case 1: key = 20 bytes of 0x0b, data = "Hi There"
        let key = [0x0bu8; 20];
        let msg = b"Hi There";
        let expected = [
            0xb0, 0x34, 0x4c, 0x61, 0xd8, 0xdb, 0x38, 0x53, 0x5c, 0xa8, 0xaf, 0xce, 0xaf, 0x0b,
            0xf1, 0x2b, 0x88, 0x1d, 0xc2, 0x00, 0xc9, 0x83, 0x3d, 0xa7, 0x26, 0xe9, 0x37, 0x6c,
            0x2e, 0x32, 0xcf, 0xf7,
        ];
        assert_eq!(hmac_sha256(&key, msg), expected);
    }
}
