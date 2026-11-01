//! # util.rs
//!
//! Small helpers shared by several modules: clock, random tokens, strict input
//! validation (these checks protect the Orthanc URLs and the Weasis command
//! line from injection) and detection of the public URL of the portal.

use crate::error::AppError;
use crate::settings::Settings;
use axum::http::{header, HeaderMap};
use rand::RngCore;
use std::time::{SystemTime, UNIX_EPOCH};

/// Current time as seconds since 1970-01-01 (UTC).
pub fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// 256 bits of randomness from the operating system, hex encoded (64 chars).
/// Used for session cookies and Weasis access tokens.
pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Orthanc resource identifiers look like `8a5f3e0b-1c2d3e4f-...` (hex + dashes).
/// Validating strictly prevents path tricks such as `../` in URLs sent to Orthanc.
pub fn is_valid_orthanc_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 80 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// A DICOM UID is made of digits and dots only (max 64 chars). The UID is later
/// placed inside a quoted Weasis command, so nothing else may get through.
pub fn is_valid_dicom_uid(uid: &str) -> bool {
    !uid.is_empty() && uid.len() <= 64 && uid.chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// Trims an optional form value and turns blank strings into `None`.
pub fn non_empty(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// DICOM matching uses `*` and `?` wildcards. When the user typed a plain word we
/// search "contains" semantics by wrapping it in `*`; explicit wildcards are kept.
pub fn contains_pattern(term: &str) -> String {
    if term.contains('*') || term.contains('?') {
        term.to_string()
    } else {
        format!("*{term}*")
    }
}

/// Converts the `YYYY-MM-DD` value of an HTML date input into DICOM's `YYYYMMDD`.
pub fn dicom_date(iso: &str) -> Result<String, AppError> {
    let digits: String = iso.chars().filter(|c| *c != '-').collect();
    if digits.len() == 8 && digits.chars().all(|c| c.is_ascii_digit()) {
        Ok(digits)
    } else {
        Err(AppError::BadRequest(format!("Invalid date '{iso}', expected YYYY-MM-DD")))
    }
}

/// Makes a string safe to use as a file name inside `Content-Disposition`.
pub fn sanitize_filename(s: &str) -> String {
    let cleaned: String =
        s.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' }).collect();
    let trimmed = cleaned.trim_matches('_');
    if trimmed.is_empty() {
        "download".to_string()
    } else {
        trimmed.chars().take(80).collect()
    }
}

/// Did the browser reach us over HTTPS? Cloudflare (and most reverse proxies)
/// tell us through `X-Forwarded-Proto` / `CF-Visitor`; direct intranet HTTP access has neither.
pub fn is_https(headers: &HeaderMap) -> bool {
    let forwarded = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(',').next().unwrap_or("").trim().eq_ignore_ascii_case("https"))
        .unwrap_or(false);
    let cf_visitor =
        headers.get("cf-visitor").and_then(|v| v.to_str().ok()).map(|v| v.contains("\"https\"")).unwrap_or(false);
    forwarded || cf_visitor
}

/// The URL the *user's computer* must use to reach this portal, e.g.
/// `https://pacs.example.com` (via Cloudflare) or `http://10.0.0.5:8080` (intranet).
///
/// Weasis runs on the user's machine and needs it to call back our DICOMweb
/// proxy, so it cannot be the internal address of Orthanc.
pub fn public_base_url(headers: &HeaderMap, settings: &Settings) -> Result<String, AppError> {
    if !settings.public_base_url.is_empty() {
        return Ok(settings.public_base_url.clone());
    }
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AppError::BadRequest("Missing Host header".into()))?;
    // Only characters that can legitimately appear in "host[:port]" are accepted,
    // because the value is embedded in a quoted command line.
    if !host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']')) {
        return Err(AppError::BadRequest("Invalid Host header".into()));
    }
    let scheme = if is_https(headers) { "https" } else { "http" };
    Ok(format!("{scheme}://{host}"))
}
