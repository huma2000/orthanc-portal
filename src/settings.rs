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

/// How dates are written in the portal (Settings > Appearance).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DateFormat {
    /// 2024-05-17
    #[default]
    Iso,
    /// 17/05/2024
    Dmy,
    /// 05/17/2024
    Mdy,
    /// 17.05.2024
    DmyDots,
    /// 17 May 2024
    Long,
}

/// How patient names are written (DICOM stores `DOE^JOHN`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NameFormat {
    /// DOE, JOHN
    #[default]
    LastFirst,
    /// JOHN DOE
    FirstLast,
    /// DOE^JOHN, as stored
    Dicom,
}

/// The OHIF modes that can be offered (each one a button), with the address of each.
pub const OHIF_MODES: [&str; 6] = ["viewer", "mpr", "volume", "tmtv", "segmentation", "microscopy"];

/// The fields the study window can show (Settings > Search & labels), in the order offered.
pub const STUDY_DETAIL_FIELDS: [&str; 14] = [
    "patient_id",
    "birth_date",
    "sex",
    "other_ids",
    "study_date",
    "description",
    "accession",
    "referring",
    "requesting",
    "institution",
    "modalities",
    "series",
    "labels",
    "study_uid",
];

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

/// How the portal talks to the mail server (Settings > Email).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SmtpSecurity {
    /// Plain connection upgraded to TLS (usually port 587). The common choice.
    #[default]
    Starttls,
    /// TLS from the first byte (usually port 465).
    Tls,
    /// No encryption: only for a mail relay on the local network (usually port 25).
    None,
}

/// Light / dark colours of the portal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    /// Follow the operating system's preference.
    #[default]
    Auto,
    Light,
    Dark,
}

/// Size of the study cards on the home page of the simplified view.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CardSize {
    Small,
    #[default]
    Medium,
    Large,
}

/// How the recent studies of the simplified view's home page are shown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecentLayout {
    /// The same table as the Studies page: same columns, same buttons.
    #[default]
    Table,
    /// A grid of cards with a big preview; a click opens the study's details.
    Cards,
}

/// Which studies count as "recent" on the home page of the simplified view.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecentOrder {
    /// Newest study date first (when the examination was done).
    StudyDate,
    /// Most recently received by Orthanc first (uploads, modalities sending in).
    #[default]
    Received,
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

/// Value of a switch missing from an older stored copy of the settings.
fn default_auto_refresh_seconds() -> u64 {
    30
}

fn default_true() -> bool {
    true
}

/// One quick modality filter of the search pages (the CT, MR, US... buttons).
/// Administrators choose which ones are shown and in what order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModalityButton {
    /// Text of the button, e.g. `RX`.
    pub label: String,
    /// DICOM modality codes it searches for, e.g. `["CR", "DX"]` ("any of").
    pub codes: Vec<String>,
    /// Shown when the mouse rests on the button, e.g. "X-ray (CR + DX)".
    #[serde(default)]
    pub title: String,
    /// Hidden buttons stay in the list (so they can be shown again) but are not offered.
    #[serde(default = "default_true")]
    pub visible: bool,
}

/// The modality buttons every new installation starts with.
pub fn default_modality_buttons() -> Vec<ModalityButton> {
    let button = |label: &str, codes: &[&str], title: &str| ModalityButton {
        label: label.into(),
        codes: codes.iter().map(|c| c.to_string()).collect(),
        title: title.into(),
        visible: true,
    };
    vec![
        button("CT", &["CT"], "Computed tomography"),
        button("MR", &["MR"], "Magnetic resonance"),
        button("US", &["US"], "Ultrasound"),
        button("RX", &["CR", "DX"], "X-ray (CR + DX)"),
        button("MG", &["MG"], "Mammography"),
        button("XA", &["XA"], "X-ray angiography"),
        button("RF", &["RF"], "Fluoroscopy"),
        button("NM", &["NM"], "Nuclear medicine"),
        button("PT", &["PT"], "PET"),
    ]
}

/// The roles every new installation starts with.
pub fn default_roles() -> Vec<Role> {
    let role = |key: &str, label: &str, permissions: &[Permission]| Role {
        key: key.into(),
        label: label.into(),
        permissions: permissions.to_vec(),
    };
    vec![
        role("viewer", "Viewer (search + view)", &[Permission::Read, Permission::Visualize]),
        role(
            "contributor",
            "Contributor (viewer + upload + labels)",
            &[Permission::Read, Permission::Visualize, Permission::Upload, Permission::Labels],
        ),
        role(
            "exporter",
            "Exporter (viewer + download + send + share)",
            &[Permission::Read, Permission::Visualize, Permission::Download, Permission::Send, Permission::Share],
        ),
        role(
            "clinical",
            "Full clinical (everything except administration)",
            &[
                Permission::Read,
                Permission::Visualize,
                Permission::Upload,
                Permission::Download,
                Permission::Send,
                Permission::Share,
                Permission::Labels,
            ],
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
    /// Orthanc account used by the portal. EMPTY = the portal connects WITHOUT an account
    /// (no `Authorization` header), for an Orthanc that has no users and only accepts a
    /// list of allowed clients / IP addresses.
    pub orthanc_username: String,
    /// Its password. Never sent to the browser (see [`Settings::to_view`]). Always empty
    /// when there is no account (see [`Settings::normalise`]).
    pub orthanc_password: Secret,

    // ---- Network ------------------------------------------------------------
    /// INTRANET listener, e.g. `0.0.0.0:8080`: the whole portal, for staff.
    pub bind_addr: String,
    /// Is the internet-facing listener running? It serves ONLY invitation links (shared studies).
    pub remote_enabled: bool,
    /// Its address, e.g. `0.0.0.0:8081`. Point the Cloudflare Tunnel here.
    pub remote_bind_addr: String,
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

    // ---- Search ------------------------------------------------------------------
    /// Quick modality filters of the Studies / Series pages, in display order.
    #[serde(default = "default_modality_buttons")]
    pub modality_buttons: Vec<ModalityButton>,

    // ---- Viewers and uploads --------------------------------------------------
    /// Offer the "Weasis" button (desktop viewer, must be installed on each computer)?
    #[serde(default = "default_true")]
    pub weasis_enabled: bool,
    pub ohif: OhifMode,
    /// Largest single uploaded file, in megabytes.
    pub max_upload_mb: u64,

    // ---- Sharing studies with people outside (invitation links) -----------------
    /// Public address of the internet-facing listener as outsiders reach it, e.g.
    /// `https://pacs.example.com`. Links are `<this>/s/<token>`. Must be set to share.
    pub share_public_url: String,
    /// Suggested validity of a new link, in hours, and the longest an administrator allows.
    pub share_default_hours: u64,
    pub share_max_hours: u64,
    /// Suggested number of ZIP downloads of a new link (0 = view only), and the most allowed.
    pub share_default_downloads: u64,
    pub share_max_downloads: u64,
    /// Keep a copy of each link's password so its creator (or an administrator) can see it again in
    /// the Shares list while the link is active. Off = shown only once, stored only as a hash.
    #[serde(default = "default_true")]
    pub share_keep_passwords: bool,

    // ---- Email (share invitations sent by the portal) -----------------------------
    /// Mail server. Empty = email is switched off.
    #[serde(default)]
    pub smtp_host: String,
    #[serde(default = "default_smtp_port")]
    pub smtp_port: u64,
    #[serde(default)]
    pub smtp_security: SmtpSecurity,
    /// Account on the mail server (empty = the server accepts the portal without signing in).
    #[serde(default)]
    pub smtp_username: String,
    /// Its password. Never sent to the browser (see [`Settings::to_view`]).
    #[serde(default)]
    pub smtp_password: Secret,
    /// Sender of the emails, e.g. `Chalklands PACS <pacs@chalklands.example>`.
    #[serde(default)]
    pub smtp_from: String,
    /// Text at the end of every email (address, phone number of the practice...).
    #[serde(default)]
    pub email_footer: String,

    // ---- Preview thumbnails -----------------------------------------------------
    /// Show a small preview image next to every study and series?
    pub thumbnails_enabled: bool,
    /// Memory (MB) the portal may use to remember generated thumbnails. 0 = no cache.
    pub thumbnail_cache_mb: u64,

    // ---- Roles ------------------------------------------------------------------
    pub roles: Vec<Role>,

    // ---- Appearance: colours (both views, the login page and the invitation page) ----
    #[serde(default)]
    pub theme_mode: ThemeMode,
    /// `#rrggbb`, or empty for the built-in colour.
    #[serde(default)]
    pub color_accent: String,
    /// Top bar / header.
    #[serde(default)]
    pub color_header: String,
    /// Icon column of the simplified view.
    #[serde(default)]
    pub color_sidebar: String,
    /// Page background.
    #[serde(default)]
    pub color_background: String,

    // ---- Appearance: the home page and the icons (the "simplified_" names date from when
    // this layout was one of two views; kept so saved settings stay valid) -------------------
    /// Number of studies in the "recent studies" grid of the home page.
    #[serde(default = "default_recent_count")]
    pub simplified_recent_count: u64,
    #[serde(default)]
    pub simplified_recent_order: RecentOrder,
    #[serde(default)]
    pub simplified_recent_layout: RecentLayout,
    #[serde(default)]
    pub simplified_card_size: CardSize,
    /// Show "Total studies: N" in the header.
    #[serde(default = "default_true")]
    pub simplified_show_total: bool,
    /// Show the name under each icon of the column (otherwise only when hovering).
    #[serde(default)]
    pub simplified_show_labels: bool,
    /// Titles of the two areas of the home page.
    #[serde(default = "default_recent_title")]
    pub simplified_recent_title: String,
    #[serde(default = "default_upload_text")]
    pub simplified_upload_text: String,
    /// Automatic refresh of the recent studies (home page) and of the Transfers page.
    #[serde(default = "default_true")]
    pub recent_auto_refresh: bool,
    #[serde(default = "default_true")]
    pub transfers_auto_refresh: bool,
    /// Seconds between two automatic refreshes.
    #[serde(default = "default_auto_refresh_seconds")]
    pub auto_refresh_seconds: u64,

    // ---- Search and lists (Settings > Search & labels) --------------------------------
    /// Columns of the studies tables, in order (keys of [`STUDY_COLUMNS`]).
    #[serde(default = "default_study_columns")]
    pub study_columns: Vec<String>,
    /// Search boxes of the Studies search, in order (keys of [`STUDY_SEARCH_FIELDS`]).
    #[serde(default = "default_study_search_fields")]
    pub study_search_fields: Vec<String>,
    /// Search while typing (after `search_min_chars` letters) instead of with the Search button.
    #[serde(default)]
    pub search_as_you_type: bool,
    #[serde(default = "default_search_min_chars")]
    pub search_min_chars: u64,
    /// Load the next results when scrolling to the end of a list, instead of Previous / Next.
    #[serde(default)]
    pub infinite_scroll: bool,

    // ---- Labels -----------------------------------------------------------------------
    /// The only labels that may be put on studies. Empty = any label.
    #[serde(default)]
    pub available_labels: Vec<String>,
    /// Show how many studies carry each label (one count per label for Orthanc).
    #[serde(default = "default_true")]
    pub label_counts: bool,
    /// Labels at all: when off, no labels column, filter, button or Help section.
    #[serde(default = "default_true")]
    pub labels_enabled: bool,
    /// The fields of the study window, in order (keys of [`STUDY_DETAIL_FIELDS`]).
    #[serde(default = "default_study_detail_fields")]
    pub study_detail_fields: Vec<String>,

    // ---- Optional features (Settings > Features) ------------------------------------
    /// Tick boxes on the lists, to download, send, share, label, view or delete several studies at once.
    #[serde(default = "default_true")]
    pub bulk_actions: bool,
    /// The images of a series, their DICOM tags, a preview of one image and PDF reports.
    #[serde(default = "default_true")]
    pub instance_browser: bool,
    /// Download a single DICOM file (needs the `download` permission).
    #[serde(default = "default_true")]
    pub single_file_download: bool,
    /// Export a series as NIfTI (needs Orthanc's neuro plugin).
    #[serde(default)]
    pub nifti_export: bool,
    /// Delete studies, series and images (needs the `delete` permission).
    #[serde(default)]
    pub allow_delete: bool,
    /// Change tags, move a study to another patient or a series to another study (`edit`).
    #[serde(default)]
    pub allow_modify: bool,
    /// Anonymize a study or series (`edit`).
    #[serde(default)]
    pub allow_anonymize: bool,
    /// Add a series made from PDF files or pictures to a study (`edit`).
    #[serde(default)]
    pub allow_add_series: bool,
    /// One invitation link for several studies.
    #[serde(default = "default_true")]
    pub multi_study_shares: bool,
    /// Send to the Orthanc peers and the DICOMweb servers configured in Orthanc, not only to DICOM nodes.
    #[serde(default)]
    pub send_to_peers: bool,
    #[serde(default)]
    pub send_to_dicomweb: bool,
    /// Search other systems and bring studies in (`retrieve`).
    #[serde(default)]
    pub query_retrieve: bool,
    /// Modality worklists (Orthanc's worklists plugin, `worklists`).
    #[serde(default)]
    pub worklists_enabled: bool,
    /// Configuration > Jobs: every Orthanc job, with cancel / pause / resume / retry.
    #[serde(default = "default_true")]
    pub jobs_page: bool,
    /// Configuration > System: version, storage, counts, plugins, log level, metrics.
    #[serde(default = "default_true")]
    pub system_page: bool,
    /// Configuration > System: read any address of Orthanc's REST API (GET only).
    #[serde(default)]
    pub raw_api: bool,
    /// More viewers, each offered when its Orthanc plugin is there (auto), always or never.
    #[serde(default = "default_ohif_mode")]
    pub stone_viewer: OhifMode,
    #[serde(default = "default_ohif_mode")]
    pub volview_viewer: OhifMode,
    #[serde(default = "default_ohif_mode")]
    pub wsi_viewer: OhifMode,
    /// Address of a MedDream viewer, with `{study_uid}` where the study goes. Empty = no MedDream button.
    #[serde(default)]
    pub meddream_url: String,
    /// The OHIF modes offered (keys of [`OHIF_MODES`]).
    #[serde(default = "default_ohif_modes")]
    pub ohif_modes: Vec<String>,
    /// Name of downloaded ZIP files, with {patient_id}, {patient_name}, {study_date}, {description},
    /// {accession}, {modality}, {id}.
    #[serde(default = "default_download_name")]
    pub download_name_template: String,

    // ---- Formatting (Settings > Appearance) -------------------------------------------
    #[serde(default)]
    pub date_format: DateFormat,
    #[serde(default)]
    pub name_format: NameFormat,
    /// Extra CSS rules added after the portal's own (staff portal and invitation page).
    #[serde(default)]
    pub custom_css: String,
}

fn default_ohif_mode() -> OhifMode {
    OhifMode::Auto
}

fn default_ohif_modes() -> Vec<String> {
    vec!["viewer".into()]
}

fn default_download_name() -> String {
    "{patient_id}_{study_date}_{id}".into()
}

fn default_study_detail_fields() -> Vec<String> {
    ["patient_id", "sex", "birth_date", "study_date", "description", "modalities", "series", "labels"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// The download name of a study: the template filled in, made safe for every file system.
pub fn download_name(template: &str, values: &[(&str, &str)]) -> String {
    let mut name = template.to_string();
    for (key, value) in values {
        name = name.replace(&format!("{{{key}}}"), value);
    }
    let clean = crate::util::sanitize_filename(&name);
    let clean: String = clean.chars().take(120).collect();
    if clean.trim_matches(|c| c == '_' || c == '.').is_empty() {
        "study".into()
    } else {
        clean
    }
}

/// Every column the studies tables can show (Settings > Search & labels), in the order offered.
pub const STUDY_COLUMNS: [&str; 14] = [
    "patient",
    "patient_id",
    "birth_date",
    "sex",
    "other_ids",
    "study_date",
    "description",
    "accession",
    "referring",
    "requesting",
    "institution",
    "modalities",
    "series",
    "labels",
];
/// Every search box the Studies search can offer.
pub const STUDY_SEARCH_FIELDS: [&str; 13] = [
    "patient_name",
    "patient_id",
    "birth_date",
    "sex",
    "other_ids",
    "description",
    "accession",
    "referring",
    "requesting",
    "institution",
    "modality",
    "dates",
    "labels",
];

fn default_study_columns() -> Vec<String> {
    ["patient", "patient_id", "study_date", "description", "modalities", "series", "labels"].map(String::from).to_vec()
}
fn default_study_search_fields() -> Vec<String> {
    ["patient_name", "patient_id", "description", "modality", "dates", "labels"].map(String::from).to_vec()
}
fn default_search_min_chars() -> u64 {
    3
}

fn default_smtp_port() -> u64 {
    587
}
fn default_recent_count() -> u64 {
    12
}
fn default_recent_title() -> String {
    "Recent studies".into()
}
fn default_upload_text() -> String {
    "Drag a file to upload it".into()
}

/// Permissions that existed in earlier versions and no longer do (`switch_view`: choosing between the
/// normal and the simplified view, which became the only one). They are removed from the roles of
/// saved settings before reading them: otherwise the whole saved document would be refused.
const RETIRED_PERMISSIONS: [&str; 1] = ["switch_view"];

fn forget_retired_permissions(stored: &mut Value) {
    if let Some(roles) = stored.get_mut("roles").and_then(Value::as_array_mut) {
        for role in roles {
            if let Some(list) = role.get_mut("permissions").and_then(Value::as_array_mut) {
                list.retain(|p| !p.as_str().is_some_and(|p| RETIRED_PERMISSIONS.contains(&p)));
            }
        }
    }
}

/// `#rrggbb` (or empty = the built-in colour).
fn valid_color(value: &str) -> bool {
    value.is_empty()
        || (value.len() == 7 && value.starts_with('#') && value[1..].chars().all(|c| c.is_ascii_hexdigit()))
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
            share_public_url: String::new(),
            share_default_hours: 48,
            share_max_hours: 720,
            share_default_downloads: 1,
            share_max_downloads: 20,
            share_keep_passwords: true,
            smtp_host: String::new(),
            smtp_port: default_smtp_port(),
            smtp_security: SmtpSecurity::Starttls,
            smtp_username: String::new(),
            smtp_password: Secret::default(),
            smtp_from: String::new(),
            email_footer: String::new(),
            public_base_url: String::new(),
            session_hours: 12,
            weasis_token_minutes: 240,
            cookie_secure: CookieSecure::Auto,
            login_max_failures: 5,
            login_lockout_minutes: 5,
            audit_retention_days: 0,
            modality_buttons: default_modality_buttons(),
            weasis_enabled: true,
            ohif: OhifMode::Auto,
            max_upload_mb: 2048,
            thumbnails_enabled: true,
            thumbnail_cache_mb: 64,
            roles: default_roles(),
            theme_mode: ThemeMode::Auto,
            color_accent: String::new(),
            color_header: String::new(),
            color_sidebar: String::new(),
            color_background: String::new(),
            simplified_recent_count: default_recent_count(),
            simplified_recent_order: RecentOrder::Received,
            simplified_recent_layout: RecentLayout::Table,
            simplified_card_size: CardSize::Medium,
            simplified_show_total: true,
            simplified_show_labels: false,
            simplified_recent_title: default_recent_title(),
            simplified_upload_text: default_upload_text(),
            recent_auto_refresh: true,
            transfers_auto_refresh: true,
            auto_refresh_seconds: default_auto_refresh_seconds(),
            study_columns: default_study_columns(),
            study_search_fields: default_study_search_fields(),
            search_as_you_type: false,
            search_min_chars: default_search_min_chars(),
            infinite_scroll: false,
            available_labels: Vec::new(),
            label_counts: true,
            labels_enabled: true,
            study_detail_fields: default_study_detail_fields(),
            bulk_actions: true,
            instance_browser: true,
            single_file_download: true,
            nifti_export: false,
            allow_delete: false,
            allow_modify: false,
            allow_anonymize: false,
            allow_add_series: false,
            multi_study_shares: true,
            send_to_peers: false,
            send_to_dicomweb: false,
            query_retrieve: false,
            worklists_enabled: false,
            jobs_page: true,
            system_page: true,
            raw_api: false,
            stone_viewer: OhifMode::Auto,
            volview_viewer: OhifMode::Auto,
            wsi_viewer: OhifMode::Auto,
            meddream_url: String::new(),
            ohif_modes: default_ohif_modes(),
            download_name_template: default_download_name(),
            date_format: DateFormat::Iso,
            name_format: NameFormat::LastFirst,
            custom_css: String::new(),
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
        if let Some(v) = env_opt("WEASIS_ENABLED") {
            s.weasis_enabled = match v.to_lowercase().as_str() {
                "true" | "1" | "yes" => true,
                "false" | "0" | "no" => false,
                other => bail!("WEASIS_ENABLED must be true or false (got '{other}')"),
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
            let mut stored: Value = serde_json::from_str(&raw)?;
            forget_retired_permissions(&mut stored);
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

    /// Uses the listening addresses of `seed` (the environment), keeping everything else.
    /// Only the addresses: switching the internet listener on or off stays a setting.
    pub fn take_listen_addresses_from(&mut self, seed: &Settings) {
        self.bind_addr = seed.bind_addr.clone();
        self.remote_bind_addr = seed.remote_bind_addr.clone();
        if self.remote_enabled
            && self.remote_bind_addr.parse::<SocketAddr>().map(|a| a.port()).ok()
                == self.bind_addr.parse::<SocketAddr>().map(|a| a.port()).ok()
        {
            tracing::error!("BIND_ADDR and REMOTE_BIND_ADDR use the same port: the internet listener is switched off");
            self.remote_enabled = false;
        }
    }

    /// Trims text fields and removes trailing slashes from URLs.
    pub fn normalise(&mut self) {
        self.portal_name = self.portal_name.trim().to_string();
        self.browser_title = self.browser_title.trim().to_string();
        self.login_notice = self.login_notice.trim().to_string();
        self.orthanc_url = self.orthanc_url.trim().trim_end_matches('/').to_string();
        self.orthanc_username = self.orthanc_username.trim().to_string();
        // No account means no password: never keep a hidden, unused secret around.
        if self.orthanc_username.is_empty() {
            self.orthanc_password = Secret::default();
        }
        self.bind_addr = self.bind_addr.trim().to_string();
        self.remote_bind_addr = self.remote_bind_addr.trim().to_string();
        self.public_base_url = self.public_base_url.trim().trim_end_matches('/').to_string();
        self.share_public_url = self.share_public_url.trim().trim_end_matches('/').to_string();
        self.smtp_host = self.smtp_host.trim().to_string();
        self.smtp_username = self.smtp_username.trim().to_string();
        self.smtp_from = self.smtp_from.trim().to_string();
        self.email_footer = self.email_footer.trim().to_string();
        // No account on the mail server means no password.
        if self.smtp_username.is_empty() {
            self.smtp_password = Secret::default();
        }
        for button in &mut self.modality_buttons {
            button.label = button.label.trim().to_string();
            button.title = button.title.trim().to_string();
            let mut codes: Vec<String> = Vec::new();
            for code in button.codes.iter().map(|c| c.trim().to_uppercase()).filter(|c| !c.is_empty()) {
                if !codes.contains(&code) {
                    codes.push(code);
                }
            }
            button.codes = codes;
        }
        for color in
            [&mut self.color_accent, &mut self.color_header, &mut self.color_sidebar, &mut self.color_background]
        {
            *color = color.trim().to_lowercase();
        }
        self.simplified_recent_title = self.simplified_recent_title.trim().to_string();
        for label in &mut self.available_labels {
            *label = label.trim().to_string();
        }
        self.available_labels.retain(|l| !l.is_empty());
        self.available_labels.dedup();
        self.simplified_upload_text = self.simplified_upload_text.trim().to_string();
        self.meddream_url = self.meddream_url.trim().to_string();
        self.download_name_template = self.download_name_template.trim().to_string();
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

        // --- email
        if !self.smtp_host.is_empty() {
            let host_ok = self.smtp_host.len() <= 253
                && self
                    .smtp_host
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '[' | ']' | ':'));
            if !host_ok {
                return bad("The mail server must be a host name or an IP address, e.g. smtp.example.com".into());
            }
            if self.smtp_from.parse::<lettre::message::Mailbox>().is_err() {
                return bad("Enter the sender of the emails, e.g. Chalklands PACS <pacs@example.com>".into());
            }
        }
        if !(1..=65535).contains(&self.smtp_port) {
            return bad("The mail server port must be between 1 and 65535".into());
        }
        if has_control(&self.smtp_username) || self.smtp_username.len() > 256 || self.smtp_password.0.len() > 256 {
            return bad("The mail server user name or password is not valid".into());
        }
        // Signing in over an unencrypted connection would send the password in clear on the network.
        if !self.smtp_username.is_empty() && self.smtp_security == SmtpSecurity::None {
            return bad(
                "Signing in to the mail server needs encryption (STARTTLS or TLS): without it the password would \
                 travel unprotected. Choose an encryption, or leave the user name empty for a relay that needs no \
                 sign-in"
                    .into(),
            );
        }
        if self.email_footer.chars().count() > 500 || self.email_footer.chars().any(|c| c.is_control() && c != '\n') {
            return bad("The text at the end of the emails can have at most 500 characters".into());
        }

        // --- sharing
        if !self.share_public_url.is_empty() {
            // It is written into links people will click: allow nothing exotic.
            let chars_ok = self.share_public_url.chars().all(|c| c.is_ascii_alphanumeric() || ":/.-_[]".contains(c));
            if !(self.share_public_url.starts_with("https://") || self.share_public_url.starts_with("http://"))
                || !chars_ok
            {
                return bad("The public address for share links must look like https://pacs.example.com".into());
            }
        }
        if self.share_default_hours > self.share_max_hours {
            return bad("The default validity of a share link cannot be longer than the maximum".into());
        }
        if self.share_default_downloads > self.share_max_downloads {
            return bad("The default number of downloads cannot be larger than the maximum".into());
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
        range("Share link validity, default (hours)", self.share_default_hours, 1, 8760)?;
        range("Share link validity, maximum (hours)", self.share_max_hours, 1, 8760)?;
        range("Share downloads, default", self.share_default_downloads, 0, 1000)?;
        range("Share downloads, maximum", self.share_max_downloads, 0, 1000)?;

        // --- modality buttons
        if self.modality_buttons.len() > 40 {
            return bad("At most 40 modality buttons are allowed".into());
        }
        let mut labels: Vec<String> = Vec::new();
        for button in &self.modality_buttons {
            if !(1..=16).contains(&button.label.chars().count()) || has_control(&button.label) {
                return bad("Each modality button needs a label of 1 to 16 characters".into());
            }
            let label_key = button.label.to_lowercase();
            if labels.contains(&label_key) {
                return bad(format!("The modality button '{}' appears twice", button.label));
            }
            labels.push(label_key);
            // Same rule as the search itself (routes/studies.rs): letters and digits only.
            if button.codes.is_empty()
                || button.codes.len() > 8
                || button.codes.iter().any(|c| c.len() > 16 || !c.chars().all(|ch| ch.is_ascii_alphanumeric()))
            {
                return bad(format!(
                    "The modality button '{}' needs 1 to 8 DICOM codes made of letters and digits, e.g. CT or CR, DX",
                    button.label
                ));
            }
            if button.title.chars().count() > 80 || has_control(&button.title) {
                return bad(format!(
                    "The description of the modality button '{}' can have at most 80 characters",
                    button.label
                ));
            }
        }

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

        // --- appearance
        for (name, color) in [
            ("accent", &self.color_accent),
            ("header", &self.color_header),
            ("icon column", &self.color_sidebar),
            ("background", &self.color_background),
        ] {
            if !valid_color(color) {
                return bad(format!("The {name} colour must look like #1a2b3c (or be empty for the default)"));
            }
        }
        range("Recent studies on the home page", self.simplified_recent_count, 1, 100)?;
        range("Auto-refresh interval (seconds)", self.auto_refresh_seconds, 5, 3600)?;
        // --- optional features and formatting
        let mut seen_fields = Vec::new();
        for f in &self.study_detail_fields {
            if !STUDY_DETAIL_FIELDS.contains(&f.as_str()) || seen_fields.contains(f) {
                return bad(format!("'{f}' is not a field of the study window (or appears twice)"));
            }
            seen_fields.push(f.clone());
        }
        let mut seen_modes = Vec::new();
        for m in &self.ohif_modes {
            if !OHIF_MODES.contains(&m.as_str()) || seen_modes.contains(m) {
                return bad(format!("'{m}' is not an OHIF mode (or appears twice)"));
            }
            seen_modes.push(m.clone());
        }
        if !self.meddream_url.is_empty()
            && (!(self.meddream_url.starts_with("https://") || self.meddream_url.starts_with("http://"))
                || !self.meddream_url.contains("{study_uid}")
                || self.meddream_url.len() > 300
                || self
                    .meddream_url
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control() || c == '"' || c == '<' || c == '>'))
        {
            return bad("The MedDream address must start with http:// or https:// and contain {study_uid}".into());
        }
        if self.download_name_template.is_empty()
            || self.download_name_template.chars().count() > 120
            || has_control(&self.download_name_template)
        {
            return bad("The name of downloaded files needs 1 to 120 characters".into());
        }
        if self.custom_css.len() > 50_000 {
            return bad("The custom CSS can have at most 50,000 characters".into());
        }
        // The rules stay inside a <link>ed style sheet; still, nothing that could close a tag or load from elsewhere.
        let css = self.custom_css.to_lowercase();
        if css.contains("</") || css.contains("@import") || css.contains("url(") || css.contains("expression(") {
            return bad("The custom CSS cannot contain @import, url(...), expression(...) or HTML tags".into());
        }
        if self.simplified_recent_title.chars().count() > 60 || has_control(&self.simplified_recent_title) {
            return bad("The title of the recent studies can have at most 60 characters".into());
        }
        if self.simplified_upload_text.chars().count() > 120 || has_control(&self.simplified_upload_text) {
            return bad("The text of the upload area can have at most 120 characters".into());
        }

        // --- search and labels
        let check_keys = |what: &str, keys: &[String], allowed: &[&str], at_least_one: bool| -> Result<(), AppError> {
            if at_least_one && keys.is_empty() {
                return Err(AppError::BadRequest(format!("Choose at least one {what}")));
            }
            for (i, key) in keys.iter().enumerate() {
                if !allowed.contains(&key.as_str()) {
                    return Err(AppError::BadRequest(format!("Unknown {what} '{key}'")));
                }
                if keys[..i].contains(key) {
                    return Err(AppError::BadRequest(format!("The {what} '{key}' is chosen twice")));
                }
            }
            Ok(())
        };
        check_keys("column", &self.study_columns, &STUDY_COLUMNS, true)?;
        check_keys("search box", &self.study_search_fields, &STUDY_SEARCH_FIELDS, false)?;
        range("Letters before searching while typing", self.search_min_chars, 1, 10)?;
        if self.available_labels.len() > 100 {
            return bad("At most 100 allowed labels".into());
        }
        if let Some(bad_label) = self.available_labels.iter().find(|l| !crate::routes::labels::valid_label(l)) {
            return bad(format!(
                "'{bad_label}' is not a valid label: 1 to 64 letters, digits, '-' or '_' (no spaces or accents)"
            ));
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
        let mut clear_smtp_password = false;
        for (key, value) in patch {
            match key.as_str() {
                "clear_orthanc_password" => clear_password = value.as_bool().unwrap_or(false),
                "clear_smtp_password" => clear_smtp_password = value.as_bool().unwrap_or(false),
                // Read-only information the form echoes back: ignore it.
                "orthanc_password_set" | "smtp_password_set" => {}
                // Write-only secrets: empty = keep the saved one.
                "orthanc_password" | "smtp_password" => {
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
        if clear_smtp_password {
            obj.insert("smtp_password".into(), json!(""));
        }

        let mut updated: Settings = serde_json::from_value(base)
            .map_err(|e| AppError::BadRequest(format!("Invalid value in the settings: {e}")))?;
        updated.normalise();
        updated.validate()?;
        Ok(updated)
    }

    /// The settings as sent to the browser: everything except the Orthanc and mail server
    /// passwords, replaced by yes/no `orthanc_password_set` and `smtp_password_set`.
    pub fn to_view(&self) -> Value {
        let mut value = serde_json::to_value(self).unwrap_or(Value::Null);
        if let Some(obj) = value.as_object_mut() {
            obj.remove("orthanc_password");
            obj.insert("orthanc_password_set".into(), json!(!self.orthanc_password.0.is_empty()));
            obj.remove("smtp_password");
            obj.insert("smtp_password_set".into(), json!(!self.smtp_password.0.is_empty()));
        }
        value
    }

    /// Can the portal send emails? (A mail server and a sender are set.)
    pub fn email_configured(&self) -> bool {
        !self.smtp_host.is_empty() && !self.smtp_from.is_empty()
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
    /// The role of a user: the one stored with the account when it still exists, otherwise
    /// (accounts made before roles were remembered, or a deleted role) the first role whose
    /// permissions are exactly the user's. `None` = custom permissions.
    pub fn role_of(&self, stored: &str, permissions: &PermissionSet) -> Option<&Role> {
        self.roles
            .iter()
            .find(|r| !stored.is_empty() && r.key == stored)
            .or_else(|| self.roles.iter().find(|r| &PermissionSet::from_slice(&r.permissions) == permissions))
    }

    /// The colours, as sent to every page (login and invitation pages included).
    pub fn theme(&self) -> Value {
        json!({
            "mode": self.theme_mode,
            "accent": self.color_accent,
            "header": self.color_header,
            "sidebar": self.color_sidebar,
            "background": self.color_background,
        })
    }

    /// The modality buttons offered on the search pages, in order (hidden ones left out).
    pub fn visible_modality_buttons(&self) -> Vec<ModalityButton> {
        self.modality_buttons.iter().filter(|b| b.visible).cloned().collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped defaults pass their own validation.
    #[test]
    fn the_defaults_are_valid() {
        assert!(Settings::defaults().validate().is_ok());
        assert_eq!(Settings::defaults().visible_modality_buttons().len(), 9);
    }

    /// Modality buttons: codes are tidied up, hidden ones are not offered, bad ones are refused.
    #[test]
    fn modality_buttons_are_tidied_and_checked() {
        let base = Settings::defaults();
        let mut buttons = default_modality_buttons();
        buttons.swap(0, 1); // MR first
        buttons[2].visible = false; // hide US
        buttons[3].codes = vec![" cr ".into(), "dx".into(), "CR".into()];
        let edited = base.with_patch(&json!({ "modality_buttons": buttons })).unwrap();
        let labels: Vec<String> = edited.visible_modality_buttons().into_iter().map(|b| b.label).collect();
        assert_eq!(labels, ["MR", "CT", "RX", "MG", "XA", "RF", "NM", "PT"]);
        assert_eq!(edited.modality_buttons[3].codes, ["CR", "DX"]);
        assert_eq!(edited.modality_buttons.len(), 9, "a hidden button is kept, so it can be shown again");

        let refused = |list: Value| base.with_patch(&json!({ "modality_buttons": list })).is_err();
        assert!(
            refused(json!([{ "label": "CT", "codes": ["CT"] }, { "label": "ct", "codes": ["CT"] }])),
            "same label twice"
        );
        assert!(refused(json!([{ "label": "CT", "codes": [] }])), "no code");
        assert!(refused(json!([{ "label": "CT", "codes": ["C*"] }])), "wildcard in a code");
        assert!(refused(json!([{ "label": "", "codes": ["CT"] }])), "no label");
        // Every button may be hidden: the "Other codes" box is always there.
        assert!(!refused(json!([{ "label": "CT", "codes": ["CT"], "visible": false }])));
    }

    /// A user's role: the stored one, else the role with exactly their permissions, else none.
    #[test]
    fn the_role_of_a_user() {
        let viewer = PermissionSet::from_slice(&[Permission::Read, Permission::Visualize]);
        let custom = PermissionSet::from_slice(&[Permission::Read, Permission::Upload, Permission::Send]);
        let s = Settings::defaults();
        assert_eq!(s.role_of("viewer", &viewer).map(|r| r.key.as_str()), Some("viewer"));
        assert_eq!(s.role_of("", &viewer).map(|r| r.key.as_str()), Some("viewer"), "found by its permissions");
        assert_eq!(
            s.role_of("administrator", &viewer).map(|r| r.key.as_str()),
            Some("administrator"),
            "the stored role wins"
        );
        assert_eq!(s.role_of("gone", &custom).map(|r| r.key.as_str()), None);
    }

    /// Settings saved by earlier versions still load, with everything kept: the switch between two
    /// views (simplified_enabled, default_interface, the view of each role) and the switch_view
    /// permission no longer exist.
    #[test]
    fn settings_from_the_two_views_era_still_load() {
        let mut roles = serde_json::to_value(default_roles()).unwrap();
        roles[0]["interface"] = json!("simplified");
        roles[4]["permissions"].as_array_mut().unwrap().push(json!("switch_view"));
        let stored = json!({
            "portal_name": "Chalklands", "simplified_enabled": true, "default_interface": "simplified",
            "simplified_recent_count": 7, "roles": roles,
        });
        let loaded = Settings::merged_with_stored(Settings::defaults(), Some(stored.to_string()));
        assert_eq!(loaded.portal_name, "Chalklands", "the saved settings were kept, not replaced by the defaults");
        assert_eq!(loaded.simplified_recent_count, 7);
        assert_eq!(loaded.roles[4].permissions, Permission::ALL.to_vec(), "switch_view is dropped");
    }

    /// The mail server password never travels without encryption.
    #[test]
    fn mail_sign_in_needs_encryption() {
        let base = Settings::defaults();
        let mail = |security: &str, user: &str| {
            base.with_patch(&json!({
                "smtp_host": "smtp.example.com", "smtp_from": "PACS <pacs@example.com>",
                "smtp_security": security, "smtp_username": user, "smtp_password": "secret"
            }))
        };
        assert!(mail("none", "mailer").is_err(), "a password over a plain connection");
        assert!(mail("none", "").is_ok(), "a relay without sign-in");
        assert!(mail("starttls", "mailer").is_ok());
        assert!(mail("tls", "mailer").is_ok());
    }

    /// Columns, search boxes and allowed labels are checked.
    #[test]
    fn search_and_label_settings_are_checked() {
        let base = Settings::defaults();
        let ok = |patch: Value| base.with_patch(&patch).is_ok();
        assert!(ok(json!({ "study_columns": ["patient", "accession", "labels"], "study_search_fields": [] })));
        assert!(!ok(json!({ "study_columns": [] })), "at least one column");
        assert!(!ok(json!({ "study_columns": ["patient", "patient"] })), "twice");
        assert!(!ok(json!({ "study_search_fields": ["shoe_size"] })), "unknown");
        assert!(ok(json!({ "available_labels": ["urgent", " teaching "] })));
        assert!(!ok(json!({ "available_labels": ["with space"] })));
        assert!(!ok(json!({ "search_min_chars": 0 })));
        assert_eq!(
            base.with_patch(&json!({ "available_labels": [" a ", "", "b"] })).unwrap().available_labels,
            ["a", "b"]
        );
    }

    /// Colours and texts of the appearance tab are checked.
    #[test]
    fn appearance_values_are_checked() {
        let base = Settings::defaults();
        let ok = |patch: Value| base.with_patch(&patch).is_ok();
        assert!(ok(json!({ "color_accent": " #1A2B3C ", "color_header": "" })));
        assert_eq!(base.with_patch(&json!({ "color_accent": "#1A2B3C" })).unwrap().color_accent, "#1a2b3c");
        assert!(!ok(json!({ "color_accent": "red" })));
        assert!(!ok(json!({ "color_header": "#12345" })));
        assert!(!ok(json!({ "color_background": "url(x)" })));
        assert!(!ok(json!({ "simplified_recent_count": 0 })));
        assert!(!ok(json!({ "simplified_recent_count": 101 })));
        assert_eq!(base.auto_refresh_seconds, 30);
        assert!(ok(json!({ "auto_refresh_seconds": 5, "recent_auto_refresh": false })));
        assert!(!ok(json!({ "auto_refresh_seconds": 4 })));
        assert!(!ok(json!({ "auto_refresh_seconds": 3601 })));
        assert!(ok(json!({ "date_format": "dmy", "name_format": "first_last", "custom_css": ".x { color: red }" })));
        assert!(!ok(json!({ "custom_css": "@import 'http://evil';" })));
        assert!(!ok(json!({ "custom_css": "body { background: url(http://x) }" })));
        assert!(!ok(json!({ "date_format": "yyyy" })));
        assert!(ok(json!({ "meddream_url": "https://md.example.com/?study={study_uid}" })));
        assert!(!ok(json!({ "meddream_url": "javascript:alert(1)//{study_uid}" })));
        assert!(!ok(json!({ "meddream_url": "https://md.example.com/" })), "no {{study_uid}}");
        assert!(ok(json!({ "ohif_modes": ["viewer", "tmtv"] })));
        assert!(!ok(json!({ "ohif_modes": ["teleport"] })));
        assert!(!ok(json!({ "study_detail_fields": ["shoe_size"] })));
        assert!(!ok(json!({ "download_name_template": "" })));
        assert!(!ok(json!({ "theme_mode": "neon" })));
        assert!(!ok(json!({ "simplified_recent_layout": "list" })));
        assert!(ok(json!({ "simplified_recent_order": "study_date" })));
        assert!(ok(
            json!({ "theme_mode": "dark", "simplified_card_size": "large", "simplified_recent_layout": "cards" })
        ));
    }

    #[test]
    fn download_names_follow_the_template() {
        let values = [("patient_id", "P-001"), ("study_date", "20240517"), ("description", "CT/CHEST"), ("id", "aaaa")];
        assert_eq!(download_name("{patient_id}_{study_date}_{id}", &values), "P-001_20240517_aaaa");
        assert!(!download_name("{description}", &values).contains('/'));
        assert_eq!(download_name("{nothing}", &[]), sanitize("{nothing}"));
        assert_eq!(download_name("___", &[]), "download");
    }

    fn sanitize(s: &str) -> String {
        crate::util::sanitize_filename(s)
    }
}
