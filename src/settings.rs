//! # settings.rs
//!
//! Every setting that can be changed **from the web interface** (Settings tab,
//! administrators on the intranet only).
//!
//! ## Where settings come from
//! 1. Built-in defaults ([`Settings::defaults`]).
//! 2. Environment variables / `.env` - only the **first-start seed** ([`Settings::from_env`]),
//!    so existing Docker deployments keep working.
//! 3. The SQLite database - once an administrator saves the Settings form, the
//!    stored values win. Keys missing from an older stored copy fall back to (1)+(2).
//!
//! The only things that cannot live here are those needed *before* the database
//! can be read (database path, first administrator, reset flag): see `config.rs`.
//!
//! ## How changes reach the running program
//! Handlers never keep a copy: they call `state.settings.get()` for a snapshot on
//! every request, so a saved change applies immediately. Settings that need
//! action (Orthanc address, listening ports) are applied by `routes/settings.rs`.

use crate::config::{env_opt, env_parse};
use crate::error::AppError;
use crate::permissions::{Permission, PermissionSet};
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fmt;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};

/// Whether the session cookie gets the `Secure` attribute.
///
/// A `Secure` cookie is only stored by browsers on HTTPS connections. Because
/// the portal is reachable both over plain HTTP (intranet) and over HTTPS
/// (Cloudflare Tunnel), the default `Auto` mode decides per request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CookieSecure {
    /// `Secure` only when the request arrived over HTTPS (`X-Forwarded-Proto` / `CF-Visitor`).
    Auto,
    /// Always `Secure` (only if you serve HTTPS everywhere).
    Always,
    /// Never `Secure` (plain HTTP only deployments).
    Never,
}

/// Whether the browser viewer (OHIF) button is offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OhifMode {
    /// Offer it when Orthanc reports the OHIF plugin in `GET /plugins`.
    Auto,
    /// Always offer it.
    On,
    /// Never offer it.
    Off,
}

/// A secret string. It serialises normally (it must be stored in the database to
/// be usable) but prints as `***` in `Debug`, so it can never leak into a log by accident.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(pub String);

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.0.is_empty() { "(empty)" } else { "***" })
    }
}

/// A named bundle of permissions, offered as a shortcut when editing a user.
/// The list of roles is itself a setting: administrators can add, change and remove them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Role {
    /// Short identifier: lowercase letters, digits, `-` and `_`.
    pub key: String,
    /// Name shown in the user form.
    pub label: String,
    pub permissions: Vec<Permission>,
}

/// The roles every new installation starts with.
pub fn default_roles() -> Vec<Role> {
    let role = |key: &str, label: &str, permissions: &[Permission]| Role {
        key: key.into(),
        label: label.into(),
        permissions: permissions.to_vec(),
    };
    vec![
        role(
            "viewer",
            "Viewer (search + view; the only role useful remotely)",
            &[Permission::Read, Permission::Visualize],
        ),
        role(
            "contributor",
            "Contributor (viewer + upload)",
            &[Permission::Read, Permission::Visualize, Permission::Upload],
        ),
        role(
            "exporter",
            "Exporter (viewer + download + send)",
            &[Permission::Read, Permission::Visualize, Permission::Download, Permission::Send],
        ),
        role(
            "clinical",
            "Full clinical (everything except administration)",
            &[Permission::Read, Permission::Visualize, Permission::Upload, Permission::Download, Permission::Send],
        ),
        role("administrator", "Administrator (everything)", &Permission::ALL),
    ]
}

/// All runtime settings. Edited as a whole from the web interface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    // ---- General ----------------------------------------------------------
    /// Name shown in the top bar and on the login page.
    pub portal_name: String,
    /// Text of the browser window / tab. Empty = use the portal name.
    pub browser_title: String,
    /// Show the portal name next to the logo? (Untick it when the logo already contains the name.)
    pub show_portal_name: bool,
    /// Optional message shown on the login page (e.g. "Authorised staff only").
    pub login_notice: String,

    // ---- Orthanc connection -----------------------------------------------
    /// REST API of Orthanc, no trailing slash (e.g. `http://orthanc:8042`).
    pub orthanc_url: String,
    /// Orthanc account used by the portal. Empty = no authentication.
    pub orthanc_username: String,
    /// Its password. Never sent to the browser (see [`Settings::to_view`]).
    pub orthanc_password: Secret,

    // ---- Network ------------------------------------------------------------
    /// INTRANET listener, e.g. `0.0.0.0:8080`: every feature.
    pub bind_addr: String,
    /// Is the internet-facing, VIEW-ONLY listener running?
    pub remote_enabled: bool,
    /// Its address, e.g. `0.0.0.0:8081`. Point the Cloudflare Tunnel here.
    pub remote_bind_addr: String,
    /// Over the internet, refuse requests that return WHOLE DICOM files (a viewer only needs
    /// the metadata and the pixel frames). Stops "save as .dcm" through the viewer's own
    /// address. Recommended; switch it off only if a special object type fails to display.
    pub remote_block_dicom_files: bool,
    /// Force the address given to Weasis (e.g. `https://pacs.example.com`).
    /// Empty = detect per request, which suits intranet and Cloudflare at once.
    pub public_base_url: String,

    // ---- Sessions and login security ----------------------------------------
    /// Hours a login stays valid (applies to the next logins).
    pub session_hours: u64,
    /// Minutes the token inside a Weasis link stays valid.
    pub weasis_token_minutes: u64,
    pub cookie_secure: CookieSecure,
    /// Failed logins in a row before an account is locked.
    pub login_max_failures: u32,
    /// Minutes an account stays locked.
    pub login_lockout_minutes: u64,
    /// Days to keep the audit log and finished transfers. 0 = keep everything.
    pub audit_retention_days: u64,

    // ---- Viewers and uploads --------------------------------------------------
    pub ohif: OhifMode,
    /// Largest single uploaded file, in megabytes.
    pub max_upload_mb: u64,

    // ---- Preview thumbnails -----------------------------------------------------
    /// Show a small preview image next to every study and series?
    pub thumbnails_enabled: bool,
    /// Memory (MB) the portal may use to remember generated thumbnails. 0 = no cache.
    pub thumbnail_cache_mb: u64,

    // ---- Roles ------------------------------------------------------------------
    pub roles: Vec<Role>,
}

impl Settings {
    /// Built-in defaults, used when nothing else is configured.
    pub fn defaults() -> Self {
        Self {
            portal_name: "Orthanc Portal".into(),
            browser_title: String::new(),
            show_portal_name: true,
            login_notice: String::new(),
            orthanc_url: "http://localhost:8042".into(),
            orthanc_username: String::new(),
            orthanc_password: Secret::default(),
            bind_addr: "0.0.0.0:8080".into(),
            remote_enabled: true,
            remote_bind_addr: "0.0.0.0:8081".into(),
            remote_block_dicom_files: true,
            public_base_url: String::new(),
            session_hours: 12,
            weasis_token_minutes: 240,
            cookie_secure: CookieSecure::Auto,
            login_max_failures: 5,
            login_lockout_minutes: 5,
            audit_retention_days: 0,
            ohif: OhifMode::Auto,
            max_upload_mb: 2048,
            thumbnails_enabled: true,
            thumbnail_cache_mb: 64,
            roles: default_roles(),
        }
    }

    /// Defaults overridden by the environment variables of earlier versions
    /// (`ORTHANC_URL`, `BIND_ADDR`, `SESSION_HOURS`...). Only a first-start seed.
    pub fn from_env() -> Result<Self> {
        let mut s = Self::defaults();
        if let Some(v) = env_opt("ORTHANC_URL") {
            s.orthanc_url = v;
        }
        if let Some(v) = env_opt("ORTHANC_USERNAME") {
            s.orthanc_username = v;
        }
        if let Some(v) = env_opt("ORTHANC_PASSWORD") {
            s.orthanc_password = Secret(v);
        }
        if let Some(v) = env_opt("BIND_ADDR") {
            s.bind_addr = v;
        }
        // REMOTE_BIND_ADDR=off disables the internet-facing listener.
        if let Some(v) = env_opt("REMOTE_BIND_ADDR") {
            match v.to_lowercase().as_str() {
                "off" | "none" | "disabled" | "false" => s.remote_enabled = false,
                _ => s.remote_bind_addr = v,
            }
        }
        if let Some(v) = env_opt("PUBLIC_BASE_URL") {
            s.public_base_url = v;
        }
        s.session_hours = env_parse("SESSION_HOURS", s.session_hours)?;
        s.weasis_token_minutes = env_parse("WEASIS_TOKEN_MINUTES", s.weasis_token_minutes)?;
        s.max_upload_mb = env_parse("MAX_UPLOAD_MB", s.max_upload_mb)?;
        if let Some(v) = env_opt("COOKIE_SECURE") {
            s.cookie_secure = match v.to_lowercase().as_str() {
                "auto" => CookieSecure::Auto,
                "true" | "1" | "yes" => CookieSecure::Always,
                "false" | "0" | "no" => CookieSecure::Never,
                other => bail!("COOKIE_SECURE must be auto, true or false (got '{other}')"),
            };
        }
        if let Some(v) = env_opt("OHIF_ENABLED") {
            s.ohif = match v.to_lowercase().as_str() {
                "auto" => OhifMode::Auto,
                "true" | "1" | "yes" => OhifMode::On,
                "false" | "0" | "no" => OhifMode::Off,
                other => bail!("OHIF_ENABLED must be auto, true or false (got '{other}')"),
            };
        }
        s.normalise();
        s.validate().map_err(|e| anyhow!("invalid settings in the environment: {e}"))?;
        Ok(s)
    }

    /// Loads the stored settings, filling anything missing from `seed`.
    /// A damaged or invalid stored copy is ignored (with a warning) rather than
    /// preventing the portal from starting.
    pub fn merged_with_stored(seed: Settings, stored_json: Option<String>) -> Settings {
        let Some(raw) = stored_json else { return seed };
        let merged = (|| -> Result<Settings> {
            let stored: Value = serde_json::from_str(&raw)?;
            let mut base = serde_json::to_value(&seed)?;
            if let (Some(base_obj), Some(stored_obj)) = (base.as_object_mut(), stored.as_object()) {
                for (k, v) in stored_obj {
                    base_obj.insert(k.clone(), v.clone());
                }
            }
            let mut settings: Settings = serde_json::from_value(base)?;
            settings.normalise();
            settings.validate().map_err(|e| anyhow!("{e}"))?;
            Ok(settings)
        })();
        match merged {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("Stored settings are invalid ({e}); using the defaults instead");
                seed
            }
        }
    }

    /// Trims text fields and removes trailing slashes from URLs.
    pub fn normalise(&mut self) {
        self.portal_name = self.portal_name.trim().to_string();
        self.browser_title = self.browser_title.trim().to_string();
        self.login_notice = self.login_notice.trim().to_string();
        self.orthanc_url = self.orthanc_url.trim().trim_end_matches('/').to_string();
        self.orthanc_username = self.orthanc_username.trim().to_string();
        self.bind_addr = self.bind_addr.trim().to_string();
        self.remote_bind_addr = self.remote_bind_addr.trim().to_string();
        self.public_base_url = self.public_base_url.trim().trim_end_matches('/').to_string();
        for role in &mut self.roles {
            role.key = role.key.trim().to_string();
            role.label = role.label.trim().to_string();
            role.permissions.sort();
            role.permissions.dedup();
        }
    }

    /// Checks every value; the message of the first problem is shown in the UI.
    pub fn validate(&self) -> Result<(), AppError> {
        let bad = |m: String| Err(AppError::BadRequest(m));
        let has_control = |s: &str| s.chars().any(|c| c.is_control());

        // --- general
        if !(1..=60).contains(&self.portal_name.chars().count()) || has_control(&self.portal_name) {
            return bad("The portal name must have 1 to 60 characters".into());
        }
        if self.browser_title.chars().count() > 80 || has_control(&self.browser_title) {
            return bad("The browser window title can have at most 80 characters".into());
        }
        if self.login_notice.chars().count() > 500 || self.login_notice.chars().any(|c| c.is_control() && c != '\n') {
            return bad("The login notice can have at most 500 characters".into());
        }

        // --- Orthanc
        let url_ok = (self.orthanc_url.starts_with("http://") || self.orthanc_url.starts_with("https://"))
            && !self.orthanc_url.chars().any(|c| c.is_whitespace() || c.is_control());
        if !url_ok {
            return bad("The Orthanc address must look like http://orthanc:8042 or https://...".into());
        }
        if has_control(&self.orthanc_username)
            || self.orthanc_username.len() > 128
            || self.orthanc_password.0.len() > 256
        {
            return bad("The Orthanc user name or password is not valid".into());
        }

        // --- network
        if self.bind_addr.parse::<SocketAddr>().is_err() {
            return bad(format!(
                "'{}' is not a valid intranet address. Use IP:PORT, e.g. 0.0.0.0:8080",
                self.bind_addr
            ));
        }
        if self.remote_enabled {
            match self.remote_bind_addr.parse::<SocketAddr>() {
                Err(_) => {
                    return bad(format!(
                        "'{}' is not a valid remote address. Use IP:PORT, e.g. 0.0.0.0:8081",
                        self.remote_bind_addr
                    ))
                }
                Ok(remote) => {
                    if remote.port() == self.bind_addr.parse::<SocketAddr>().map(|a| a.port()).unwrap_or(0) {
                        return bad("The intranet and remote listeners must use different ports".into());
                    }
                }
            }
        }
        if !self.public_base_url.is_empty() {
            // The value ends up inside a quoted Weasis command line: allow nothing exotic.
            let chars_ok = self.public_base_url.chars().all(|c| c.is_ascii_alphanumeric() || ":/.-_[]".contains(c));
            if !(self.public_base_url.starts_with("http://") || self.public_base_url.starts_with("https://"))
                || !chars_ok
            {
                return bad("The public address must look like https://pacs.example.com (no spaces or quotes)".into());
            }
        }

        // --- numbers
        let range = |name: &str, v: u64, lo: u64, hi: u64| -> Result<(), AppError> {
            if (lo..=hi).contains(&v) {
                Ok(())
            } else {
                Err(AppError::BadRequest(format!("{name} must be between {lo} and {hi}")))
            }
        };
        range("Session duration (hours)", self.session_hours, 1, 720)?;
        range("Weasis token duration (minutes)", self.weasis_token_minutes, 5, 1440)?;
        range("Failed logins before lockout", self.login_max_failures as u64, 3, 20)?;
        range("Lockout duration (minutes)", self.login_lockout_minutes, 1, 1440)?;
        range("Audit log retention (days)", self.audit_retention_days, 0, 3650)?;
        range("Maximum upload size (MB)", self.max_upload_mb, 1, 102_400)?;
        range("Thumbnail cache (MB)", self.thumbnail_cache_mb, 0, 1024)?;

        // --- roles
        if self.roles.len() > 30 {
            return bad("At most 30 roles are allowed".into());
        }
        let mut seen = Vec::new();
        for role in &self.roles {
            let key_ok = (1..=32).contains(&role.key.len())
                && role.key.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
            if !key_ok {
                return bad(format!(
                    "Role key '{}' is not valid: use 1-32 lowercase letters, digits, - or _",
                    role.key
                ));
            }
            if seen.contains(&role.key) {
                return bad(format!("Role key '{}' is used twice", role.key));
            }
            seen.push(role.key.clone());
            if !(1..=80).contains(&role.label.chars().count()) || has_control(&role.label) {
                return bad(format!("The name of role '{}' must have 1 to 80 characters", role.key));
            }
            PermissionSet::from_slice(&role.permissions).validate()?;
        }
        Ok(())
    }

    /// Applies a partial update coming from the web form and returns the validated result.
    ///
    /// Only the keys present in `patch` change. The Orthanc password is special:
    /// absent or empty = keep the saved one, and `"clear_orthanc_password": true` removes it.
    pub fn with_patch(&self, patch: &Value) -> Result<Settings, AppError> {
        let patch =
            patch.as_object().ok_or_else(|| AppError::BadRequest("The settings must be a JSON object".into()))?;
        let mut base = serde_json::to_value(self).map_err(|e| AppError::Internal(e.to_string()))?;
        let obj = base.as_object_mut().ok_or_else(|| AppError::Internal("settings are not an object".into()))?;

        let mut clear_password = false;
        for (key, value) in patch {
            match key.as_str() {
                "clear_orthanc_password" => clear_password = value.as_bool().unwrap_or(false),
                // Read-only information the form echoes back: ignore it.
                "orthanc_password_set" => {}
                "orthanc_password" => {
                    if value.as_str().map(|p| !p.is_empty()).unwrap_or(false) {
                        obj.insert(key.clone(), value.clone());
                    }
                }
                _ if obj.contains_key(key) => {
                    obj.insert(key.clone(), value.clone());
                }
                _ => return Err(AppError::BadRequest(format!("Unknown setting '{key}'"))),
            }
        }
        if clear_password {
            obj.insert("orthanc_password".into(), json!(""));
        }

        let mut updated: Settings = serde_json::from_value(base)
            .map_err(|e| AppError::BadRequest(format!("Invalid value in the settings: {e}")))?;
        updated.normalise();
        updated.validate()?;
        Ok(updated)
    }

    /// The settings as sent to the browser: everything except the Orthanc
    /// password, replaced by a yes/no `orthanc_password_set`.
    pub fn to_view(&self) -> Value {
        let mut value = serde_json::to_value(self).unwrap_or(Value::Null);
        if let Some(obj) = value.as_object_mut() {
            obj.remove("orthanc_password");
            obj.insert("orthanc_password_set".into(), json!(!self.orthanc_password.0.is_empty()));
        }
        value
    }

    /// Names of the settings that differ between `self` and `other` (for the audit
    /// log; values are never listed, so secrets cannot leak).
    pub fn changed_keys(&self, other: &Settings) -> Vec<String> {
        let (a, b) = (serde_json::to_value(self).unwrap_or_default(), serde_json::to_value(other).unwrap_or_default());
        match (a.as_object(), b.as_object()) {
            (Some(a), Some(b)) => a.iter().filter(|(k, v)| b.get(*k) != Some(v)).map(|(k, _)| k.clone()).collect(),
            _ => Vec::new(),
        }
    }

    // ---- convenience getters used by the handlers ----------------------------
    /// Text of the browser tab: the custom title, or the portal name when none is set.
    pub fn window_title(&self) -> &str {
        if self.browser_title.is_empty() {
            &self.portal_name
        } else {
            &self.browser_title
        }
    }
    pub fn session_ttl_secs(&self) -> u64 {
        self.session_hours * 3600
    }
    pub fn lockout_secs(&self) -> i64 {
        (self.login_lockout_minutes * 60) as i64
    }
    pub fn max_upload_bytes(&self) -> usize {
        (self.max_upload_mb as usize).saturating_mul(1024 * 1024)
    }
    pub fn thumbnail_cache_bytes(&self) -> usize {
        (self.thumbnail_cache_mb as usize).saturating_mul(1024 * 1024)
    }
}

/// Shared, replaceable holder of the current [`Settings`].
/// `get()` returns a cheap snapshot; `set()` swaps in a new version atomically.
#[derive(Clone)]
pub struct SettingsStore {
    inner: Arc<RwLock<Arc<Settings>>>,
}

impl SettingsStore {
    pub fn new(settings: Settings) -> Self {
        Self { inner: Arc::new(RwLock::new(Arc::new(settings))) }
    }

    pub fn get(&self) -> Arc<Settings> {
        self.inner.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set(&self, settings: Settings) {
        *self.inner.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(settings);
    }
}
