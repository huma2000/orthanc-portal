//! # routes/viewer.rs
//!
//! Viewing studies. Two viewers, both fed by the portal so Orthanc itself is never exposed.
//!
//! ## Staff (intranet) - Weasis and OHIF
//! * **Weasis** (desktop viewer, must be installed on the user's PC):
//!   1. The user clicks "Weasis". The browser calls `GET /api/weasis/link?study_uid=...`.
//!   2. The portal creates a short-lived **bearer token** and answers with
//!      `weasis://<url-encoded command>`:
//!
//!      ```text
//!      $dicom:rs --url "https://pacs.example.com/dicom-web"
//!                -r "studyUID=1.2.840..." -H "Authorization: Bearer <token>"
//!      ```
//!   3. The browser hands the link to Weasis, which then calls `/dicom-web/...` on THIS portal
//!      with the token.
//! * **OHIF** (web viewer, nothing to install): Orthanc's OHIF plugin (configured with
//!   `"DataSource": "dicom-web"`) serves the app at `/ohif/`; the portal relays it. OHIF runs
//!   in the portal's own origin, so it authenticates with the user's session cookie.
//!
//! ## Guests (internet, invitation link) - OHIF only
//! A person with an invitation sees the shared study in OHIF, authenticated by the cookie
//! set when they entered the link's password (`routes/guest.rs`). The very same proxy
//! serves them, but **confined to that one study**, and without ever returning whole DICOM
//! files: see `sharing.rs` for the exact rules and the reasons (other patients' data, and the
//! "N downloads" limit of the invitation). Weasis is never available to guests: it would save
//! the images on their computer.
//!
//! ## The read-only proxy
//! `/dicom-web/*`, `/wado` and `/ohif/*` are forwarded to Orthanc with **GET only**, after
//! checking who is asking. Paths are validated so nothing else of Orthanc is reachable, and
//! the DICOMweb plugin's other resources (such as queries to OTHER DICOMweb servers) are not
//! relayed.

use crate::auth::{self, authenticate_share, authenticate_viewer, AuthUser};
use crate::error::{AppError, AppResult};
use crate::permissions::Permission;
use crate::routes::Audience;
use crate::sharing;
use crate::state::AppState;
use crate::util::{is_https, is_valid_dicom_uid, public_base_url};
use axum::body::Body;
use axum::extract::{Path, Query, RawQuery, State};
use axum::http::{header, HeaderMap};
use axum::response::{Redirect, Response};
use axum::{Extension, Json};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
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
    if !state.settings.get().weasis_enabled {
        return Err(AppError::Denied("Weasis is switched off (Settings > General > Viewers and uploads)".into()));
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
    audience: Audience,
    area: Area,
    path: &str,
    query: Option<String>,
) -> AppResult<Response> {
    // Who is asking? Staff: the session cookie or a Weasis token, with the `visualize`
    // permission. Guest: the cookie set by an invitation's password; the viewer then only
    // gets what belongs to that invitation's study (checked below).
    let guest_study_uid: Option<String> = match audience {
        Audience::Staff => {
            authenticate_viewer(state, headers)?;
            None
        }
        Audience::Guest => Some(authenticate_share(state, headers)?.study_uid),
    };

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
    // A guest may only read THEIR study, and never whole DICOM files: the only way to get the
    // files is the counted ZIP download (see sharing.rs for the rules and the reasons).
    if let Some(study_uid) = &guest_study_uid {
        let allowed = match area {
            Area::DicomWeb => sharing::guest_may_read_dicomweb(path, query.as_deref(), study_uid),
            Area::Wado => sharing::guest_may_read_wado(query.as_deref(), study_uid),
            Area::Ohif => true, // the viewer application itself (static files)
        };
        if !allowed {
            return Err(AppError::Denied("This link only gives access to the shared study.".into()));
        }
    }

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
    Extension(audience): Extension<Audience>,
    headers: HeaderMap,
    Path(path): Path<String>,
    RawQuery(query): RawQuery,
) -> AppResult<Response> {
    forward(&state, &headers, audience, Area::DicomWeb, &path, query).await
}

/// `GET /wado?requestType=WADO&...`
pub async fn wado_proxy(
    State(state): State<AppState>,
    Extension(audience): Extension<Audience>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> AppResult<Response> {
    forward(&state, &headers, audience, Area::Wado, "", query).await
}

/// `GET /ohif/{*path}` - the OHIF web application and its pages.
pub async fn ohif_proxy(
    State(state): State<AppState>,
    Extension(audience): Extension<Audience>,
    headers: HeaderMap,
    Path(path): Path<String>,
    RawQuery(query): RawQuery,
) -> AppResult<Response> {
    forward(&state, &headers, audience, Area::Ohif, &path, query).await
}

/// `GET /ohif/` - OHIF's own study list.
pub async fn ohif_index(
    State(state): State<AppState>,
    Extension(audience): Extension<Audience>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> AppResult<Response> {
    forward(&state, &headers, audience, Area::Ohif, "", query).await
}

/// `GET /ohif` -> `/ohif/` (OHIF's relative links need the trailing slash).
pub async fn ohif_redirect() -> Redirect {
    Redirect::permanent("/ohif/")
}
