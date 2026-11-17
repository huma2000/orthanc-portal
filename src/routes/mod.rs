//! # routes/mod.rs
//!
//! Builds the HTTP routers and the cross-cutting middlewares.
//!
//! Two doors, one program
//! ----------------------
//! `main.rs` starts the same application twice, with different routes ([`Audience`]):
//!
//! * the **intranet listener** (Settings > Network, default :8080) serves the whole portal to
//!   staff with accounts, limited by each user's permissions;
//! * the **internet listener** (default :8081, the one the Cloudflare Tunnel points to) serves
//!   ONLY **invitation links**: a guest opens `/s/<token>`, enters the password that was
//!   generated for that link, and can then look at that one study in the browser viewer and
//!   download it a limited number of times. There are no accounts, no search and no
//!   administration there; those routes are not even registered.
//!
//! As a safety net, the intranet listener refuses any request that carries Cloudflare headers
//! (it would mean the tunnel was pointed at the wrong port).
//!
//! URL map
//! -------
//! ```text
//! -- intranet listener (staff) --
//! /                        web UI (single page, embedded in the binary)
//! /static/app.js|css       UI assets
//! /api/login|logout|me     session endpoints
//! /api/studies, /series    consult / search               (permission: read)
//! /api/stats               number of studies in the PACS  (permission: read)
//! /api/*/thumbnail         preview image                  (permission: visualize)
//! /api/weasis/link         build a weasis:// URI          (permission: visualize)
//! /dicom-web/*, /wado      read-only DICOMweb proxy       (Weasis and OHIF)
//! /ohif/*                  OHIF web viewer proxy          (permission: visualize)
//! /api/upload              upload a DICOM file            (permission: upload)
//! /api/*/download          ZIP download                   (permission: download)
//! /api/modalities|send|transfers   send to DICOM nodes    (permission: send)
//! /api/studies/{id}/share  create an invitation link      (permission: share)
//! /api/shares              list / revoke invitations      (permission: share)
//! /api/admin/*             users, audit, settings, branding, DICOM nodes (permission: admin)
//!
//! -- internet listener (guests with an invitation) --
//! /s/<token>               the invitation page (asks for the password)
//! /static/share.js|app.css assets of that page
//! /api/share/<token>/*     status, unlock (password), study details, ZIP download
//! /dicom-web/*, /wado, /ohif/*   the viewer, confined to the shared study (routes/viewer.rs)
//! /branding/*, /api/public, /healthz   public
//! ```

pub mod admin;
pub mod branding;
pub mod download;
pub mod guest;
pub mod send;
pub mod session;
pub mod settings;
pub mod shares;
pub mod studies;
pub mod thumbnails;
pub mod upload;
pub mod viewer;

use crate::auth;
use crate::state::AppState;
use crate::util::is_https;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Extension, Json, Router};
use serde_json::json;
use std::sync::OnceLock;

/// Who a listener serves. Handlers shared by both (the viewer proxy) look at it to decide
/// how to authenticate the caller.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Audience {
    /// The intranet listener: the whole portal, for staff with accounts.
    Staff,
    /// The internet-facing listener: only invitation links, for guests without accounts.
    Guest,
}

/// Creates the router of one listener.
pub fn router(state: AppState, audience: Audience) -> Router {
    match audience {
        Audience::Staff => staff_router(state),
        Audience::Guest => guest_router(state),
    }
}

/// Routes common to both listeners: static files, health check, branding images and the
/// read-only viewer proxy (which authenticates according to the [`Audience`]).
fn common_routes() -> Router<AppState> {
    Router::new()
        .route("/static/app.css", get(|headers: HeaderMap| serve_static(&APP_CSS, headers)))
        .route("/favicon.svg", get(|headers: HeaderMap| serve_static(&DEFAULT_MARK, headers)))
        .route("/healthz", get(|| async { "ok" }))
        // --- public branding images (login / invitation pages and the browser tab need them) ---
        .route("/branding/logo", get(branding::logo_image))
        .route("/branding/favicon", get(branding::favicon_image))
        // --- read-only proxy for the viewers (Weasis + OHIF) ---
        .route("/dicom-web/{*path}", get(viewer::dicomweb_proxy))
        .route("/wado", get(viewer::wado_proxy))
        .route("/ohif", get(viewer::ohif_redirect))
        .route("/ohif/", get(viewer::ohif_index))
        .route("/ohif/{*path}", get(viewer::ohif_proxy))
}

/// The intranet portal for staff.
fn staff_router(state: AppState) -> Router {
    // The upload size limit is a setting (Settings > Uploads) read on every request.
    let upload_guard = middleware::from_fn_with_state(state.clone(), upload_limit);

    let api = Router::new()
        // --- session ---
        .route("/login", post(session::login))
        .route("/logout", post(session::logout))
        .route("/me", get(session::me))
        // --- portal name / login notice, readable before signing in ---
        .route("/public", get(settings::public_info))
        // --- consult ---
        .route("/studies", get(studies::search_studies))
        .route("/studies/{id}/series", get(studies::study_series))
        .route("/series", get(studies::search_series))
        .route("/stats", get(studies::statistics))
        // --- preview thumbnails (they need `visualize`; see routes/thumbnails.rs) ---
        .route("/studies/{id}/thumbnail", get(thumbnails::study_thumbnail))
        .route("/series/{id}/thumbnail", get(thumbnails::series_thumbnail))
        // --- Weasis ---
        .route("/weasis/link", get(viewer::weasis_link))
        // --- upload (with its own, adjustable body-size limit) ---
        .route("/upload", post(upload::upload).layer(upload_guard))
        // --- download ---
        .route("/studies/{id}/download", get(download::download_study))
        .route("/series/{id}/download", get(download::download_series))
        // --- send to remote DICOM nodes ---
        .route("/modalities", get(send::list_modalities))
        .route("/studies/{id}/send", post(send::send_study))
        .route("/transfers", get(send::list_transfers))
        // --- invitation links for people outside ---
        .route("/studies/{id}/share", post(shares::create_share))
        .route("/shares", get(shares::list_shares))
        .route("/shares/{id}/revoke", post(shares::revoke_share))
        .route("/shares/{id}/password", post(shares::show_password))
        .route("/shares/{id}", put(shares::update_share))
        // --- administration ---
        .route("/admin/users", get(admin::list_users).post(admin::create_user))
        .route("/admin/users/{id}", put(admin::update_user).delete(admin::delete_user))
        .route("/admin/audit", get(admin::audit_log))
        // --- logo and website icon ---
        .route("/admin/branding/{kind}", put(branding::upload_image).delete(branding::delete_image))
        // --- configuration from the web interface ---
        .route("/admin/settings", get(settings::get_settings).put(settings::update_settings))
        .route("/admin/orthanc/test", post(settings::test_orthanc))
        .route("/admin/email/test", post(settings::test_email))
        .route("/admin/nodes", get(settings::list_nodes))
        .route("/admin/nodes/{alias}", put(settings::put_node).delete(settings::delete_node))
        .route("/admin/nodes/{alias}/echo", post(settings::echo_node))
        // All JSON/API endpoints share the CSRF guard.
        .layer(middleware::from_fn(csrf_guard));

    common_routes()
        // --- web UI, compiled into the executable: nothing to deploy besides the binary ---
        .route("/", get(branding::index)) // the page, with the configured title and icon filled in
        .route("/static/app.js", get(|headers: HeaderMap| serve_static(&APP_JS, headers)))
        .nest("/api", api)
        .layer(Extension(Audience::Staff))
        .layer(middleware::from_fn(reject_internet_traffic))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

/// The internet-facing side: invitation links only.
fn guest_router(state: AppState) -> Router {
    let api = Router::new()
        .route("/public", get(settings::public_info))
        .route("/share/{token}/status", get(guest::status))
        .route("/share/{token}/unlock", post(guest::unlock))
        .route("/share/{token}/study", get(guest::study))
        .route("/share/{token}/download", get(guest::download))
        .route("/share/{token}/lock", post(guest::lock))
        .layer(middleware::from_fn(csrf_guard));

    common_routes()
        .route("/", get(guest::page))
        .route("/s/{token}", get(guest::page))
        .route("/static/share.js", get(|headers: HeaderMap| serve_static(&SHARE_JS, headers)))
        .nest("/api", api)
        .layer(Extension(Audience::Guest))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

/// Safety net of the intranet listener: a request carrying Cloudflare's headers reached it
/// through the tunnel, which must point at the internet-facing listener. Nothing of the staff
/// portal is served to it.
async fn reject_internet_traffic(request: Request, next: Next) -> Response {
    if request.uri().path() != "/healthz" && auth::is_cloudflare_request(request.headers()) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "This address is for the intranet only. Open the invitation link you were sent." })),
        )
            .into_response();
    }
    next.run(request).await
}

/// A file compiled into the program, served with an `ETag` computed from its content.
/// Browsers then ask "has it changed?" and get a tiny `304 Not Modified` instead of the
/// whole file again on every page load (the ETag changes by itself with each new version).
struct StaticFile {
    content_type: &'static str,
    body: &'static str,
    etag: OnceLock<String>,
}

static APP_JS: StaticFile = StaticFile {
    content_type: "text/javascript; charset=utf-8",
    body: include_str!("../../static/app.js"),
    etag: OnceLock::new(),
};
static SHARE_JS: StaticFile = StaticFile {
    content_type: "text/javascript; charset=utf-8",
    body: include_str!("../../static/share.js"),
    etag: OnceLock::new(),
};
static APP_CSS: StaticFile = StaticFile {
    content_type: "text/css; charset=utf-8",
    body: include_str!("../../static/app.css"),
    etag: OnceLock::new(),
};
static DEFAULT_MARK: StaticFile =
    StaticFile { content_type: "image/svg+xml", body: include_str!("../../static/favicon.svg"), etag: OnceLock::new() };

impl StaticFile {
    /// `"<64-bit FNV-1a hash of the content>"`, computed once.
    fn etag(&self) -> &str {
        self.etag.get_or_init(|| {
            let hash = self
                .body
                .bytes()
                .fold(0xcbf2_9ce4_8422_2325_u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3));
            format!("\"{hash:016x}\"")
        })
    }
}

/// Serves an embedded file: `304` when the browser already has this exact version.
async fn serve_static(file: &'static StaticFile, headers: HeaderMap) -> Response {
    let etag = file.etag();
    let browser_has_it = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(',').any(|candidate| candidate.trim().trim_start_matches("W/") == etag))
        .unwrap_or(false);
    // `no-cache` = "keep a copy, but ask before using it" (cheap thanks to the 304).
    if browser_has_it {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag), (header::CACHE_CONTROL, "no-cache")]).into_response();
    }
    ([(header::CONTENT_TYPE, file.content_type), (header::CACHE_CONTROL, "no-cache"), (header::ETAG, etag)], file.body)
        .into_response()
}

/// Enforces the maximum upload size, which is a setting an administrator can
/// change at any time. Rejects at once when the announced `Content-Length` is too
/// big, and also cuts off a body that grows past the limit while streaming.
async fn upload_limit(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let max = state.settings.get().max_upload_bytes();
    let announced_too_big = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        .map(|len| len > max)
        .unwrap_or(false);
    if announced_too_big {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({ "error": format!("The file is larger than the {} MB limit", max / 1024 / 1024) })),
        )
            .into_response();
    }
    let (parts, body) = request.into_parts();
    let limited = Body::new(http_body_util::Limited::new(body, max));
    next.run(Request::from_parts(parts, limited)).await
}

/// **CSRF protection** for state-changing API calls (POST/PUT/DELETE).
///
/// Browsers refuse to add a custom header such as `X-Requested-With` to a
/// *cross-site* request unless the target explicitly allows it (CORS), and this
/// portal never does. A malicious web page therefore cannot forge a request
/// with the victim's cookie. Our own UI always sends `X-Requested-With: portal`.
async fn csrf_guard(request: Request, next: Next) -> Response {
    let is_safe_method = matches!(*request.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    let has_marker =
        request.headers().get("x-requested-with").and_then(|v| v.to_str().ok()).map(|v| v == "portal").unwrap_or(false);

    if !is_safe_method && !has_marker {
        return (StatusCode::FORBIDDEN, Json(json!({ "error": "Missing X-Requested-With header" }))).into_response();
    }
    next.run(request).await
}

/// Adds browser security headers to every response:
///  * `Content-Security-Policy`: only scripts/styles served by this portal may run;
///  * `X-Frame-Options` / `frame-ancestors`: the portal cannot be embedded in another site;
///  * `nosniff`, `no-referrer`: stop content-type guessing and referrer leaks.
///
/// The OHIF web viewer (`/ohif/*`) is a large third-party application that needs
/// inline styles, WebAssembly decoders and web workers, so ONLY those pages get a
/// more permissive policy. It is still limited to this origin (no external sites).
async fn security_headers(request: Request, next: Next) -> Response {
    let path = request.uri().path();
    let is_ohif = path == "/ohif" || path.starts_with("/ohif/");
    let is_branding_image = path.starts_with("/branding/");
    let is_api = path.starts_with("/api/");
    let over_https = is_https(request.headers());

    let policy = if is_branding_image {
        // Uploaded images can never run anything, even if opened directly in a tab.
        "default-src 'none'; style-src 'unsafe-inline'; sandbox"
    } else if is_ohif {
        "default-src 'self'; \
         script-src 'self' 'unsafe-inline' 'unsafe-eval' 'wasm-unsafe-eval' blob:; \
         worker-src 'self' blob:; style-src 'self' 'unsafe-inline'; \
         img-src 'self' data: blob:; font-src 'self' data:; connect-src 'self' data: blob:; \
         media-src 'self' data: blob:; frame-ancestors 'none'; base-uri 'self'"
    } else {
        // blob: = the preview thumbnails, which app.js downloads with fetch() (to send the
        // session and tell "no preview" apart from errors) and shows through object URLs.
        "default-src 'self'; img-src 'self' data: blob:; style-src 'self'; script-src 'self'; \
         frame-ancestors 'none'; base-uri 'none'; form-action 'self'"
    };

    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(policy));
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    // The portal never needs the camera, microphone, location...
    headers.insert(
        header::HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static("camera=(), microphone=(), geolocation=(), payment=(), usb=()"),
    );
    // Reached over HTTPS (Cloudflare): tell browsers to keep using HTTPS for six months.
    if over_https {
        headers.insert(header::STRICT_TRANSPORT_SECURITY, HeaderValue::from_static("max-age=15552000"));
    }
    // Patient data in JSON answers must never be stored by browsers or proxies. (Routes that
    // already chose their own policy, such as thumbnails and downloads, keep it.)
    if is_api && !headers.contains_key(header::CACHE_CONTROL) {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}
