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
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        text.push(HEX[usize::from(b >> 4)] as char);
        text.push(HEX[usize::from(b & 0x0f)] as char);
    }
    text
}

/// Orthanc resource identifiers look like `8a5f3e0b-1c2d3e4f-...` (hex + dashes).
/// Validating strictly prevents path tricks such as `../` in URLs sent to Orthanc.
pub fn is_valid_orthanc_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 80 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// `Ok` for a valid Orthanc id, otherwise the "400 Invalid id" every route answers.
pub fn orthanc_id(id: &str) -> Result<(), AppError> {
    if is_valid_orthanc_id(id) {
        Ok(())
    } else {
        Err(AppError::BadRequest("Invalid id".into()))
    }
}

/// `Ok` when an optional feature is switched on, otherwise a 403 that says where to switch it on.
/// `what` is the start of the sentence: "Deleting is switched off".
pub fn feature(on: bool, what: &str) -> Result<(), AppError> {
    if on {
        Ok(())
    } else {
        Err(AppError::Denied(format!("{what} (Settings > Features)")))
    }
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
    // "P-001" between double quotes = exactly that value (as in Orthanc Explorer 2).
    if term.len() >= 2 && term.starts_with('"') && term.ends_with('"') {
        return term[1..term.len() - 1].to_string();
    }
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

/// Days since 1970-01-01 -> (year, month, day), UTC (Howard Hinnant's algorithm: no date library).
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// The first day of a month -> days since 1970-01-01 (the inverse of [`civil_from_days`]).
pub fn days_from_civil(year: i64, month: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (i64::from(month) + 9) % 12;
    let doy = (153 * mp + 2) / 5;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// A file the portal made for one answer (a patient CD, a backup copy): deleted when dropped,
/// at the end of the download or on any failure on the way.
pub struct TempFile(pub std::path::PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Answers with a temporary file as a download, then deletes it (the file travels with the stream).
pub async fn temp_file_download(
    temp: TempFile,
    content_type: &str,
    file_name: &str,
) -> Result<axum::response::Response, AppError> {
    use tokio::io::AsyncReadExt;
    let size = std::fs::metadata(&temp.0).map(|m| m.len()).unwrap_or(0);
    let file = tokio::fs::File::open(&temp.0)
        .await
        .map_err(|e| AppError::Internal(format!("cannot read {file_name}: {e}")))?;
    let stream = futures_util::stream::unfold((file, temp), |(mut file, guard)| async move {
        let mut buffer = vec![0u8; 256 * 1024];
        match file.read(&mut buffer).await {
            Ok(0) => None,
            Ok(n) => {
                buffer.truncate(n);
                Some((Ok::<axum::body::Bytes, std::io::Error>(axum::body::Bytes::from(buffer)), (file, guard)))
            }
            Err(e) => Some((Err(e), (file, guard))),
        }
    });
    axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, size)
        .header(header::CONTENT_DISPOSITION, format!("attachment; filename=\"{file_name}\""))
        // Patient data / secrets: never kept by browsers or proxies.
        .header(header::CACHE_CONTROL, "no-store")
        .body(axum::body::Body::from_stream(stream))
        .map_err(|e| AppError::Internal(format!("cannot build the download: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_without_a_library() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_860), (2024, 5, 17));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(days_from_civil(2024, 3)), (2024, 3, 1));
        assert_eq!(civil_from_days(days_from_civil(2023, 1)), (2023, 1, 1));
    }

    /// Tokens are 64 lowercase hex characters and never repeat.
    #[test]
    fn random_tokens_are_hex_and_unique() {
        let (a, b) = (random_token(), random_token());
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_ne!(a, b);
    }
}
