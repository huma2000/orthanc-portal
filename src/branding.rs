//! # branding.rs
//!
//! The **logo** and the **website icon** (favicon) that administrators upload from
//! Settings > Branding, so every practice can show its own identity.
//!
//! They are stored in the SQLite database and kept in memory here. They are
//! **public** on purpose: the login page and the browser tab need them before
//! anybody has signed in (see `routes/branding.rs`).
//!
//! ## Why uploaded images are checked so carefully
//! The image is served from the portal's own address, so a malicious file would run
//! with the portal's privileges. Therefore:
//!  * the type is decided from the file's **first bytes**, never from the name or the
//!    `Content-Type` the browser claims;
//!  * only PNG, JPEG, WebP, GIF, ICO and SVG are accepted, with small size limits;
//!  * an **SVG** (the only format that can contain code) must not contain scripts,
//!    event handlers, `<foreignObject>`, embedded pages or entities;
//!  * whatever the check misses is neutralised when serving: images get a
//!    `sandbox` Content-Security-Policy that forbids running anything.

use crate::db::Db;
use crate::error::AppError;
use crate::util::now_unix;
use axum::body::Bytes;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// The two kinds of branding image.
pub const LOGO: &str = "logo";
pub const FAVICON: &str = "favicon";

/// Size limits of the uploaded files.
pub const MAX_LOGO_BYTES: usize = 1024 * 1024; // 1 MB
pub const MAX_FAVICON_BYTES: usize = 256 * 1024; // 256 KB

/// The mark shown when no logo / icon has been uploaded (compiled into the program).
pub const DEFAULT_MARK_SVG: &str = include_str!("../static/favicon.svg");

/// Largest allowed file for a kind, or `None` if the kind does not exist.
pub fn max_bytes(kind: &str) -> Option<usize> {
    match kind {
        LOGO => Some(MAX_LOGO_BYTES),
        FAVICON => Some(MAX_FAVICON_BYTES),
        _ => None,
    }
}

/// One uploaded image, ready to be served.
pub struct BrandImage {
    pub content_type: &'static str,
    pub data: Bytes,
    /// Last change (unix seconds). Added to the URL (`?v=`) so browsers fetch a new
    /// image as soon as it changes.
    pub version: i64,
}

impl BrandImage {
    /// Public address of this image, e.g. `/branding/logo?v=1760000000`.
    pub fn url(&self, kind: &str) -> String {
        format!("/branding/{kind}?v={}", self.version)
    }
}

/// Memory copy of the uploaded images. Cheap to clone.
#[derive(Clone, Default)]
pub struct BrandingStore {
    images: Arc<RwLock<HashMap<String, Arc<BrandImage>>>>,
}

/// Maps a stored content type back to the fixed strings this module serves.
fn known_content_type(stored: &str) -> Option<&'static str> {
    ["image/png", "image/jpeg", "image/webp", "image/gif", "image/x-icon", "image/svg+xml"]
        .into_iter()
        .find(|known| *known == stored)
}

impl BrandingStore {
    /// Reads the saved images from the database (at start-up).
    pub fn load(db: &Db) -> Result<Self, AppError> {
        let store = Self::default();
        for (kind, content_type, data, version) in db.load_branding()? {
            if let (Some(content_type), true) = (known_content_type(&content_type), max_bytes(&kind).is_some()) {
                store.set(&kind, BrandImage { content_type, data: Bytes::from(data), version });
            }
        }
        Ok(store)
    }

    pub fn get(&self, kind: &str) -> Option<Arc<BrandImage>> {
        self.images.read().unwrap_or_else(|e| e.into_inner()).get(kind).cloned()
    }

    pub fn set(&self, kind: &str, image: BrandImage) {
        self.images.write().unwrap_or_else(|e| e.into_inner()).insert(kind.to_string(), Arc::new(image));
    }

    pub fn remove(&self, kind: &str) {
        self.images.write().unwrap_or_else(|e| e.into_inner()).remove(kind);
    }

    /// What the web UI needs to show the branding: public addresses and whether
    /// each image is a custom one. Used by `/api/public` and the settings screen.
    pub fn describe(&self) -> Value {
        let logo = self.get(LOGO).map(|i| json!({ "url": i.url(LOGO), "custom": true, "size": i.data.len() }));
        let favicon = match self.get(FAVICON) {
            Some(i) => json!({ "url": i.url(FAVICON), "type": i.content_type, "custom": true, "size": i.data.len() }),
            None => json!({ "url": "/favicon.svg", "type": "image/svg+xml", "custom": false, "size": 0 }),
        };
        json!({ "logo": logo, "favicon": favicon })
    }

    /// Validates, saves and publishes a new image. Returns an error message
    /// (shown to the administrator) if the file is not acceptable.
    pub fn replace(&self, db: &Db, kind: &str, data: Bytes) -> Result<(), AppError> {
        let max = max_bytes(kind).ok_or(AppError::NotFound)?;
        if data.is_empty() {
            return Err(AppError::BadRequest("The file is empty".into()));
        }
        if data.len() > max {
            return Err(AppError::BadRequest(format!("The file is too big: the limit is {} KB", max / 1024)));
        }
        let content_type = detect_image(&data).map_err(AppError::BadRequest)?;
        let version = now_unix();
        db.save_branding(kind, content_type, &data, version)?;
        self.set(kind, BrandImage { content_type, data, version });
        Ok(())
    }
}

/// Decides the image type from its first bytes. `Err` explains why it is refused.
pub fn detect_image(data: &[u8]) -> Result<&'static str, String> {
    let starts = |magic: &[u8]| data.len() >= magic.len() && &data[..magic.len()] == magic;
    if starts(b"\x89PNG\r\n\x1a\n") {
        Ok("image/png")
    } else if starts(&[0xFF, 0xD8, 0xFF]) {
        Ok("image/jpeg")
    } else if starts(b"GIF87a") || starts(b"GIF89a") {
        Ok("image/gif")
    } else if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Ok("image/webp")
    } else if starts(&[0x00, 0x00, 0x01, 0x00]) {
        Ok("image/x-icon")
    } else if let Ok(text) = std::str::from_utf8(data) {
        check_svg(text).map(|_| "image/svg+xml")
    } else {
        Err("Unsupported file. Use PNG, JPEG, WebP, GIF, ICO or SVG".into())
    }
}

/// Accepts an SVG only if it holds nothing that can run code or load other content.
fn check_svg(text: &str) -> Result<(), String> {
    let lower = text.trim_start_matches('\u{feff}').to_ascii_lowercase();
    let trimmed = lower.trim_start();
    if !(trimmed.starts_with("<svg") || trimmed.starts_with("<?xml") || trimmed.starts_with("<!doctype svg"))
        || !lower.contains("<svg")
    {
        return Err("Unsupported file. Use PNG, JPEG, WebP, GIF, ICO or SVG".into());
    }
    const FORBIDDEN: [&str; 9] =
        ["<script", "javascript:", "<foreignobject", "<iframe", "<embed", "<object", "<!entity", "<audio", "<video"];
    if let Some(bad) = FORBIDDEN.iter().find(|f| lower.contains(*f)) {
        return Err(format!("This SVG is not allowed: it contains '{bad}'"));
    }
    if has_event_handler(&lower) {
        return Err("This SVG is not allowed: it contains an event handler (onload, onclick...)".into());
    }
    Ok(())
}

/// Looks for `onxxx=` attributes (`onload=`, `onclick =`...). `lower` is lower-cased.
fn has_event_handler(lower: &str) -> bool {
    let bytes = lower.as_bytes();
    let mut from = 0;
    while let Some(found) = lower[from..].find("on") {
        let start = from + found;
        from = start + 2;
        // Must begin a word: preceded by whitespace, a quote or '/'.
        let begins_word = start == 0 || matches!(bytes[start - 1], b' ' | b'\t' | b'\n' | b'\r' | b'"' | b'\'' | b'/');
        if !begins_word {
            continue;
        }
        let mut end = start + 2;
        while end < bytes.len() && bytes[end].is_ascii_alphabetic() {
            end += 1;
        }
        while end < bytes.len() && bytes[end].is_ascii_whitespace() {
            end += 1;
        }
        if end - start > 4 && end < bytes.len() && bytes[end] == b'=' {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_formats_by_content() {
        assert_eq!(detect_image(b"\x89PNG\r\n\x1a\nrest"), Ok("image/png"));
        assert_eq!(detect_image(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0]), Ok("image/jpeg"));
        assert_eq!(detect_image(b"GIF89a...."), Ok("image/gif"));
        assert_eq!(detect_image(b"RIFF\x00\x00\x00\x00WEBPVP8 "), Ok("image/webp"));
        assert_eq!(detect_image(&[0, 0, 1, 0, 1, 0]), Ok("image/x-icon"));
        assert!(detect_image(b"<html><script>alert(1)</script></html>").is_err());
        assert!(detect_image(b"just some text").is_err());
        assert!(detect_image(&[0x7f, b'E', b'L', b'F']).is_err());
    }

    #[test]
    fn svg_must_be_inert() {
        assert!(detect_image(b"<svg xmlns='http://www.w3.org/2000/svg'><circle r='5'/></svg>").is_ok());
        assert!(detect_image(b"<?xml version='1.0'?><svg><path d='M0 0'/></svg>").is_ok());
        assert!(detect_image(b"<svg><script>alert(1)</script></svg>").is_err());
        assert!(detect_image(b"<svg onload='alert(1)'></svg>").is_err());
        assert!(detect_image(b"<svg><a href='javascript:alert(1)'><text>x</text></a></svg>").is_err());
        assert!(detect_image(b"<svg><foreignObject><div/></foreignObject></svg>").is_err());
        assert!(detect_image(b"<svg><rect width='1' onclick = 'x()'/></svg>").is_err());
        // "online" or "icon" inside normal words must not be mistaken for handlers
        assert!(detect_image(b"<svg><text>Radiology online icon=1</text></svg>").is_ok());
    }
}
