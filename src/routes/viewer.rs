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
//! checking who is asking.
//!
//! ## Several Orthanc servers
//! A staff viewer's DICOMweb and WADO requests go to the server of the study they name, among the
//! servers the person may use (`OrthancPool::for_study`). The viewer buttons pass the server the
//! study was opened from: `server=` on the OHIF page and on the Weasis link. The viewers' own
//! files (OHIF, Stone, VolView: programs, no patient data) come from the default server. A
//! guest reads the server of their link only. Paths are validated so nothing else of Orthanc is reachable, and
//! the DICOMweb plugin's other resources (such as queries to OTHER DICOMweb servers) are not
//! relayed.

use crate::auth::{self, authenticate_share, authenticate_viewer, AuthUser};
use crate::error::{AppError, AppResult};
use crate::orthanc::OrthancClient;
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
    /// The key of the Orthanc server it is opened from (empty = found by its UID).
    #[serde(default)]
    server: String,
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
    if !params.server.is_empty() {
        state.orthancs.note_route(&user, &params.study_uid, &params.server);
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
    /// The other viewer applications of Orthanc's plugins (staff only): Stone Web Viewer,
    /// VolView and the whole-slide-imaging viewer (whose tiles are under `/wsi/` too).
    App(&'static str),
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
    // Which Orthanc answers: a guest's link is on one server (its studies' IDs say which);
    // staff, the server of the study the request names (see the module documentation).
    let (orthanc, guest): (OrthancClient, Option<(i64, Vec<String>)>) = match audience {
        Audience::Staff => {
            let user = authenticate_viewer(state, headers)?;
            // A search for studies that names none (OHIF's list of the patient's other studies):
            // every server of the person is asked, and the answers are merged.
            if area == Area::DicomWeb && path == "studies" && study_uid_of(area, path, query.as_deref()).is_none() {
                let usable = state.orthancs.usable(&user);
                if usable.len() > 1 {
                    return merged_study_search(&usable, headers, query.as_deref()).await;
                }
            }
            let client = match area {
                Area::DicomWeb | Area::Wado => {
                    let uid = study_uid_of(area, path, query.as_deref());
                    state.orthancs.for_study(&user, uid.as_deref()).await?
                }
                // Whole-slide images: the tiles are images of the default server.
                Area::App("/wsi") => state.orthancs.default_for(&user)?,
                Area::Ohif | Area::App(_) => {
                    // The OHIF page of a study opened from a server: its requests go there.
                    if area == Area::Ohif {
                        note_opened_from(state, &user, query.as_deref());
                    }
                    state.orthancs.stored(crate::settings::MAIN_SERVER)?
                }
            };
            (client, None)
        }
        Audience::Guest => {
            let share = authenticate_share(state, headers)?;
            // The server stored with the link (checked when the link was made).
            let (server, _) = state.orthancs.resolve_stored(&share.study_id)?;
            (server, Some((share.id, share.study_uids())))
        }
    };

    // Where the request goes inside Orthanc.
    let mut upstream = match area {
        Area::Wado => "/wado".to_string(),
        Area::DicomWeb | Area::Ohif | Area::App(_) => {
            if !path.is_empty() && !is_safe_path(path) {
                return Err(AppError::BadRequest("Invalid path".into()));
            }
            // The DICOMweb plugin also offers `servers/...` (queries to OTHER DICOMweb
            // servers configured in Orthanc), `info`, etc. A viewer only ever needs the
            // standard resources of THIS archive, so nothing else is relayed.
            if area == Area::DicomWeb && !matches!(path.split('/').next(), Some("studies" | "series" | "instances")) {
                return Err(AppError::NotFound);
            }
            let root = match area {
                Area::DicomWeb => "/dicom-web",
                Area::App(root) => root,
                _ => "/ohif",
            };
            format!("{root}/{path}")
        }
    };
    // A guest may only read THEIR study, and never whole DICOM files: the only way to get the
    // files is the counted ZIP download (see sharing.rs for the rules and the reasons).
    if let Some((share_id, study_uids)) = &guest {
        let denied = || AppError::Denied("This link only gives access to the shared study.".into());
        let allowed = match area {
            Area::DicomWeb => sharing::guest_may_read_dicomweb(path, query.as_deref(), study_uids),
            Area::Wado => sharing::guest_may_read_wado(query.as_deref(), study_uids),
            Area::Ohif => true,    // the viewer application itself (static files)
            Area::App(_) => false, // guests only get OHIF
        };
        if !allowed {
            return Err(denied());
        }
        // The series and image named must really be part of the shared study.
        let member = match area {
            Area::DicomWeb => sharing::dicomweb_member(path).map_err(|_| denied())?,
            Area::Wado => Some(sharing::wado_member(query.as_deref()).ok_or_else(denied)?),
            _ => None,
        };
        if let Some(member) = member {
            if !belongs_to_study(state, &orthanc, *share_id, &member).await? {
                return Err(denied());
            }
        }
    }

    if let Some(q) = query.filter(|q| !q.is_empty()) {
        upstream.push('?');
        upstream.push_str(&q);
    }

    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    let proto = if is_https(headers) { "https" } else { "http" };
    let answer = orthanc.proxy_get(&upstream, headers, host, proto).await?;

    let cache_control = match area {
        Area::Ohif | Area::App(_) if is_static_asset(path) => "private, max-age=3600",
        Area::Ohif | Area::App(_) => "private, no-cache",
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

/// `GET /dicom-web/studies?...` on several servers: each answer (a DICOM JSON list) is read and the
/// lists are joined, the same study (StudyInstanceUID `0020000D`) given once. A server that does
/// not answer is left out, unless none does.
async fn merged_study_search(
    servers: &[(crate::servers::ServerInfo, OrthancClient)],
    headers: &HeaderMap,
    query: Option<&str>,
) -> AppResult<Response> {
    let mut upstream = "/dicom-web/studies".to_string();
    if let Some(q) = query.filter(|q| !q.is_empty()) {
        upstream.push('?');
        upstream.push_str(q);
    }
    // Only `Accept` is passed on: the answers must come back plain to be read here.
    let mut accept = HeaderMap::new();
    if let Some(value) = headers.get(header::ACCEPT) {
        accept.insert(header::ACCEPT, value.clone());
    }
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    let proto = if is_https(headers) { "https" } else { "http" };
    let answers = futures_util::future::join_all(servers.iter().map(|(info, client)| {
        let (upstream, accept) = (&upstream, &accept);
        async move {
            let ask = async {
                let answer = client.proxy_get(upstream, accept, host, proto).await?;
                if answer.status() == axum::http::StatusCode::NO_CONTENT {
                    return Ok(Vec::new());
                }
                if !answer.status().is_success() {
                    return Err(AppError::Upstream(format!("HTTP {}", answer.status().as_u16())));
                }
                // A study list is small: an answer announcing more than this is refused, and so
                // is one that turns out bigger.
                const MAX_ANSWER: u64 = 16 * 1024 * 1024;
                if answer.content_length().is_some_and(|n| n > MAX_ANSWER) {
                    return Err(AppError::Upstream("the answer is too big".into()));
                }
                let bytes = answer.bytes().await.map_err(|e| AppError::Upstream(e.to_string()))?;
                if bytes.len() as u64 > MAX_ANSWER {
                    return Err(AppError::Upstream("the answer is too big".into()));
                }
                if bytes.is_empty() {
                    return Ok(Vec::new());
                }
                serde_json::from_slice::<Vec<Value>>(&bytes).map_err(|e| AppError::Upstream(e.to_string()))
            };
            let result = tokio::time::timeout(crate::routes::studies::SERVER_TIMEOUT, ask)
                .await
                .unwrap_or_else(|_| Err(AppError::Upstream("did not answer in time".into())));
            (info.key.clone(), result)
        }
    }))
    .await;
    let mut studies: Vec<Value> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let mut first_error = None;
    let mut answered = false;
    for (server, result) in answers {
        match result {
            Ok(list) => {
                answered = true;
                for study in list {
                    let uid = study["0020000D"]["Value"][0].as_str().unwrap_or_default().to_string();
                    if !uid.is_empty() && seen.contains(&uid) {
                        continue;
                    }
                    seen.push(uid);
                    studies.push(study);
                }
            }
            Err(e) => {
                tracing::warn!("viewer study search: Orthanc server '{server}' did not answer: {e}");
                first_error = first_error.or(Some(e));
            }
        }
    }
    if let (false, Some(e)) = (answered, first_error) {
        return Err(e);
    }
    let body = serde_json::to_vec(&studies).map_err(|e| AppError::Internal(e.to_string()))?;
    Response::builder()
        .header(header::CONTENT_TYPE, "application/dicom+json")
        .header(header::CACHE_CONTROL, "private, no-store")
        .body(Body::from(body))
        .map_err(|e| AppError::Internal(format!("cannot build proxy response: {e}")))
}

/// The values of one parameter of a query string, decoded.
fn query_values(query: Option<&str>, name: &str) -> Vec<String> {
    query
        .unwrap_or_default()
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .filter(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| {
            percent_encoding::percent_decode_str(&value.replace('+', " ")).decode_utf8_lossy().into_owned()
        })
        .collect()
}

/// The StudyInstanceUID a viewer request names: `studies/{uid}/...` in a DICOMweb path, or the
/// `StudyInstanceUID` (`0020000D`) of a DICOMweb search, or `studyUID` of a WADO-URI request.
fn study_uid_of(area: Area, path: &str, query: Option<&str>) -> Option<String> {
    let from_query = |names: &[&str]| names.iter().flat_map(|n| query_values(query, n)).next();
    match area {
        Area::DicomWeb => {
            let mut segments = path.split('/');
            match (segments.next(), segments.next()) {
                (Some("studies"), Some(uid)) if !uid.is_empty() => Some(uid.to_string()),
                _ => from_query(&["StudyInstanceUID", "0020000D"]),
            }
        }
        Area::Wado => from_query(&["studyUID"]),
        _ => None,
    }
    .filter(|uid| is_valid_dicom_uid(uid))
}

/// `/ohif/viewer?StudyInstanceUIDs=...&server=archive`: the studies are read from that server.
fn note_opened_from(state: &AppState, user: &crate::db::User, query: Option<&str>) {
    let Some(server) = query_values(query, "server").into_iter().next() else { return };
    for uids in query_values(query, "StudyInstanceUIDs") {
        for uid in uids.split(',').map(str::trim).filter(|u| is_valid_dicom_uid(u)) {
            state.orthancs.note_route(user, uid, &server);
        }
    }
}

/// Does the series (and image) a guest names belong to the study written before it? Asked to
/// Orthanc with `/tools/find` on the UIDs (all checked to be plain UIDs, so no wildcard), and
/// remembered for a few minutes per invitation.
async fn belongs_to_study(
    state: &AppState,
    orthanc: &OrthancClient,
    share_id: i64,
    member: &sharing::Member,
) -> AppResult<bool> {
    let now = crate::util::now_unix();
    if state.guest_members.knows(share_id, member, now) {
        return Ok(true);
    }
    // An image of a series: a viewer reads EVERY image of the series it shows (hundreds for a CT),
    // so ask once for all the images of that series in the study and remember each of them.
    if let (Some(series), Some(_)) = (&member.series, &member.instance) {
        let listed = sharing::Member {
            study: member.study.clone(),
            series: Some(series.clone()),
            instance: Some(sharing::WHOLE_SERIES.into()),
        };
        if !state.guest_members.knows(share_id, &listed, now) {
            let query = json!({ "StudyInstanceUID": member.study, "SeriesInstanceUID": series });
            let found = orthanc.find(&json!({ "Level": "Instance", "Query": query, "Expand": true })).await?;
            if found.is_empty() {
                return Ok(false); // the series is not part of the study
            }
            let images =
                found.iter().filter_map(|i| i["MainDicomTags"]["SOPInstanceUID"].as_str()).map(|uid| sharing::Member {
                    study: member.study.clone(),
                    series: Some(series.clone()),
                    instance: Some(uid.into()),
                });
            state.guest_members.remember_all(share_id, images.chain([listed]), now);
            if state.guest_members.knows(share_id, member, now) {
                return Ok(true);
            }
        }
        // Not in the list (an image added since): asked on its own below.
    }
    let mut tags = serde_json::Map::new();
    tags.insert("StudyInstanceUID".into(), json!(member.study));
    if let Some(series) = &member.series {
        tags.insert("SeriesInstanceUID".into(), json!(series));
    }
    if let Some(instance) = &member.instance {
        tags.insert("SOPInstanceUID".into(), json!(instance));
    }
    let level = if member.instance.is_some() { "Instance" } else { "Series" };
    let found = orthanc.find(&json!({ "Level": level, "Query": tags, "Limit": 1 })).await?;
    if found.is_empty() {
        return Ok(false);
    }
    state.guest_members.remember(share_id, member, now);
    Ok(true)
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

/// `/stone-webviewer/*`, `/volview/*` and `/wsi/*`: the other viewers, for staff with `visualize`.
pub async fn app_proxy(
    State(state): State<AppState>,
    Extension(audience): Extension<Audience>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    RawQuery(query): RawQuery,
) -> AppResult<Response> {
    let full = uri.path();
    let (root, rest) = ["/stone-webviewer", "/volview", "/wsi"]
        .into_iter()
        .find_map(|root| full.strip_prefix(root).map(|rest| (root, rest.trim_start_matches('/'))))
        .ok_or(AppError::NotFound)?;
    let settings = state.settings.get();
    let mode = match root {
        "/stone-webviewer" => settings.stone_viewer,
        "/volview" => settings.volview_viewer,
        _ => settings.wsi_viewer,
    };
    if mode == crate::settings::OhifMode::Off {
        return Err(AppError::NotFound);
    }
    let rest = if rest.is_empty() { "index.html" } else { rest };
    forward(&state, &headers, audience, Area::App(root), rest, query).await
}
