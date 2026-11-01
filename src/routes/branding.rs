//! # routes/branding.rs
//!
//! Per-practice identity: window title, website icon and logo.
//!
//! Public (no sign-in needed, on both listeners):
//! * `GET /` - the web page, with the configured **title** and **icon** already written
//!   into the HTML (no flash of the default ones, works without JavaScript)
//! * `GET /branding/logo` - the uploaded logo, or the default mark
//! * `GET /branding/favicon` - the uploaded website icon, or the default mark
//!
//! Administrators only, intranet listener only:
//! * `PUT    /api/admin/branding/{logo|favicon}`   upload an image (raw request body)
//! * `DELETE /api/admin/branding/{logo|favicon}`   remove it (the default is used again)
//!
//! Validation of the uploaded file lives in `branding.rs` (type detected from the
//! content, SVG checked, size limits). The images are served with a `sandbox`
//! Content-Security-Policy, see `security_headers` in `routes/mod.rs`.

use crate::auth::AuthUser;
use crate::branding::{max_bytes, BrandImage, DEFAULT_MARK_SVG};
use crate::error::{AppError, AppResult};
use crate::permissions::Permission;
use crate::state::AppState;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::Value;
use std::sync::Arc;

/// The page template. `{{TITLE}}`, `{{FAVICON_URL}}` and `{{FAVICON_TYPE}}` are
/// filled in for every request.
const INDEX_HTML: &str = include_str!("../../static/index.html");

/// Escapes text for safe use inside HTML (the title is typed by an administrator).
fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

/// `GET /` - the single page of the web UI, personalised.
pub async fn index(State(state): State<AppState>) -> Response {
    let settings = state.settings.get();
    let branding = state.branding.describe();
    let favicon_url = branding["favicon"]["url"].as_str().unwrap_or("/favicon.svg");
    let favicon_type = branding["favicon"]["type"].as_str().unwrap_or("image/svg+xml");

    let html = INDEX_HTML
        .replace("{{TITLE}}", &html_escape(settings.window_title()))
        .replace("{{FAVICON_URL}}", &html_escape(favicon_url))
        .replace("{{FAVICON_TYPE}}", &html_escape(favicon_type));
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8"), (header::CACHE_CONTROL, "no-cache")], html).into_response()
}

/// Wraps an image (or the default mark) in a response. The address carries `?v=<time>`,
/// so a long cache time is safe: a changed image has a different address.
fn image_response(image: Option<Arc<BrandImage>>) -> Response {
    const CACHE: &str = "public, max-age=3600";
    match image {
        Some(image) => {
            ([(header::CONTENT_TYPE, image.content_type), (header::CACHE_CONTROL, CACHE)], image.data.clone())
                .into_response()
        }
        None => ([(header::CONTENT_TYPE, "image/svg+xml"), (header::CACHE_CONTROL, CACHE)], DEFAULT_MARK_SVG)
            .into_response(),
    }
}

pub async fn logo_image(State(state): State<AppState>) -> Response {
    image_response(state.branding.get(crate::branding::LOGO))
}

pub async fn favicon_image(State(state): State<AppState>) -> Response {
    image_response(state.branding.get(crate::branding::FAVICON))
}

/// Uploads a new logo or icon. The body is the raw file; its type is detected from
/// the content, whatever the browser says.
pub async fn upload_image(
    State(state): State<AppState>,
    AuthUser(admin): AuthUser,
    Path(kind): Path<String>,
    body: Bytes,
) -> AppResult<Json<Value>> {
    admin.require(Permission::Admin)?;
    let size = body.len();
    state.branding.replace(&state.db, &kind, body)?;
    state.db.audit(&admin.username, "branding_updated", &format!("{kind} ({size} bytes)"));
    Ok(Json(state.branding.describe()))
}

/// Removes an uploaded image: the default mark is shown again.
pub async fn delete_image(
    State(state): State<AppState>,
    AuthUser(admin): AuthUser,
    Path(kind): Path<String>,
) -> AppResult<Json<Value>> {
    admin.require(Permission::Admin)?;
    max_bytes(&kind).ok_or(AppError::NotFound)?;
    state.db.delete_branding(&kind)?;
    state.branding.remove(&kind);
    state.db.audit(&admin.username, "branding_removed", &kind);
    Ok(Json(state.branding.describe()))
}
