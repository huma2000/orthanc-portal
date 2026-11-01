//! # routes/viewer.rs
//!
//! Viewing studies. Two viewers are supported; both are fed by the portal, so
//! Orthanc itself is never exposed. Both need the `visualize` permission.
//!
//! **Over the internet only the browser viewer (OHIF) is allowed.** Weasis saves the
//! images on the user's computer, which would let patient images leave the building, so:
//!  * the Weasis link route is not registered on the remote listener (and is refused for
//!    requests carrying Cloudflare headers);
//!  * a Weasis bearer token is ignored over the internet, even a genuine one issued on the
//!    intranet (`auth::authenticate_viewer` accepts only the session cookie there);
//!  * unless switched off in Settings, requests that return WHOLE DICOM files are refused
//!    over the internet: a viewer only needs metadata and pixel frames, so this closes the
//!    "type the DICOMweb address and save the .dcm" route (`is_whole_dicom_retrieval`).
//!
//! On the intranet both viewers work without restriction.
//!
//! ## 1. Weasis (desktop viewer, must be installed on the user's PC)
//! 1. The user clicks "Weasis". The browser calls `GET /api/weasis/link?study_uid=...`.
//! 2. The portal creates a short-lived **bearer token** and answers with
//!    `weasis://<url-encoded command>` where the command is:
//!
//!    ```text
//!    $dicom:rs --url "https://pacs.example.com/dicom-web"
//!              -r "studyUID=1.2.840..." -H "Authorization: Bearer <token>"
//!    ```
//! 3. The browser hands the link to Weasis (the `weasis://` protocol handler
//!    registered by the Weasis installer). Weasis then calls `/dicom-web/...` on
//!    THIS portal, sending the token.
//!
//! ## 2. OHIF (web viewer, nothing to install)
//! Orthanc's **OHIF plugin** (configured with `"DataSource": "dicom-web"`) serves
//! the OHIF web application at `/ohif/`. The portal relays it, together with the
//! DICOMweb data it reads, so the viewer works in a normal browser tab:
//! `/ohif/viewer?StudyInstanceUIDs=<uid>`. OHIF runs inside the portal's own
//! origin, so it authenticates with the user's session cookie.
//!
//! ## The read-only proxy
//! `/dicom-web/*`, `/wado` and `/ohif/*` are forwarded to Orthanc with **GET only**,
//! after checking the token/cookie and the permission. A viewer can therefore never
//! modify the PACS, and paths are validated so nothing else of Orthanc is reachable.

use crate::auth::{self, authenticate_viewer, AuthUser};
use crate::error::{AppError, AppResult};
use crate::permissions::{AccessScope, Permission};
use crate::state::AppState;
use crate::util::{is_https, is_valid_dicom_uid, public_base_url};
use axum::body::Body;
use axum::extract::{Path, Query, RawQuery, State};
use axum::http::{header, HeaderMap};
use axum::response::{Redirect, Response};
use axum::Json;
use percent_encoding::{percent_decode_str, utf8_percent_encode, NON_ALPHANUMERIC};
use serde::Deserialize;
use serde_json::{json, Value};

// ======================================================================= Weasis

#[derive(Deserialize)]
pub struct WeasisParams {
    /// DICOM StudyInstanceUID of the study to open.
    study_uid: String,
}

/// Builds the `weasis://` URI that opens one study in the Weasis viewer.
pub async fn weasis_link(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    headers: HeaderMap,
    Query(params): Query<WeasisParams>,
) -> AppResult<Json<Value>> {
    user.require(Permission::Visualize)?;

    // Weasis saves the images on the user's computer. Over the internet that would put
    // patient images on machines nobody controls, so there the browser viewer (OHIF) is the
    // only way to look at them. (This route is not even registered on the remote listener;
    // this check covers requests that carry Cloudflare headers on the intranet port.)
    if user.scope == AccessScope::Remote {
        return Err(AppError::Denied(
            "Weasis is not available over the internet because it downloads the images to the computer. \
             Use the browser viewer instead."
                .into(),
        ));
    }

    // The UID goes inside a quoted command line: only digits and dots are allowed.
    if !is_valid_dicom_uid(&params.study_uid) {
        return Err(AppError::BadRequest("Invalid StudyInstanceUID".into()));
    }

    // URL of OUR /dicom-web proxy as seen from the user's computer (intranet
    // address or Cloudflare hostname, whichever the user is using right now).
    let settings = state.settings.get();
    let base = public_base_url(&headers, &settings)?;
    let dicomweb_url = format!("{base}/dicom-web");

    let ttl_minutes = settings.weasis_token_minutes;
    // Reuses the user's current token when it still has plenty of life left, so opening
    // many studies does not pile up tokens (they all grant the same read-only access).
    let token =
        state.weasis_tokens.issue_or_reuse(user.id, (ttl_minutes * 60) as i64, auth::MAX_WEASIS_TOKENS_PER_USER);

    // Weasis command line (see the module documentation).
    let command = format!(
        "$dicom:rs --url \"{dicomweb_url}\" -r \"studyUID={}\" -H \"Authorization: Bearer {token}\"",
        params.study_uid
    );
    // The Weasis documentation asks to URL-encode the command that follows `weasis://`.
    let uri = format!("weasis://{}", utf8_percent_encode(&command, NON_ALPHANUMERIC));

    state.db.audit(&user.username, "open_weasis", &params.study_uid);
    Ok(Json(json!({
        "uri": uri,
        "dicomweb_url": dicomweb_url,
        "expires_in_minutes": ttl_minutes,
    })))
}

// ================================================================ viewer proxy

/// The three parts of Orthanc the viewers may read.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Area {
    /// DICOMweb plugin: QIDO-RS / WADO-RS (Weasis and OHIF).
    DicomWeb,
    /// WADO-URI (`/wado?requestType=WADO&...`), used by OHIF for some requests.
    Wado,
    /// Files of the OHIF web application itself.
    Ohif,
}

/// Accepts only harmless relative paths: `studies/{uid}/series/...`, `metadata`,
/// `assets/app.bundle.js`... Rejects `..`, empty segments and any unusual
/// character, so the proxy can never be used to reach other Orthanc endpoints.
fn is_whole_dicom_retrieval(path: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    matches!(
        parts.as_slice(),
        ["studies", _]
            | ["series", _]
            | ["instances", _]
            | ["studies", _, "series", _]
            | ["studies", _, "series", _, "instances", _]
    )
}

/// WADO-URI (`/wado?requestType=WADO&...`) answers with a whole DICOM file unless a picture
/// format is requested with `contentType=image/jpeg` (or png...). True only when every
/// `contentType` given is a picture type and there is at least one.
fn is_rendered_wado_request(query: Option<&str>) -> bool {
    let Some(query) = query else { return false };
    let types: Vec<String> = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .filter(|(key, _)| key.eq_ignore_ascii_case("contenttype"))
        .flat_map(|(_, value)| {
            percent_decode_str(value)
                .decode_utf8_lossy()
                .to_ascii_lowercase()
                .split(',')
                .map(|t| t.trim().to_string())
                .collect::<Vec<_>>()
        })
        .collect();
    !types.is_empty() && types.iter().all(|t| t.starts_with("image/"))
}

fn is_safe_path(path: &str) -> bool {
    path.len() <= 512
        && path.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        })
}

/// Files of the OHIF app that never change under the same name (content-hashed
/// bundles, fonts, images, WASM codecs) may be cached by the browser for a while.
/// HTML pages and `app-config.js` must always be re-fetched.
fn is_static_asset(path: &str) -> bool {
    const EXTENSIONS: [&str; 10] = [".js", ".css", ".wasm", ".woff", ".woff2", ".ttf", ".png", ".jpg", ".svg", ".ico"];
    !path.ends_with("app-config.js") && EXTENSIONS.iter().any(|ext| path.ends_with(ext))
}

/// Response headers copied from Orthanc to the viewer. Everything else (cookies,
/// server banners, redirects with internal addresses...) is dropped on purpose.
const RELAYED_HEADERS: [&str; 9] = [
    "content-type",
    "content-encoding",
    "content-length",
    "content-range",
    "accept-ranges",
    "etag",
    "last-modified",
    "cross-origin-opener-policy",
    "cross-origin-embedder-policy",
];

/// Authenticates, validates the path, forwards the GET to Orthanc and streams the
/// answer back unchanged.
async fn forward(
    state: &AppState,
    headers: &HeaderMap,
    area: Area,
    path: &str,
    query: Option<String>,
) -> AppResult<Response> {
    // Bearer token (Weasis, intranet only) or session cookie (OHIF) + the `visualize` permission.
    let user = authenticate_viewer(state, headers)?;

    // Over the internet a viewer needs metadata and pixel frames, never WHOLE DICOM files.
    // Refusing those closes the "type the DICOMweb address and save the .dcm" route.
    if user.scope == AccessScope::Remote && state.settings.get().remote_block_dicom_files {
        let whole_file = match area {
            Area::DicomWeb => is_whole_dicom_retrieval(path),
            Area::Wado => !is_rendered_wado_request(query.as_deref()),
            Area::Ohif => false,
        };
        if whole_file {
            return Err(AppError::Denied(
                "Downloading whole DICOM files is not allowed over the internet. Images can be viewed in the browser viewer."
                    .into(),
            ));
        }
    }

    // Where the request goes inside Orthanc.
    let mut upstream = match area {
        Area::Wado => "/wado".to_string(),
        Area::DicomWeb | Area::Ohif => {
            if !path.is_empty() && !is_safe_path(path) {
                return Err(AppError::BadRequest("Invalid path".into()));
            }
            // The DICOMweb plugin also offers `servers/...` (queries to OTHER DICOMweb
            // servers configured in Orthanc), `info`, etc. A viewer only ever needs the
            // standard resources of THIS archive, so nothing else is relayed.
            if area == Area::DicomWeb && !matches!(path.split('/').next(), Some("studies" | "series" | "instances")) {
                return Err(AppError::NotFound);
            }
            let root = if area == Area::DicomWeb { "/dicom-web" } else { "/ohif" };
            format!("{root}/{path}")
        }
    };
    if let Some(q) = query.filter(|q| !q.is_empty()) {
        upstream.push('?');
        upstream.push_str(&q);
    }

    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    let proto = if is_https(headers) { "https" } else { "http" };
    let answer = state.orthanc.proxy_get(&upstream, headers, host, proto).await?;

    let cache_control = match area {
        Area::Ohif if is_static_asset(path) => "private, max-age=3600",
        Area::Ohif => "private, no-cache",
        // Images and patient data: never cached by browsers or proxies.
        Area::DicomWeb | Area::Wado => "private, no-store",
    };

    let mut builder = Response::builder().status(answer.status()).header(header::CACHE_CONTROL, cache_control);
    for name in RELAYED_HEADERS {
        if let Some(value) = answer.headers().get(name) {
            builder = builder.header(name, value);
        }
    }
    builder
        .body(Body::from_stream(answer.bytes_stream()))
        .map_err(|e| AppError::Internal(format!("cannot build proxy response: {e}")))
}

/// `GET /dicom-web/{*path}`
pub async fn dicomweb_proxy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(path): Path<String>,
    RawQuery(query): RawQuery,
) -> AppResult<Response> {
    forward(&state, &headers, Area::DicomWeb, &path, query).await
}

/// `GET /wado?requestType=WADO&...`
pub async fn wado_proxy(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> AppResult<Response> {
    forward(&state, &headers, Area::Wado, "", query).await
}

/// `GET /ohif/{*path}` - the OHIF web application and its pages.
pub async fn ohif_proxy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(path): Path<String>,
    RawQuery(query): RawQuery,
) -> AppResult<Response> {
    forward(&state, &headers, Area::Ohif, &path, query).await
}

/// `GET /ohif/` - OHIF's own study list.
pub async fn ohif_index(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> AppResult<Response> {
    forward(&state, &headers, Area::Ohif, "", query).await
}

/// `GET /ohif` -> `/ohif/` (OHIF's relative links need the trailing slash).
pub async fn ohif_redirect() -> Redirect {
    Redirect::permanent("/ohif/")
}
