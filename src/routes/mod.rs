//! # routes/mod.rs
//!
//! Builds the complete HTTP router and the two cross-cutting middlewares.
//!
//! Two listeners, one program
//! --------------------------
//! `main.rs` starts the same application twice:
//!
//! * **intranet listener** (Settings > Network, default :8080) - every feature,
//!   limited only by each user's permissions;
//! * **remote listener** (Settings > Network, default :8081) - the one the
//!   Cloudflare Tunnel must point to. It is **view-only**: the routes for upload,
//!   download, sending, settings and administration are not even registered there.
//!
//! On top of that, any request carrying Cloudflare headers is treated as remote
//! even on the intranet listener (see `auth::effective_scope`).
//!
//! URL map
//! -------
//! ```text
//! /                       web UI (single page, embedded in the binary)
//! /static/app.js|css      UI assets
//! /healthz                liveness probe (no auth)
//! /api/public             branding: name, title, logo, icon (no auth)
//! /branding/logo|favicon  uploaded logo / website icon     (no auth)
//!
//! -- available everywhere (intranet + remote) --
//! /api/login|logout|me    session endpoints
//! /api/studies, /series   consult / search               (permission: read)
//! /api/*/thumbnail        preview image                  (permission: visualize)
//! /dicom-web/*, /wado     read-only DICOMweb proxy       (Weasis and OHIF)
//! /ohif/*                 OHIF web viewer proxy          (permission: visualize)
//!
//! -- intranet listener only --
//! /api/weasis/link        build a weasis:// URI          (permission: visualize)
//! /api/upload             upload a DICOM file            (permission: upload)
//! /api/*/download         ZIP download                   (permission: download)
//! /api/modalities         DICOM destinations             (permission: send)
//! /api/studies/{id}/send  send a study to a destination  (permission: send)
//! /api/transfers          follow the transfers           (permission: send)
//! /api/admin/users|audit  users + audit log              (permission: admin)
//! /api/admin/settings     web-editable configuration     (permission: admin)
//! /api/admin/orthanc/test "Test connection"              (permission: admin)
//! /api/admin/branding/*   upload / remove logo and icon  (permission: admin)
//! /api/admin/nodes        DICOM nodes of Orthanc         (permission: admin)
//! ```

pub mod admin;
pub mod branding;
pub mod download;
pub mod send;
pub mod session;
pub mod settings;
pub mod studies;
pub mod thumbnails;
pub mod upload;
pub mod viewer;

use crate::permissions::AccessScope;
use crate::state::AppState;
use crate::util::is_https;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde_json::json;
use std::sync::OnceLock;

/// Creates the router for one listener. `scope` decides which routes exist:
/// the `Remote` router only contains the view-only ones.
pub fn router(state: AppState, scope: AccessScope) -> Router {
    // The upload size limit is a setting (Settings > Uploads) read on every request.
    let upload_guard = middleware::from_fn_with_state(state.clone(), upload_limit);

    // ---- Routes present on BOTH listeners: sign in, search, view -------------
    let mut api = Router::new()
        // --- session ---
        .route("/login", post(session::login))
        .route("/logout", post(session::logout))
        .route("/me", get(session::me))
        // --- consult ---
        .route("/studies", get(studies::search_studies))
        .route("/studies/{id}/series", get(studies::study_series))
        .route("/series", get(studies::search_series))
        // --- preview thumbnails (they need `visualize`; see routes/thumbnails.rs) ---
        .route("/studies/{id}/thumbnail", get(thumbnails::study_thumbnail))
        .route("/series/{id}/thumbnail", get(thumbnails::series_thumbnail))
        // (Viewing in the browser needs no API call: OHIF is reached through the /ohif proxy.)
        // --- portal name / login notice, readable before signing in ---
        .route("/public", get(settings::public_info));

    // ---- Routes that exist ONLY on the intranet listener ---------------------
    if scope == AccessScope::Full {
        api = api
            // --- Weasis: it saves the images on the computer, so never over the internet ---
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
            // --- administration ---
            .route("/admin/users", get(admin::list_users).post(admin::create_user))
            .route("/admin/users/{id}", put(admin::update_user).delete(admin::delete_user))
            .route("/admin/audit", get(admin::audit_log))
            // --- logo and website icon ---
            .route("/admin/branding/{kind}", put(branding::upload_image).delete(branding::delete_image))
            // --- configuration from the web interface ---
            .route("/admin/settings", get(settings::get_settings).put(settings::update_settings))
            .route("/admin/orthanc/test", post(settings::test_orthanc))
            .route("/admin/nodes", get(settings::list_nodes))
            .route("/admin/nodes/{alias}", put(settings::put_node).delete(settings::delete_node))
            .route("/admin/nodes/{alias}/echo", post(settings::echo_node));
    }
    // All JSON/API endpoints share the CSRF guard.
    let api = api.layer(middleware::from_fn(csrf_guard));

    Router::new()
        // --- web UI, compiled into the executable: nothing to deploy besides the binary ---
        .route("/", get(branding::index)) // the page, with the configured title and icon filled in
        .route("/static/app.js", get(|headers: HeaderMap| serve_static(&APP_JS, headers)))
        .route("/static/app.css", get(|headers: HeaderMap| serve_static(&APP_CSS, headers)))
        .route("/favicon.svg", get(|headers: HeaderMap| serve_static(&DEFAULT_MARK, headers)))
        .route("/healthz", get(|| async { "ok" }))
        // --- public branding images (the login page and the browser tab need them) ---
        .route("/branding/logo", get(branding::logo_image))
        .route("/branding/favicon", get(branding::favicon_image))
        // --- read-only proxy for the viewers (Weasis + OHIF) ---
        .route("/dicom-web/{*path}", get(viewer::dicomweb_proxy))
        .route("/wado", get(viewer::wado_proxy))
        .route("/ohif", get(viewer::ohif_redirect))
        .route("/ohif/", get(viewer::ohif_index))
        .route("/ohif/{*path}", get(viewer::ohif_proxy))
        .nest("/api", api)
        .layer(middleware::from_fn(security_headers))
        .with_state(AppState { listener_scope: scope, ..state })
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
        "default-src 'self'; img-src 'self' data:; style-src 'self'; script-src 'self'; \
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
