//! # orthanc.rs
//!
//! A small client for the parts of Orthanc's REST API this portal needs. The
//! portal targets **recent Orthanc releases only (1.12.5 or newer)** and relies
//! on their "extended find" (`OrderBy`, `RequestedTags`, parent filters), so
//! there is no compatibility code for older servers.
//!
//! | Portal feature          | Orthanc endpoint                                      |
//! |-------------------------|-------------------------------------------------------|
//! | Search studies / series | `POST /tools/find`                                    |
//! | Study / series detail   | `GET /studies/{id}`, `GET /studies/{id}/series`       |
//! | Upload                  | `POST /instances`                                     |
//! | ZIP download            | `GET /studies/{id}/archive` or `/media` (+ series)    |
//! | Send to a DICOM node    | `GET /modalities?expand`, `POST /modalities/{id}/store`, `GET /jobs/{id}` |
//! | Preview thumbnails      | `GET /series/{id}`, `GET /instances/{id}/rendered?width=...` |
//! | Weasis / OHIF viewers   | `GET /dicom-web/...`, `/wado`, `/ohif/...` (read-only proxy) |
//! | Feature detection       | `GET /system`, `GET /plugins`                         |
//! | Manage DICOM nodes (admin) | `PUT/DELETE /modalities/{id}`, `POST /modalities/{id}/echo` |
//!
//! The portal is the ONLY thing that talks to Orthanc, using the credentials
//! from the configuration. Orthanc itself never has to be exposed to the
//! intranet or the internet.

use crate::error::AppError;
use crate::models::ModalityNode;
use crate::settings::{OhifMode, Settings};
use axum::body::Bytes;
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use reqwest::header::{self, HeaderMap};
use reqwest::{Client, Method, RequestBuilder, Response, StatusCode};
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

/// Characters of a DICOM node alias that are written as they are in a URL path
/// (letters and digits always are). Everything else is percent-encoded.
const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

/// Timeout for ordinary JSON calls. Streaming calls (upload/download/viewer
/// proxy) have NO overall timeout, otherwise big transfers would be cut.
const JSON_TIMEOUT: Duration = Duration::from_secs(60);

/// Oldest supported Orthanc: the release that introduced the extended find API.
pub const MIN_VERSION: (u32, u32, u32) = (1, 12, 5);

/// How long the list of loaded plugins is remembered.
const PLUGINS_TTL: Duration = Duration::from_secs(60);

/// How long we wait for ANY data from Orthanc during a streamed transfer (upload, ZIP,
/// viewer proxy) before giving up. It is not a limit on the whole transfer, which may
/// take hours, only on a connection that has gone completely silent.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Plugins reported by `GET /plugins` and when they were read.
type PluginsCache = Arc<Mutex<Option<(Instant, Vec<String>)>>>;

/// Result of the start-up compatibility check ([`OrthancClient::info`]).
#[derive(Debug)]
pub struct OrthancInfo {
    pub version: String,
    /// `true` when the version is recent enough AND the index database reports
    /// the `HasExtendedFind` capability (needed for ordering and filtering).
    pub supported: bool,
}

/// Which kind of resource an archive is built from.
#[derive(Debug, Clone, Copy)]
pub enum ArchiveKind {
    Study,
    Series,
}

/// Where and how to reach Orthanc. It can be replaced while the program runs
/// (when an administrator saves new settings), see [`OrthancClient::reconfigure`].
#[derive(Clone)]
struct Endpoint {
    base_url: String,
    credentials: Option<(String, String)>,
}

impl Endpoint {
    fn from_settings(settings: &Settings) -> Self {
        let credentials = if settings.orthanc_username.is_empty() {
            None
        } else {
            Some((settings.orthanc_username.clone(), settings.orthanc_password.0.clone()))
        };
        Self { base_url: settings.orthanc_url.clone(), credentials }
    }
}

/// Outcome of "Test connection" in the Settings screen.
#[derive(Debug, Serialize)]
pub struct Diagnosis {
    /// Reachable, authenticated AND recent enough.
    pub ok: bool,
    /// One sentence explaining the result, shown to the administrator.
    pub message: String,
    pub version: Option<String>,
    pub supported: bool,
    /// Names of the loaded plugins (`GET /plugins`).
    pub plugins: Vec<String>,
    pub dicomweb: bool,
    pub ohif: bool,
    /// Number of DICOM nodes (modalities) known to Orthanc.
    pub modalities: usize,
}

#[derive(Clone)]
pub struct OrthancClient {
    http: Client,
    /// Current address and credentials; swapped when the settings change.
    endpoint: Arc<RwLock<Endpoint>>,
    plugins_cache: PluginsCache,
}

/// Reads the leading digits of "10-rc1" -> 10.
fn leading_number(s: &str) -> Option<u32> {
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

impl OrthancClient {
    fn build(endpoint: Endpoint) -> Result<Self, AppError> {
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .read_timeout(STREAM_IDLE_TIMEOUT)
            .user_agent(concat!("orthanc-portal/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| AppError::Internal(format!("cannot build HTTP client: {e}")))?;
        Ok(Self { http, endpoint: Arc::new(RwLock::new(endpoint)), plugins_cache: Arc::new(Mutex::new(None)) })
    }

    /// The client used by the whole portal, configured from the saved settings.
    pub fn new(settings: &Settings) -> Result<Self, AppError> {
        Self::build(Endpoint::from_settings(settings))
    }

    /// A throw-away client for "Test connection": it can try values the
    /// administrator has typed but not saved yet.
    pub fn for_test(url: &str, username: &str, password: &str) -> Result<Self, AppError> {
        let credentials = if username.is_empty() { None } else { Some((username.to_string(), password.to_string())) };
        Self::build(Endpoint { base_url: url.trim().trim_end_matches('/').to_string(), credentials })
    }

    /// Points the running client at another Orthanc (or other credentials).
    /// In-flight requests finish against the old one; new requests use the new one.
    pub fn reconfigure(&self, settings: &Settings) {
        *self.endpoint.write().unwrap_or_else(|e| e.into_inner()) = Endpoint::from_settings(settings);
        *self.plugins_cache.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Creates a request to `{Orthanc address}{path}` with the Orthanc credentials attached.
    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        let endpoint = self.endpoint.read().unwrap_or_else(|e| e.into_inner()).clone();
        let builder = self.http.request(method, format!("{}{}", endpoint.base_url, path));
        match endpoint.credentials {
            Some((user, pass)) => builder.basic_auth(user, Some(pass)),
            None => builder,
        }
    }

    /// Turns a non-2xx Orthanc answer into an [`AppError`].
    async fn check(resp: Response) -> Result<Response, AppError> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        if status == StatusCode::NOT_FOUND {
            return Err(AppError::NotFound);
        }
        let body = resp.text().await.unwrap_or_default();
        // Orthanc's own wording is useful to the administrator but it is shown to every
        // user who triggers the error: keep it short and free of control characters.
        let short: String = body.chars().filter(|c| !c.is_control()).take(160).collect();
        Err(AppError::Upstream(format!("HTTP {status} {short}")))
    }

    // ------------------------------------------------------ basic JSON calls

    /// `GET path` and parse the answer as JSON.
    pub async fn get_json(&self, path: &str) -> Result<Value, AppError> {
        let resp = self.request(Method::GET, path).timeout(JSON_TIMEOUT).send().await?;
        Ok(Self::check(resp).await?.json::<Value>().await?)
    }

    /// `POST /tools/find` - returns the array of matching resources.
    pub async fn find(&self, body: &Value) -> Result<Vec<Value>, AppError> {
        let resp = self.request(Method::POST, "/tools/find").json(body).timeout(JSON_TIMEOUT).send().await?;
        match Self::check(resp).await?.json::<Value>().await? {
            Value::Array(items) => Ok(items),
            _ => Err(AppError::Upstream("unexpected answer from /tools/find".into())),
        }
    }

    // ------------------------------------------------- version / capabilities

    /// Reads `GET /system` and tells whether this Orthanc is recent enough.
    pub async fn info(&self) -> Result<OrthancInfo, AppError> {
        let system = self.get_json("/system").await?;
        let version = system["Version"].as_str().unwrap_or("unknown").to_string();

        let has_extended_find = system["Capabilities"]
            .as_array()
            .map(|caps| caps.iter().any(|c| c.as_str() == Some("HasExtendedFind")))
            .unwrap_or(false);

        // "1.12.10" -> (1, 12, 10). Development builds ("mainline") count as new enough.
        let mut parts = version.split('.').map(leading_number);
        let version_ok = match (parts.next().flatten(), parts.next().flatten(), parts.next().flatten()) {
            (Some(major), Some(minor), patch) => (major, minor, patch.unwrap_or(0)) >= MIN_VERSION,
            _ => true,
        };
        Ok(OrthancInfo { version, supported: version_ok && has_extended_find })
    }

    /// Is a plugin whose name contains `name` loaded? (`GET /plugins`, cached 60 s.)
    async fn plugin_loaded(&self, name: &str) -> bool {
        // Look in the cache first. The lock is released before any `.await`.
        {
            let cache = self.plugins_cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((read_at, plugins)) = cache.as_ref() {
                if read_at.elapsed() < PLUGINS_TTL {
                    return plugins.iter().any(|p| p.to_lowercase().contains(name));
                }
            }
        }
        let plugins: Vec<String> = match self.get_json("/plugins").await {
            Ok(Value::Array(items)) => items.iter().filter_map(|v| v.as_str()).map(str::to_string).collect(),
            _ => {
                // Orthanc does not answer. Remember "no plugins" for a few seconds only, so
                // that every page load does not wait for the connection timeout again.
                let backdated =
                    Instant::now().checked_sub(PLUGINS_TTL - Duration::from_secs(10)).unwrap_or_else(Instant::now);
                *self.plugins_cache.lock().unwrap_or_else(|e| e.into_inner()) = Some((backdated, Vec::new()));
                return false;
            }
        };
        let found = plugins.iter().any(|p| p.to_lowercase().contains(name));
        *self.plugins_cache.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), plugins));
        found
    }

    /// Should the UI offer the OHIF browser viewer? (see [`OhifMode`])
    pub async fn ohif_available(&self, mode: OhifMode) -> bool {
        match mode {
            OhifMode::On => true,
            OhifMode::Off => false,
            OhifMode::Auto => self.plugin_loaded("ohif").await,
        }
    }

    /// "Test connection": reaches Orthanc, checks the version and lists what it offers.
    /// Never fails: problems are reported in the returned [`Diagnosis`].
    pub async fn diagnose(&self) -> Diagnosis {
        let failed = |message: String| Diagnosis {
            ok: false,
            message,
            version: None,
            supported: false,
            plugins: Vec::new(),
            dicomweb: false,
            ohif: false,
            modalities: 0,
        };

        let info = match self.info().await {
            Ok(info) => info,
            Err(AppError::Upstream(m)) if m.contains("HTTP 401") => {
                return failed("Orthanc refused the user name or password".into())
            }
            Err(AppError::Upstream(m)) => return failed(format!("Cannot use this Orthanc: {m}")),
            Err(AppError::NotFound) => return failed("The address answers, but it is not an Orthanc REST API".into()),
            Err(e) => return failed(e.to_string()),
        };

        let plugins: Vec<String> = match self.get_json("/plugins").await {
            Ok(Value::Array(items)) => items.iter().filter_map(|v| v.as_str()).map(str::to_string).collect(),
            _ => Vec::new(),
        };
        let has = |name: &str| plugins.iter().any(|p| p.to_lowercase().contains(name));
        let modalities = self.modalities_raw().await.map(|m| m.len()).unwrap_or(0);

        let message = if info.supported {
            format!("Connected to Orthanc {}", info.version)
        } else {
            format!(
                "Orthanc {} is too old: version {}.{}.{} or newer with the HasExtendedFind capability is required",
                info.version, MIN_VERSION.0, MIN_VERSION.1, MIN_VERSION.2
            )
        };
        Diagnosis {
            ok: info.supported,
            message,
            version: Some(info.version),
            supported: info.supported,
            dicomweb: has("dicom-web") || has("dicomweb"),
            ohif: has("ohif"),
            plugins,
            modalities,
        }
    }

    // ---------------------------------------------------------------- upload

    /// `POST /instances` - sends one DICOM file (or ZIP) to Orthanc.
    ///
    /// `body` is streamed straight from the browser's request, so a large file
    /// never sits fully in the portal's memory. The `Content-Length` is
    /// forwarded so Orthanc receives a normal (non-chunked) request.
    pub async fn upload_instance(&self, body: reqwest::Body, content_length: Option<u64>) -> Result<Value, AppError> {
        let mut request =
            self.request(Method::POST, "/instances").header(header::CONTENT_TYPE, "application/dicom").body(body);
        if let Some(len) = content_length {
            request = request.header(header::CONTENT_LENGTH, len);
        }
        let resp = request.send().await?;
        if resp.status() == StatusCode::BAD_REQUEST {
            return Err(AppError::BadRequest(
                "Orthanc rejected this file (is it a valid DICOM file or ZIP archive?)".into(),
            ));
        }
        Ok(Self::check(resp).await?.json::<Value>().await?)
    }

    // ------------------------------------------------------------ thumbnails

    /// A small JPEG preview of one instance: `GET /instances/{id}/rendered?width=N`
    /// with `Accept: image/jpeg`. Orthanc decodes the image, applies the DICOM
    /// windowing and shrinks it, so only a few KB travel instead of the full image.
    /// Only `width` is given, so the aspect ratio is always preserved.
    ///
    /// * `Ok(Some((bytes, content_type)))` - the preview;
    /// * `Ok(None)` - this instance cannot be previewed (no pixel data, as in a
    ///   report; unsupported compression; instance gone);
    /// * `Err(..)` - Orthanc unreachable / refusing the portal: nothing is concluded
    ///   about the image, so the failure is not remembered.
    pub async fn rendered_image(&self, instance_id: &str, width: u32) -> Result<Option<(Bytes, String)>, AppError> {
        let resp = self
            .request(Method::GET, &format!("/instances/{instance_id}/rendered?width={width}&quality=80"))
            .header(header::ACCEPT, "image/jpeg")
            .timeout(Duration::from_secs(30))
            .send()
            .await?;
        let status = resp.status();
        if status.is_success() {
            let content_type = resp
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("image/jpeg")
                .to_string();
            return Ok(Some((resp.bytes().await?, content_type)));
        }
        if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            return Err(AppError::Upstream("Orthanc refused the portal's credentials".into()));
        }
        Ok(None) // 400 / 404 / 415 / 500: this image cannot be previewed
    }

    // -------------------------------------------------------------- download

    /// Starts a ZIP download. The returned [`Response`] is NOT read here: the
    /// route streams its body to the browser chunk by chunk.
    ///
    /// * `dicomdir = false` -> `/archive`: plain ZIP of the DICOM files.
    /// * `dicomdir = true`  -> `/media`: ZIP with a DICOMDIR index (CD/DVD layout).
    pub async fn archive(&self, kind: ArchiveKind, id: &str, dicomdir: bool) -> Result<Response, AppError> {
        let resource = match kind {
            ArchiveKind::Study => "studies",
            ArchiveKind::Series => "series",
        };
        let suffix = if dicomdir { "media" } else { "archive" };
        let resp = self.request(Method::GET, &format!("/{resource}/{id}/{suffix}")).send().await?;
        Self::check(resp).await
    }

    // ------------------------------------------- sending to DICOM "modalities"

    /// Full configuration of every DICOM node known to Orthanc, as returned by
    /// `GET /modalities?expand`: `{ alias: {AET, Host, Port, AllowStore, ...} }`.
    pub async fn modalities_raw(&self) -> Result<Map<String, Value>, AppError> {
        match self.get_json("/modalities?expand").await? {
            Value::Object(map) => Ok(map),
            // Defensive: without `expand` Orthanc returns just the aliases.
            Value::Array(ids) => {
                Ok(ids.iter().filter_map(|v| v.as_str()).map(|id| (id.to_string(), Value::Null)).collect())
            }
            _ => Ok(Map::new()),
        }
    }

    /// Remote DICOM nodes that accept C-STORE (what "Send to..." may offer).
    pub async fn list_modalities(&self) -> Result<Vec<ModalityNode>, AppError> {
        let mut nodes: Vec<ModalityNode> = self
            .modalities_raw()
            .await?
            .iter()
            // `AllowStore: false` marks nodes Orthanc must not send to.
            .filter(|(_, cfg)| cfg["AllowStore"].as_bool() != Some(false))
            .map(|(id, cfg)| if cfg.is_null() { ModalityNode::bare(id) } else { ModalityNode::from_orthanc(id, cfg) })
            .collect();
        nodes.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(nodes)
    }

    /// Creates or updates a DICOM node: `PUT /modalities/{alias}`.
    /// The change survives an Orthanc restart only if Orthanc runs with
    /// `"DicomModalitiesInDatabase": true`.
    pub async fn put_modality(&self, alias: &str, config: &Value) -> Result<(), AppError> {
        let alias = utf8_percent_encode(alias, PATH_SEGMENT);
        let resp = self
            .request(Method::PUT, &format!("/modalities/{alias}"))
            .json(config)
            .timeout(JSON_TIMEOUT)
            .send()
            .await?;
        Self::check(resp).await.map(|_| ())
    }

    /// Removes a DICOM node: `DELETE /modalities/{alias}`.
    pub async fn delete_modality(&self, alias: &str) -> Result<(), AppError> {
        let alias = utf8_percent_encode(alias, PATH_SEGMENT);
        let resp = self.request(Method::DELETE, &format!("/modalities/{alias}")).timeout(JSON_TIMEOUT).send().await?;
        Self::check(resp).await.map(|_| ())
    }

    /// DICOM "ping" (C-ECHO) to a node: `POST /modalities/{alias}/echo`.
    /// Orthanc needs a JSON body here, even an empty one.
    pub async fn echo_modality(&self, alias: &str) -> Result<(), AppError> {
        let alias = utf8_percent_encode(alias, PATH_SEGMENT);
        let resp = self
            .request(Method::POST, &format!("/modalities/{alias}/echo"))
            .json(&json!({}))
            .timeout(Duration::from_secs(30))
            .send()
            .await?;
        if resp.status().is_success() {
            return Ok(());
        }
        if resp.status() == StatusCode::NOT_FOUND {
            return Err(AppError::NotFound);
        }
        Err(AppError::Upstream(
            "the DICOM node did not answer the C-ECHO: it is unreachable, or it does not know Orthanc's AET".into(),
        ))
    }

    /// Starts a C-STORE of a whole study to the node `alias` and returns the id of
    /// the Orthanc **job** doing it. `"Synchronous": false` makes the HTTP call
    /// return immediately; progress is read later with [`OrthancClient::get_job`].
    pub async fn store_to_modality(&self, alias: &str, study_id: &str) -> Result<String, AppError> {
        let alias = utf8_percent_encode(alias, PATH_SEGMENT);
        let resp = self
            .request(Method::POST, &format!("/modalities/{alias}/store"))
            .json(&json!({ "Resources": [study_id], "Synchronous": false }))
            .timeout(JSON_TIMEOUT)
            .send()
            .await?;
        let answer: Value = Self::check(resp).await?.json().await?;
        answer["ID"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| AppError::Upstream("Orthanc did not return a job id".into()))
    }

    /// `GET /jobs/{id}`: `State` (Pending/Running/Success/Failure/...), `Progress` 0-100.
    pub async fn get_job(&self, job_id: &str) -> Result<Value, AppError> {
        self.get_json(&format!("/jobs/{job_id}")).await
    }

    // ----------------------------------------------------- read-only viewer proxy

    /// `GET` pass-through used for the viewers: DICOMweb (`/dicom-web/...`) for
    /// Weasis and OHIF, WADO-URI (`/wado`) and the OHIF plugin's own files (`/ohif/...`).
    ///
    /// The response is returned UNCHECKED (status 200/204/206/304/404... are all
    /// relayed to the viewer as they are) and is streamed by the caller. Only the
    /// request headers that matter for content negotiation and caching are
    /// forwarded; Orthanc never sees the user's cookie or token.
    ///
    /// `public_host` / `public_proto` describe how the user reached the portal,
    /// so plugins that build absolute URLs produce ones valid from the outside.
    pub async fn proxy_get(
        &self,
        path_and_query: &str,
        client_headers: &HeaderMap,
        public_host: Option<&str>,
        public_proto: &str,
    ) -> Result<Response, AppError> {
        let mut request = self.request(Method::GET, path_and_query);
        for name in
            [header::ACCEPT, header::ACCEPT_ENCODING, header::RANGE, header::IF_NONE_MATCH, header::IF_MODIFIED_SINCE]
        {
            if let Some(value) = client_headers.get(&name) {
                request = request.header(name, value.clone());
            }
        }
        if let Some(host) = public_host {
            request = request
                .header(header::HOST, host)
                .header("x-forwarded-host", host)
                .header("x-forwarded-proto", public_proto);
        }
        let resp = request.send().await?;
        // If Orthanc refuses the PORTAL's own credentials, say so clearly instead
        // of relaying a 401 that would make the viewer ask the user to log in.
        if matches!(resp.status(), StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            return Err(AppError::Upstream("Orthanc refused the portal's credentials".into()));
        }
        Ok(resp)
    }
}
