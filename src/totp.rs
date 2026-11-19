//! # totp.rs
//!
//! **Two-step sign-in**: after the password, a 6-digit code from an authenticator app (Google
//! Authenticator, Microsoft Authenticator, FreeOTP, 1Password...). The codes are TOTP (RFC 6238):
//! HMAC-SHA1 of the current 30-second time step with a secret shared once through a QR code.
//! Pure functions, no HTTP: see `routes/account.rs` (setting it up) and `routes/session.rs`
//! (signing in with it).
//!
//! * The secret is 160 random bits (the size RFC 4226 recommends), shown as base32.
//! * A code is accepted for the current step and one step either side (clocks drift), and only
//!   ONCE: the step used is stored, and an older or equal one is refused (`Db::use_totp_step`).
//! * **Recovery codes** for a lost phone: ten one-time codes `ABCD-EFGH` (40 bits each). Only
//!   their SHA-256 is stored; each works once.

use crate::sharing::normalize_password;
use hmac::{Hmac, Mac};
use rand::{Rng, RngCore};
use sha1::Sha1;
use sha2::{Digest, Sha256};

/// Seconds per code.
pub const STEP_SECS: i64 = 30;
/// Digits of a code.
const DIGITS: u32 = 6;
/// Steps accepted before / after the current one.
const WINDOW: i64 = 1;
/// Number of recovery codes given at once.
pub const RECOVERY_CODES: usize = 10;

const BASE32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// RFC 4648 base32 without padding (what authenticator apps expect).
pub fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for &b in bytes {
        buffer = (buffer << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(BASE32[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(BASE32[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// The opposite of [`base32_encode`] (spaces, dashes and case ignored). `None` for other characters.
pub fn base32_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut buffer, mut bits) = (0u32, 0u32);
    for c in text.chars().filter(|c| !matches!(c, ' ' | '-' | '=')) {
        let value = BASE32.iter().position(|&b| b as char == c.to_ascii_uppercase())? as u32;
        buffer = (buffer << 5) | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}

/// A new secret (base32 text, 32 characters).
pub fn new_secret() -> String {
    let mut bytes = [0u8; 20];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    base32_encode(&bytes)
}

/// The code of time step `step` for `secret` (RFC 4226 dynamic truncation).
pub fn code_at(secret: &[u8], step: i64) -> u32 {
    let mut mac = Hmac::<Sha1>::new_from_slice(secret).expect("HMAC takes keys of any size");
    mac.update(&(step as u64).to_be_bytes());
    let hash = mac.finalize().into_bytes();
    let offset = (hash[19] & 0x0f) as usize;
    let number = u32::from_be_bytes([hash[offset] & 0x7f, hash[offset + 1], hash[offset + 2], hash[offset + 3]]);
    number % 10u32.pow(DIGITS)
}

/// The time step of a moment (unix seconds).
pub fn step_of(now: i64) -> i64 {
    now.div_euclid(STEP_SECS)
}

/// Checks a typed code against `secret` at time `now`. Returns the time step it matched (to be
/// stored so it cannot be used again), or `None`. Spaces in the typed code are ignored.
pub fn verify(secret_base32: &str, typed: &str, now: i64) -> Option<i64> {
    let digits: String = typed.chars().filter(|c| !c.is_whitespace()).collect();
    if digits.len() != DIGITS as usize || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let wanted: u32 = digits.parse().ok()?;
    let secret = base32_decode(secret_base32)?;
    let current = step_of(now);
    // Every candidate is computed (no early exit): the answer takes the same time either way.
    let mut found = None;
    for step in current - WINDOW..=current + WINDOW {
        if code_at(&secret, step) == wanted && found.is_none() {
            found = Some(step);
        }
    }
    found
}

/// The `otpauth://` address put in the QR code: the app shows "<issuer> (<account>)".
pub fn otpauth_uri(issuer: &str, account: &str, secret: &str) -> String {
    use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
    let issuer_enc = utf8_percent_encode(issuer, NON_ALPHANUMERIC).to_string();
    let account_enc = utf8_percent_encode(account, NON_ALPHANUMERIC).to_string();
    format!(
        "otpauth://totp/{issuer_enc}:{account_enc}?secret={secret}&issuer={issuer_enc}&algorithm=SHA1&digits={DIGITS}&period={STEP_SECS}"
    )
}

/// Ten new recovery codes (`ABCD-EFGH`, the readable alphabet of the share passwords).
pub fn new_recovery_codes() -> Vec<String> {
    const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
    let mut rng = rand::rngs::OsRng;
    (0..RECOVERY_CODES)
        .map(|_| {
            let mut code = String::with_capacity(9);
            for i in 0..8 {
                if i == 4 {
                    code.push('-');
                }
                code.push(ALPHABET[rng.gen_range(0..ALPHABET.len())] as char);
            }
            code
        })
        .collect()
}

/// What is stored for a recovery code: SHA-256 of its normalised form (so `abcd efgh` matches).
pub fn recovery_hash(code: &str) -> String {
    let digest = Sha256::digest(normalize_password(code).as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Does the typed text look like a recovery code rather than a 6-digit code?
pub fn looks_like_recovery_code(typed: &str) -> bool {
    normalize_password(typed).len() == 8
}

/// A QR code as an SVG picture (black on white, with the quiet zone apps need).
pub fn qr_svg(text: &str) -> Option<String> {
    let code = qrcode::QrCode::with_error_correction_level(text.as_bytes(), qrcode::EcLevel::M).ok()?;
    Some(
        code.render::<qrcode::render::svg::Color>()
            .min_dimensions(220, 220)
            .dark_color(qrcode::render::svg::Color("#000000"))
            .light_color(qrcode::render::svg::Color("#ffffff"))
            .quiet_zone(true)
            .build(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 6238, appendix B (SHA-1, the 20-byte ASCII secret "12345678901234567890"), 6 digits.
    #[test]
    fn the_codes_of_the_rfc() {
        let secret = b"12345678901234567890";
        for (time, expected) in [
            (59, 287082),
            (1_111_111_109, 81804),
            (1_111_111_111, 50471),
            (1_234_567_890, 5924),
            (2_000_000_000, 279037),
        ] {
            assert_eq!(code_at(secret, step_of(time)), expected, "t={time}");
        }
    }

    #[test]
    fn base32_round_trip() {
        assert_eq!(base32_encode(b"12345678901234567890"), "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ");
        assert_eq!(base32_decode("gezd gnbv-gy3tqojqgezdgnbvgy3tqojq").unwrap(), b"12345678901234567890");
        assert!(base32_decode("not base32!").is_none());
        let s = new_secret();
        assert_eq!(s.len(), 32);
        assert_eq!(base32_decode(&s).unwrap().len(), 20);
    }

    #[test]
    fn a_code_works_in_its_window_only() {
        let secret = base32_encode(b"12345678901234567890");
        let now = 1_111_111_111;
        let code = format!("{:06}", code_at(b"12345678901234567890", step_of(now)));
        assert_eq!(verify(&secret, &code, now), Some(step_of(now)));
        assert_eq!(verify(&secret, &format!("{} {}", &code[..3], &code[3..]), now), Some(step_of(now)));
        assert!(verify(&secret, &code, now + 30).is_some(), "one step later is still fine");
        assert!(verify(&secret, &code, now + 120).is_none(), "four steps later is not");
        assert!(verify(&secret, "12345", now).is_none());
        assert!(verify(&secret, "abcdef", now).is_none());
    }

    #[test]
    fn recovery_codes() {
        let codes = new_recovery_codes();
        assert_eq!(codes.len(), RECOVERY_CODES);
        assert!(codes.iter().all(|c| c.len() == 9 && c.as_bytes()[4] == b'-'));
        assert_eq!(recovery_hash(&codes[0]), recovery_hash(&codes[0].to_lowercase().replace('-', " ")));
        assert_ne!(recovery_hash(&codes[0]), recovery_hash(&codes[1]));
        assert!(looks_like_recovery_code(&codes[0]) && !looks_like_recovery_code("123456"));
    }

    #[test]
    fn the_qr_code_is_an_svg_picture() {
        let uri = otpauth_uri("Chalklands PACS", "dr.smith", "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ");
        assert!(uri.starts_with("otpauth://totp/Chalklands%20PACS:dr%2Esmith?secret=GEZD"), "{uri}");
        let svg = qr_svg(&uri).unwrap();
        assert!(svg.contains("<svg") && !svg.contains("<script"));
    }
}
