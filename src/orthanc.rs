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
//! The portal is the ONLY thing that talks to Orthanc. Orthanc itself never has
//! to be exposed to the intranet or the internet.
//!
//! ## With or without an account
//!
//! Orthanc can protect its REST API in two common ways, and the portal supports both:
//!
//! * **With an account** - Orthanc has `AuthenticationEnabled: true` and a user list.
//!   The portal sends HTTP Basic credentials with every request.
//! * **Without an account** - Orthanc has no users and instead only accepts a list of
//!   allowed clients / IP addresses (its own `RemoteAccessAllowed`, an authorization
//!   plugin, or a reverse proxy in front of it). The portal then sends NO
//!   `Authorization` header: it simply relies on being one of the allowed clients.
//!
//! Which one is used is decided by a single setting: an EMPTY Orthanc user name means
//! "no account". See [`Endpoint::from_settings`] and the wording of [`OrthancClient::refusal`].

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

/// What to do when the DICOMweb plugin refuses the address the viewers use. Releases after 1.24
/// only trust forwarded host names listed in `AllowedHosts`, and answer HTTP 500 otherwise.
pub const ALLOWED_HOSTS_ADVICE: &str = "Orthanc's DICOMweb plugin does not trust the address the viewers use, so they show no images: add \"AllowedHosts\": [\"*\"] to the \"DicomWeb\" section of orthanc.json (or list the portal's host names) and restart Orthanc";

/// The plugin's refusal of a forwarded host name (see [`ALLOWED_HOSTS_ADVICE`]).
fn is_untrusted_host_error(body: &str) -> bool {
    body.contains("AllowedHosts") || body.contains("Unable to trust the forwarded HTTP headers")
}

/// How long the list of loaded plugins is remembered.
const PLUGINS_TTL: Duration = Duration::from_secs(60);

/// How long we wait for ANY data from Orthanc during a streamed transfer (upload, ZIP,
/// viewer proxy) before giving up. It is not a limit on the whole transfer, which may
/// take hours, only on a connection that has gone completely silent.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Plugins reported by `GET /plugins` and when they were read.
/// The names are kept lower-cased, ready for [`OrthancClient::plugin_loaded`].
type PluginsCache = Arc<Mutex<Option<(Instant, Arc<Vec<String>>)>>>;

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

    /// The account name sent to Orthanc, or `None` when the portal connects anonymously.
    fn account(&self) -> Option<&str> {
        self.credentials.as_ref().map(|(user, _)| user.as_str())
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
    /// The account the test used, or `None` when it connected without one.
    pub account: Option<String>,
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
    /// Held while `GET /plugins` is being read: callers that find the cache stale at the same
    /// moment (a page load asks about four viewers at once) wait for that one answer instead
    /// of each asking Orthanc.
    plugins_refresh: Arc<tokio::sync::Mutex<()>>,
}

/// Reads the leading digits of "10-rc1" -> 10.
fn leading_number(s: &str) -> Option<u32> {
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Does `GET /system` report this capability? Orthanc has used two shapes for it:
/// a list of names (`["HasExtendedFind", ...]`) and, in recent builds, an object of
/// flags (`{"HasExtendedFind": true, ...}`).
fn has_capability(capabilities: &Value, name: &str) -> bool {
    match capabilities {
        Value::Array(list) => list.iter().any(|c| c.as_str() == Some(name)),
        Value::Object(flags) => flags.get(name).and_then(Value::as_bool).unwrap_or(false),
        _ => false,
    }
}

impl OrthancClient {
    fn build(endpoint: Endpoint) -> Result<Self, AppError> {
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .read_timeout(STREAM_IDLE_TIMEOUT)
            .user_agent(concat!("orthanc-portal/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| AppError::Internal(format!("cannot build HTTP client: {e}")))?;
        Ok(Self {
            http,
            endpoint: Arc::new(RwLock::new(endpoint)),
            plugins_cache: Arc::new(Mutex::new(None)),
            plugins_refresh: Arc::new(tokio::sync::Mutex::new(())),
        })
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

    /// The account the portal currently presents to Orthanc (`None` = no account).
    pub fn account(&self) -> Option<String> {
        self.endpoint.read().unwrap_or_else(|e| e.into_inner()).account().map(str::to_string)
    }

    /// One sentence explaining why Orthanc answered 401 or 403, written for the
    /// administrator and different for the two ways of connecting, because the fix is
    /// different: with an account the credentials are wrong; without one the portal is
    /// usually not on Orthanc's list of allowed clients.
    fn refusal(&self, status: StatusCode) -> String {
        match (self.account(), status) {
            (Some(user), StatusCode::UNAUTHORIZED) => format!(
                "Orthanc refused the account '{user}' (wrong user name or password). \
                 Correct it in Settings > Orthanc connection"
            ),
            (Some(user), _) => format!(
                "Orthanc does not allow the account '{user}' to do this (HTTP 403). Check that account's \
                 permissions in Orthanc and that Orthanc accepts the address the portal connects from"
            ),
            (None, StatusCode::UNAUTHORIZED) => "Orthanc asks for a user name and password, but the portal is set \
                 to connect without an account. Enter the account in Settings > Orthanc connection, or let \
                 Orthanc accept this machine without one"
                .to_string(),
            (None, _) => "Orthanc refuses this machine (HTTP 403). It probably only accepts a list of allowed \
                 clients: add the address the portal connects from. When the portal runs in Docker, Orthanc sees \
                 the Docker host's address (Orthanc on another computer) or an address of Docker's private \
                 network (172.x.x.x), not 127.0.0.1"
                .to_string(),
        }
    }

    /// Turns a non-2xx Orthanc answer into an [`AppError`].
    async fn check(&self, resp: Response) -> Result<Response, AppError> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        if status == StatusCode::NOT_FOUND {
            return Err(AppError::NotFound);
        }
        if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            return Err(AppError::Upstream(self.refusal(status)));
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
        Ok(self.check(resp).await?.json::<Value>().await?)
    }

    /// `POST path` with a JSON body, answer parsed as JSON (e.g. `/tools/count-resources`).
    pub async fn post_json(&self, path: &str, body: &Value) -> Result<Value, AppError> {
        let resp = self.request(Method::POST, path).json(body).timeout(JSON_TIMEOUT).send().await?;
        Ok(self.check(resp).await?.json::<Value>().await?)
    }

    /// `POST path` with a JSON body for work that can take minutes (a synchronous anonymization of
    /// a big study): up to 30 minutes instead of one.
    pub async fn post_json_long(&self, path: &str, body: &Value) -> Result<Value, AppError> {
        let resp = self.request(Method::POST, path).json(body).timeout(Duration::from_secs(1800)).send().await?;
        Ok(self.check(resp).await?.json::<Value>().await?)
    }

    /// `PUT path` with an empty body (e.g. putting a label on a study).
    pub async fn put_empty(&self, path: &str) -> Result<(), AppError> {
        let resp = self.request(Method::PUT, path).body("").timeout(JSON_TIMEOUT).send().await?;
        self.check(resp).await.map(|_| ())
    }

    /// `DELETE path` (e.g. taking a label off a study).
    pub async fn delete(&self, path: &str) -> Result<(), AppError> {
        let resp = self.request(Method::DELETE, path).timeout(JSON_TIMEOUT).send().await?;
        self.check(resp).await.map(|_| ())
    }

    /// `PUT path` with a plain-text body (e.g. `/tools/log-level`).
    pub async fn put_text(&self, path: &str, body: &str) -> Result<(), AppError> {
        let resp = self.request(Method::PUT, path).body(body.to_string()).timeout(JSON_TIMEOUT).send().await?;
        self.check(resp).await.map(|_| ())
    }

    /// `GET path` as text (metrics, log level).
    pub async fn get_text(&self, path: &str) -> Result<String, AppError> {
        let resp = self.request(Method::GET, path).timeout(JSON_TIMEOUT).send().await?;
        Ok(self.check(resp).await?.text().await?)
    }

    /// `GET path`, checked but NOT read: the caller streams it (a DICOM file, a PDF, a NIfTI file).
    pub async fn get_stream(&self, path: &str) -> Result<Response, AppError> {
        let resp = self.request(Method::GET, path).send().await?;
        self.check(resp).await
    }

    /// `POST path` with a JSON body, checked but NOT read (`/tools/create-archive`).
    pub async fn post_stream(&self, path: &str, body: &Value) -> Result<Response, AppError> {
        let resp = self.request(Method::POST, path).json(body).send().await?;
        self.check(resp).await
    }

    /// `POST path` that starts an Orthanc job; returns the job id (`ID` of the answer).
    pub async fn start_job(&self, path: &str, body: &Value) -> Result<String, AppError> {
        let answer = self.post_json(path, body).await?;
        answer["ID"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| AppError::Upstream("Orthanc did not return a job id".into()))
    }

    /// `{ name: {Url, ...} }` of `GET /peers?expand` or `GET /dicom-web/servers?expand` (empty when unavailable).
    pub async fn named_targets(&self, path: &str) -> Map<String, Value> {
        match self.get_json(&format!("{path}?expand")).await {
            Ok(Value::Object(map)) => map,
            Ok(Value::Array(ids)) => {
                ids.iter().filter_map(|v| v.as_str()).map(|id| (id.to_string(), Value::Null)).collect()
            }
            _ => Map::new(),
        }
    }

    /// `POST /tools/find` - returns the array of matching resources.
    pub async fn find(&self, body: &Value) -> Result<Vec<Value>, AppError> {
        let resp = self.request(Method::POST, "/tools/find").json(body).timeout(JSON_TIMEOUT).send().await?;
        match self.check(resp).await?.json::<Value>().await? {
            Value::Array(items) => Ok(items),
            _ => Err(AppError::Upstream("unexpected answer from /tools/find".into())),
        }
    }

    // ------------------------------------------------- version / capabilities

    /// Reads `GET /system` and tells whether this Orthanc is recent enough.
    pub async fn info(&self) -> Result<OrthancInfo, AppError> {
        let system = self.get_json("/system").await?;
        let version = system["Version"].as_str().unwrap_or("unknown").to_string();

        let has_extended_find = has_capability(&system["Capabilities"], "HasExtendedFind");

        // "1.12.10" -> (1, 12, 10). Development builds ("mainline") count as new enough.
        let mut parts = version.split('.').map(leading_number);
        let version_ok = match (parts.next().flatten(), parts.next().flatten(), parts.next().flatten()) {
            (Some(major), Some(minor), patch) => (major, minor, patch.unwrap_or(0)) >= MIN_VERSION,
            _ => true,
        };
        Ok(OrthancInfo { version, supported: version_ok && has_extended_find })
    }

    /// The cached plugin list, if it is still fresh. The lock is never held across an `.await`.
    fn fresh_plugins(&self) -> Option<Arc<Vec<String>>> {
        let cache = self.plugins_cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.as_ref().filter(|(read_at, _)| read_at.elapsed() < PLUGINS_TTL).map(|(_, plugins)| plugins.clone())
    }

    /// The loaded plugins, lower-cased (`GET /plugins`, cached 60 s, read once even when many ask).
    async fn plugins(&self) -> Arc<Vec<String>> {
        if let Some(plugins) = self.fresh_plugins() {
            return plugins;
        }
        let _reading = self.plugins_refresh.lock().await;
        if let Some(plugins) = self.fresh_plugins() {
            return plugins; // read by whoever held the lock before us
        }
        let (read_at, plugins) = match self.get_json("/plugins").await {
            Ok(Value::Array(items)) => {
                (Instant::now(), items.iter().filter_map(|v| v.as_str()).map(str::to_lowercase).collect())
            }
            // Orthanc does not answer. Remember "no plugins" for a few seconds only, so that
            // every page load does not wait for the connection timeout again.
            _ => (
                Instant::now().checked_sub(PLUGINS_TTL - Duration::from_secs(10)).unwrap_or_else(Instant::now),
                Vec::new(),
            ),
        };
        let plugins = Arc::new(plugins);
        *self.plugins_cache.lock().unwrap_or_else(|e| e.into_inner()) = Some((read_at, plugins.clone()));
        plugins
    }

    /// Is a plugin whose name contains `name` (lower case) loaded?
    pub async fn plugin_loaded(&self, name: &str) -> bool {
        self.plugins().await.iter().any(|p| p.contains(name))
    }

    /// Should the UI offer the viewer of the plugin `plugin`? Same rules as OHIF (see [`OhifMode`]).
    pub async fn viewer_available(&self, mode: OhifMode, plugin: &str) -> bool {
        match mode {
            OhifMode::On => true,
            OhifMode::Off => false,
            OhifMode::Auto => self.plugin_loaded(plugin).await,
        }
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
            account: self.account(),
            plugins: Vec::new(),
            dicomweb: false,
            ohif: false,
            modalities: 0,
        };

        // A bare first request, only to tell "Orthanc refuses the portal" (wrong account, or
        // this machine is not on the allowed list) apart from every other kind of problem.
        match self.request(Method::GET, "/system").timeout(JSON_TIMEOUT).send().await {
            Ok(resp) if matches!(resp.status(), StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) => {
                return failed(self.refusal(resp.status()))
            }
            Ok(_) => {}
            Err(e) => return failed(AppError::from(e).to_string()),
        }

        let info = match self.info().await {
            Ok(info) => info,
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
        let dicomweb = has("dicom-web") || has("dicomweb");
        // The viewers reach DICOMweb under the portal's own address: check the plugin accepts one.
        let untrusted_host = dicomweb && info.supported && self.dicomweb_refuses_forwarded_host().await;

        let message = if untrusted_host {
            ALLOWED_HOSTS_ADVICE.to_string()
        } else if info.supported {
            match self.account() {
                Some(user) => format!("Connected to Orthanc {} with the account '{user}'", info.version),
                None => format!("Connected to Orthanc {} without an account", info.version),
            }
        } else {
            format!(
                "Orthanc {} is too old: version {}.{}.{} or newer with the HasExtendedFind capability is required",
                info.version, MIN_VERSION.0, MIN_VERSION.1, MIN_VERSION.2
            )
        };
        Diagnosis {
            ok: info.supported && !untrusted_host,
            message,
            version: Some(info.version),
            supported: info.supported,
            account: self.account(),
            dicomweb,
            ohif: has("ohif"),
            plugins,
            modalities,
        }
    }

    /// Asks the DICOMweb plugin for one study the way a viewer does through the portal, under a
    /// host name that is not `localhost`. `true` when it refuses that name (no `AllowedHosts`).
    async fn dicomweb_refuses_forwarded_host(&self) -> bool {
        const PROBE_HOST: &str = "portal-check.invalid";
        let answer = self
            .request(Method::GET, "/dicom-web/studies?limit=1")
            .header(header::HOST, PROBE_HOST)
            .header("x-forwarded-host", PROBE_HOST)
            .header("x-forwarded-proto", "https")
            .timeout(JSON_TIMEOUT)
            .send()
            .await;
        match answer {
            Ok(resp) if resp.status() == StatusCode::INTERNAL_SERVER_ERROR => {
                is_untrusted_host_error(&resp.text().await.unwrap_or_default())
            }
            _ => false,
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
        Ok(self.check(resp).await?.json::<Value>().await?)
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
            return Err(AppError::Upstream(self.refusal(status)));
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
        self.check(resp).await
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
        self.check(resp).await.map(|_| ())
    }

    /// Removes a DICOM node: `DELETE /modalities/{alias}`.
    pub async fn delete_modality(&self, alias: &str) -> Result<(), AppError> {
        let alias = utf8_percent_encode(alias, PATH_SEGMENT);
        let resp = self.request(Method::DELETE, &format!("/modalities/{alias}")).timeout(JSON_TIMEOUT).send().await?;
        self.check(resp).await.map(|_| ())
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
        let answer: Value = self.check(resp).await?.json().await?;
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
        // If Orthanc refuses the PORTAL itself (its account, or its network address), say
        // so clearly instead of relaying a 401 that would make the viewer ask the user to log in.
        if matches!(resp.status(), StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            return Err(AppError::Upstream(self.refusal(resp.status())));
        }
        // The DICOMweb plugin refusing the portal's address: say what to change, not "500".
        if resp.status() == StatusCode::INTERNAL_SERVER_ERROR && path_and_query.starts_with("/dicom-web/") {
            let body = resp.text().await.unwrap_or_default();
            if is_untrusted_host_error(&body) {
                tracing::warn!("{ALLOWED_HOSTS_ADVICE}");
                return Err(AppError::Upstream(ALLOWED_HOSTS_ADVICE.into()));
            }
            let details = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v["Details"].as_str().or(v["Message"].as_str()).map(str::to_string))
                .unwrap_or_else(|| "internal error".into());
            return Err(AppError::Upstream(format!("Orthanc could not answer the viewer: {details}")));
        }
        Ok(resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both shapes of `Capabilities` (list of names, object of flags) are understood.
    #[test]
    fn capabilities_as_a_list_or_as_flags() {
        assert!(has_capability(&serde_json::json!(["HasExtendedChanges", "HasExtendedFind"]), "HasExtendedFind"));
        assert!(has_capability(&serde_json::json!({"HasExtendedFind": true}), "HasExtendedFind"));
        assert!(!has_capability(&serde_json::json!({"HasExtendedFind": false}), "HasExtendedFind"));
        assert!(!has_capability(&serde_json::json!(["HasExtendedChanges"]), "HasExtendedFind"));
        assert!(!has_capability(&Value::Null, "HasExtendedFind"));
    }

    /// The portal sends an account only when a user name is configured.
    #[test]
    fn an_empty_user_name_means_no_account() {
        let without = OrthancClient::for_test("http://orthanc:8042/", "", "ignored").unwrap();
        assert_eq!(without.account(), None);
        let with = OrthancClient::for_test("http://orthanc:8042", "portal", "secret").unwrap();
        assert_eq!(with.account().as_deref(), Some("portal"));
    }

    /// A refusal is explained in the words of the setup in use: the fix differs (a wrong
    /// account vs. an address missing from Orthanc's list of allowed clients).
    #[test]
    fn refusals_are_explained_for_each_way_of_connecting() {
        let without = OrthancClient::for_test("http://orthanc:8042", "", "").unwrap();
        let forbidden = without.refusal(StatusCode::FORBIDDEN);
        assert!(
            forbidden.contains("refuses this machine") && forbidden.contains("allowed") && forbidden.contains("Docker")
        );
        assert!(without.refusal(StatusCode::UNAUTHORIZED).contains("without an account"));

        let with = OrthancClient::for_test("http://orthanc:8042", "portal", "secret").unwrap();
        assert!(with.refusal(StatusCode::UNAUTHORIZED).contains("refused the account 'portal'"));
        assert!(with.refusal(StatusCode::FORBIDDEN).contains("does not allow the account 'portal'"));
        // The password must never appear in a message shown to users.
        assert!(!with.refusal(StatusCode::UNAUTHORIZED).contains("secret"));
    }

    /// Settings: no account means no stored password either.
    #[test]
    fn clearing_the_user_name_forgets_the_password() {
        let mut settings = Settings::defaults();
        settings.orthanc_username = "portal".into();
        settings.orthanc_password = crate::settings::Secret("secret".into());
        let updated = settings.with_patch(&json!({ "orthanc_username": "" })).unwrap();
        assert!(updated.orthanc_username.is_empty() && updated.orthanc_password.0.is_empty());
    }
}
