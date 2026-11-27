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
//! /static/app.js|css       UI assets (and /static/i18n/es.json, the Spanish translation)
//! /api/login|logout|me     session endpoints
//! /api/studies, /series    consult / search               (permission: read)
//! /api/stats               number of studies in the PACS  (permission: read)
//! /api/labels, /api/studies/{id}/labels/{label}   labels  (read to see, labels/admin to change)
//! /api/*/thumbnail         preview image                  (permission: visualize)
//! /api/weasis/link         build a weasis:// URI          (permission: visualize)
//! /dicom-web/*, /wado      read-only DICOMweb proxy       (Weasis and OHIF)
//! /ohif/*                  OHIF web viewer proxy          (permission: visualize)
//! /api/upload              upload a DICOM file            (permission: upload)
//! /api/*/download          ZIP download                   (permission: download)
//! /api/modalities|send|transfers   send to DICOM nodes    (permission: send)
//! /api/studies/{id}/share  create an invitation link      (permission: share)
//! /api/shares              list / revoke invitations, QR  (permission: share)
//! /api/upload-links        links for people outside to SEND studies (share + upload, Settings > Features)
//! /api/login/two-factor    second step of a two-step sign-in
//! /api/account/*           my email, my password, my two-step sign-in (any signed-in person)
//! /api/recent              my recently viewed studies     (permission: read)
//! /api/export/cd           patient CD / USB export        (permission: download)
//! /api/admin/*             users, audit, settings, branding, DICOM nodes, statistics, backups (admin)
//!
//! -- internet listener (guests with an invitation) --
//! /s/<token>               the invitation page (asks for the password)
//! /u/<token>               the page of an upload link (password, then drop files)
//! /static/share.js|app.css assets of these pages (and /static/i18n/es.json)
//! /api/share/<token>/*     status, unlock (password), study details, ZIP download
//! /api/upload/<token>/*    status, unlock (password), one file, summary, lock
//! /dicom-web/*, /wado, /ohif/*   the viewer, confined to the shared study (routes/viewer.rs)
//! /branding/*, /api/public, /healthz   public
//! ```

pub mod account;
pub mod admin;
pub mod backup;
pub mod branding;
pub mod copy;
pub mod download;
pub mod export;
pub mod files;
pub mod guest;
pub mod inbound;
pub mod labels;
pub mod manage;
pub mod recent;
pub mod retrieve;
pub mod send;
pub mod session;
pub mod settings;
pub mod shares;
pub mod stats;
pub mod studies;
pub mod system;
pub mod thumbnails;
pub mod upload;
pub mod viewer;
pub mod worklists;

use crate::auth;
use crate::state::AppState;
use crate::util::is_https;
use axum::body::{Body, Bytes};
use axum::extract::DefaultBodyLimit;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
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
        // Translations of the interface (both pages use them: the invitation page too).
        .route("/static/i18n/es.json", get(|headers: HeaderMap| serve_static(&I18N_ES, headers)))
        .route("/favicon.svg", get(|headers: HeaderMap| serve_static(&DEFAULT_MARK, headers)))
        .route("/healthz", get(|| async { "ok" }))
        // --- public branding images (login / invitation pages and the browser tab need them) ---
        .route("/branding/logo", get(branding::logo_image))
        .route("/branding/favicon", get(branding::favicon_image))
        .route("/branding/custom.css", get(branding::custom_css))
        // --- read-only proxy for the viewers (Weasis + OHIF) ---
        .route("/dicom-web/{*path}", get(viewer::dicomweb_proxy))
        .route("/wado", get(viewer::wado_proxy))
        .route("/ohif", get(viewer::ohif_redirect))
        .route("/ohif/", get(viewer::ohif_index))
        .route("/ohif/{*path}", get(viewer::ohif_proxy))
}

/// The other viewers of Orthanc's plugins (staff only; see `viewer::app_proxy`).
fn staff_viewer_routes() -> Router<AppState> {
    let mut router = Router::new();
    for root in ["/stone-webviewer", "/volview", "/wsi"] {
        router = router
            .route(root, get(viewer::app_proxy))
            .route(&format!("{root}/"), get(viewer::app_proxy))
            .route(&format!("{root}/{{*path}}"), get(viewer::app_proxy));
    }
    router
}

/// The intranet portal for staff.
fn staff_router(state: AppState) -> Router {
    // The upload size limit is a setting (Settings > Uploads) read on every request.
    let upload_guard = middleware::from_fn_with_state(state.clone(), upload_limit);
    let add_series_guard = middleware::from_fn_with_state(state.clone(), upload_limit);

    let api = Router::new()
        // --- session (and the second step of a two-step sign-in) ---
        .route("/login", post(session::login))
        .route("/login/two-factor", post(session::login_two_factor))
        .route("/logout", post(session::logout))
        .route("/me", get(session::me))
        // --- my account: email, password, two-step sign-in ---
        .route("/account/email", put(account::set_email))
        .route("/account/password", post(account::change_password))
        .route("/account/two-factor/setup", post(account::setup_two_factor))
        .route("/account/two-factor/enable", post(account::enable_two_factor))
        .route("/account/two-factor/disable", post(account::disable_two_factor))
        .route("/account/two-factor/recovery", post(account::new_recovery_codes))
        // --- recently viewed studies (each person's own) ---
        .route("/recent", get(recent::list).delete(recent::clear))
        .route("/recent/{id}", post(recent::note))
        // --- patient CD / USB export ---
        .route("/export/cd", get(export::cd_export))
        // --- portal name / login notice, readable before signing in ---
        .route("/public", get(settings::public_info))
        // --- consult ---
        .route("/studies", get(studies::search_studies))
        .route("/studies/{id}/series", get(studies::study_series))
        // --- images, tags, PDF reports, single files, NIfTI, several studies in one ZIP ---
        .route("/series/{id}/instances", get(files::series_instances))
        .route("/instances/{id}/tags", get(files::instance_tags))
        .route("/instances/{id}/preview", get(files::instance_preview))
        .route("/instances/{id}/pdf", get(files::instance_pdf))
        .route("/instances/{id}/download", get(files::instance_download))
        .route("/series/{id}/nifti", get(files::series_nifti))
        .route("/download", get(files::download_many))
        // --- delete, modify, anonymize, add a series (each switched on in Settings > Features) ---
        .route("/studies/{id}", delete(manage::delete_study))
        .route("/series/{id}", delete(manage::delete_series))
        .route("/instances/{id}", delete(manage::delete_instance))
        .route("/studies/{id}/modify", post(manage::modify_study))
        .route("/series/{id}/modify", post(manage::modify_series))
        .route("/studies/{id}/move", post(manage::move_study))
        .route("/series/{id}/move", post(manage::move_series))
        .route("/studies/{id}/anonymize", post(manage::anonymize_study))
        .route("/series/{id}/anonymize", post(manage::anonymize_series))
        .route(
            "/studies/{id}/add-series",
            post(manage::add_series).layer(DefaultBodyLimit::disable()).layer(add_series_guard),
        )
        .route("/jobs/{id}", get(manage::job_status))
        // --- query / retrieve, worklists ---
        .route("/retrieve/sources", get(retrieve::list_sources))
        .route("/retrieve/query", post(retrieve::query))
        .route("/retrieve/retrieve", post(retrieve::retrieve))
        .route("/worklists", get(worklists::list).post(worklists::create))
        .route("/worklists/{id}", put(worklists::update).delete(worklists::delete))
        .route("/series", get(studies::search_series))
        .route("/stats", get(studies::statistics))
        // --- labels on studies ---
        .route("/labels", get(labels::list_labels))
        .route("/studies/{id}/labels/{label}", put(labels::add_label).delete(labels::remove_label))
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
        .route("/studies/{id}/copy", post(copy::copy_study))
        .route("/send", post(send::send_many))
        .route("/transfers", get(send::list_transfers))
        // --- invitation links for people outside ---
        .route("/studies/{id}/share", post(shares::create_share))
        .route("/shares", get(shares::list_shares).post(shares::create_multi_share))
        .route("/shares/{id}/revoke", post(shares::revoke_share))
        .route("/shares/{id}/password", post(shares::show_password))
        .route("/shares/{id}/qr", get(shares::share_qr))
        .route("/shares/{id}", put(shares::update_share))
        // --- upload links: people outside send studies in ---
        .route("/upload-links", get(inbound::list).post(inbound::create))
        .route("/upload-links/{id}", put(inbound::update))
        .route("/upload-links/{id}/revoke", post(inbound::revoke))
        .route("/upload-links/{id}/password", post(inbound::show_password))
        .route("/upload-links/{id}/qr", get(inbound::qr))
        .route("/upload-links/{id}/studies", get(inbound::studies))
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
        // --- Configuration > System and Jobs ---
        .route("/admin/system", get(system::overview))
        .route("/admin/system/log-level", put(system::set_log_level))
        .route("/admin/system/metrics", get(system::metrics))
        .route("/admin/system/log", get(system::orthanc_log))
        .route("/admin/system/log/download", get(system::orthanc_log_download))
        .route("/admin/orthanc", get(system::raw))
        .route("/admin/jobs", get(system::list_jobs))
        .route("/admin/jobs/{id}/{action}", post(system::job_action))
        // --- Configuration > Statistics and Backups ---
        .route("/admin/stats", get(stats::overview))
        .route("/admin/backups", get(backup::list_backups).post(backup::backup_now))
        .route("/admin/backup/download", get(backup::download))
        // All JSON/API endpoints share the CSRF guard.
        .layer(middleware::from_fn(csrf_guard));

    common_routes()
        .merge(staff_viewer_routes())
        // --- web UI, compiled into the executable: nothing to deploy besides the binary ---
        .route("/", get(branding::index)) // the page, with the configured title and icon filled in
        .route("/static/app.js", get(|headers: HeaderMap| serve_static(&APP_JS, headers)))
        .nest("/api", api)
        .layer(Extension(Audience::Staff))
        .layer(middleware::from_fn(reject_internet_traffic))
        .layer(middleware::from_fn(same_origin_only))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

/// The internet-facing side: invitation links only.
fn guest_router(state: AppState) -> Router {
    // Files sent through an upload link obey the same per-file limit as staff uploads.
    let upload_guard = middleware::from_fn_with_state(state.clone(), upload_limit);
    let api = Router::new()
        .route("/public", get(settings::public_info))
        .route("/share/{token}/status", get(guest::status))
        .route("/share/{token}/unlock", post(guest::unlock))
        .route("/share/{token}/study", get(guest::study))
        .route("/share/{token}/download", get(guest::download))
        .route("/share/{token}/lock", post(guest::lock))
        // --- upload links: somebody outside sends studies in (routes/inbound.rs) ---
        .route("/upload/{token}/status", get(inbound::status))
        .route("/upload/{token}/unlock", post(inbound::unlock))
        .route("/upload/{token}/summary", get(inbound::summary))
        .route("/upload/{token}/file", post(inbound::file).layer(upload_guard))
        .route("/upload/{token}/lock", post(inbound::lock))
        .layer(middleware::from_fn(csrf_guard));

    common_routes()
        .route("/", get(guest::page))
        .route("/s/{token}", get(guest::page))
        .route("/u/{token}", get(inbound::page))
        .route("/static/share.js", get(|headers: HeaderMap| serve_static(&SHARE_JS, headers)))
        .nest("/api", api)
        .layer(Extension(Audience::Guest))
        .layer(middleware::from_fn(same_origin_only))
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
/// Browsers that accept it get a **gzip** copy (app.js: 280 KB -> about 80 KB), made once.
struct StaticFile {
    content_type: &'static str,
    body: &'static str,
    etag: OnceLock<String>,
    gzip: OnceLock<Option<(Bytes, String)>>,
}

impl StaticFile {
    const fn new(content_type: &'static str, body: &'static str) -> Self {
        Self { content_type, body, etag: OnceLock::new(), gzip: OnceLock::new() }
    }

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

    /// The gzip copy and its own ETag (`"<hash>-gz"`: the two copies are different bytes), made
    /// the first time it is asked for. `None` for small files, which would hardly shrink.
    fn gzipped(&self) -> Option<&(Bytes, String)> {
        self.gzip
            .get_or_init(|| {
                if self.body.len() < 2048 {
                    return None;
                }
                use std::io::Write;
                let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
                encoder.write_all(self.body.as_bytes()).ok()?;
                let packed = encoder.finish().ok()?;
                let etag = self.etag();
                Some((Bytes::from(packed), format!("{}-gz\"", &etag[..etag.len() - 1])))
            })
            .as_ref()
    }
}

static APP_JS: StaticFile = StaticFile::new("text/javascript; charset=utf-8", include_str!("../../static/app.js"));
static SHARE_JS: StaticFile = StaticFile::new("text/javascript; charset=utf-8", include_str!("../../static/share.js"));
static APP_CSS: StaticFile = StaticFile::new("text/css; charset=utf-8", include_str!("../../static/app.css"));
/// Spanish (Spain): English text -> translation (the keys are the English texts of app.js, share.js and the server's messages).
static I18N_ES: StaticFile =
    StaticFile::new("application/json; charset=utf-8", include_str!("../../static/i18n/es.json"));
static DEFAULT_MARK: StaticFile = StaticFile::new("image/svg+xml", include_str!("../../static/favicon.svg"));

/// Does the browser accept gzip (`Accept-Encoding: gzip, deflate, br`; not `gzip;q=0`)?
fn accepts_gzip(headers: &HeaderMap) -> bool {
    headers.get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok()).is_some_and(|v| {
        v.split(',').any(|item| {
            let mut parts = item.split(';').map(str::trim);
            parts.next().is_some_and(|name| name.eq_ignore_ascii_case("gzip"))
                && !parts.any(|p| p.strip_prefix("q=").and_then(|q| q.trim().parse::<f32>().ok()) == Some(0.0))
        })
    })
}

/// Serves an embedded file: `304` when the browser already has this exact version.
async fn serve_static(file: &'static StaticFile, headers: HeaderMap) -> Response {
    let packed = if accepts_gzip(&headers) { file.gzipped() } else { None };
    let etag = packed.map_or(file.etag(), |(_, etag)| etag.as_str());
    let browser_has_it = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(',').any(|candidate| candidate.trim().trim_start_matches("W/") == etag))
        .unwrap_or(false);
    // `no-cache` = "keep a copy, but ask before using it" (cheap thanks to the 304).
    // `Vary`: caches on the way must keep the gzip and the plain copy apart.
    let common = [(header::CACHE_CONTROL, "no-cache"), (header::VARY, "Accept-Encoding")];
    if browser_has_it {
        return (StatusCode::NOT_MODIFIED, common, [(header::ETAG, etag)]).into_response();
    }
    match packed {
        Some((bytes, _)) => (
            common,
            [(header::CONTENT_TYPE, file.content_type), (header::CONTENT_ENCODING, "gzip"), (header::ETAG, etag)],
            bytes.clone(),
        )
            .into_response(),
        None => (common, [(header::CONTENT_TYPE, file.content_type), (header::ETAG, etag)], file.body).into_response(),
    }
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

/// Paths that answer with patient data or act on it (as opposed to pages and their files).
fn is_data_path(path: &str) -> bool {
    path.starts_with("/api/") || path.starts_with("/dicom-web/") || path == "/wado"
}

/// **Only this portal's own pages may use its data addresses.** Browsers say who made a request
/// (`Sec-Fetch-Site`): another web site that links to, embeds or submits to `/api/...` is refused
/// even for a GET, which [`csrf_guard`] lets through. For example, a page elsewhere cannot make a
/// guest's browser use up a download of their invitation, or load staff images with the session
/// cookie. Programs that are not browsers (Weasis, scripts) send no such header and are unaffected;
/// opening an address typed or bookmarked by hand (`none`) is fine too.
async fn same_origin_only(request: Request, next: Next) -> Response {
    let from_elsewhere = request
        .headers()
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
        .map(|site| site.eq_ignore_ascii_case("cross-site") || site.eq_ignore_ascii_case("same-site"))
        .unwrap_or(false);
    if from_elsewhere && is_data_path(request.uri().path()) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "This address can only be used from the portal's own pages" })),
        )
            .into_response();
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
    let is_ohif = ["/ohif", "/stone-webviewer", "/volview", "/wsi"]
        .iter()
        .any(|root| path == *root || path.starts_with(&format!("{root}/")));
    let is_branding_image = path.starts_with("/branding/");
    let is_api = path.starts_with("/api/");
    let is_data = is_data_path(path);
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
    // Patient data can never be loaded into another site's page (an <img> or <script> there),
    // also by browsers too old to send `Sec-Fetch-Site` (see `same_origin_only`).
    if is_data {
        headers.insert(
            header::HeaderName::from_static("cross-origin-resource-policy"),
            HeaderValue::from_static("same-origin"),
        );
    }
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
