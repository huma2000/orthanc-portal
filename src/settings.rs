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

/// The language of the interface (Settings > Appearance). Each person can still pick another one
/// in their browser; invitation emails and the invitation page use this one by default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    /// English
    #[default]
    En,
    /// Spanish (Spain)
    Es,
}

impl Language {
    /// `"en"` / `"es"`, as in the settings and the translation file names.
    pub fn code(self) -> &'static str {
        match self {
            Language::En => "en",
            Language::Es => "es",
        }
    }

    /// What a browser without JavaScript shows (the pages are built by JavaScript).
    pub fn noscript(self) -> &'static str {
        match self {
            Language::En => "This page needs JavaScript.",
            Language::Es => "Esta página necesita JavaScript.",
        }
    }

    /// `"es"`, `"es-ES"`, `"ES"`... (anything else, or empty = none).
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().get(..2) {
            Some("en") => Some(Language::En),
            Some("es") => Some(Language::Es),
            _ => None,
        }
    }
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

/// The key of the default Orthanc server: the one of `orthanc_url`, `orthanc_username`...
/// Its resources keep their plain Orthanc IDs; those of the other servers are written `key:id`
/// (see `util::ResourceRef`).
pub const MAIN_SERVER: &str = "main";

/// At most this many Orthanc servers besides the default one.
pub const MAX_EXTRA_SERVERS: usize = 7;

/// One Orthanc server the portal works with. The default server is described by the
/// `orthanc_*` settings (so older versions and the environment variables keep working); the
/// others are in `orthanc_servers`. [`Settings::servers`] lists them all as this type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrthancServer {
    /// Short name used in IDs and links: `a-z`, `0-9` and `-`, 1 to 24 characters, never `main`.
    pub key: String,
    /// The name people see ("Archive 2015-2020"). Empty = the key.
    #[serde(default)]
    pub label: String,
    /// REST API of that Orthanc, no trailing slash.
    pub url: String,
    /// Account the portal uses there. Empty = no account (see `orthanc_username`).
    #[serde(default)]
    pub username: String,
    /// Its password: write-only, like `orthanc_password`.
    #[serde(default)]
    pub password: Secret,
    /// The folder of that Orthanc's log files, as seen by the portal (empty = not shown).
    #[serde(default)]
    pub log_dir: String,
    /// Colour of its badge, `#rrggbb` (empty = the default).
    #[serde(default)]
    pub color: String,
    /// Switched off: kept in the settings, but the portal does not use it.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Included when people search without choosing servers.
    #[serde(default = "yes")]
    pub searched_by_default: bool,
}

fn yes() -> bool {
    true
}

impl OrthancServer {
    /// The name to show: the label, or the key when there is none.
    pub fn name(&self) -> &str {
        if self.label.is_empty() {
            &self.key
        } else {
            &self.label
        }
    }
}

/// Checks the address and account of an Orthanc server (the default one or another).
fn validate_orthanc_access(url: &str, username: &str, password: &str) -> Result<(), String> {
    let url_ok = (url.starts_with("http://") || url.starts_with("https://"))
        && !url.chars().any(|c| c.is_whitespace() || c.is_control());
    if !url_ok {
        return Err("The Orthanc address must look like http://orthanc:8042 or https://...".into());
    }
    if username.chars().any(|c| c.is_control()) || username.len() > 128 || password.len() > 256 {
        return Err("The Orthanc user name or password is not valid".into());
    }
    Ok(())
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

/// How the portal signs in to the mail server (Settings > Email).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SmtpAuth {
    /// A user name and password (or none at all, for a relay that needs no sign-in).
    #[default]
    Password,
    /// Microsoft 365 / Exchange Online with OAuth 2.0 (XOAUTH2): an app registered in Microsoft
    /// Entra ID signs in as a mailbox, with a token it gets from its client secret. Microsoft 365
    /// no longer accepts a plain password for SMTP.
    Microsoft365,
}

/// The token address of Microsoft Entra ID for a tenant (when no other one is set).
pub const MICROSOFT_TOKEN_URL: &str = "https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token";
/// What the token is for: sending mail through Exchange Online (SMTP.SendAsApp).
pub const MICROSOFT_SMTP_SCOPE: &str = "https://outlook.office365.com/.default";

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

/// Two-step sign-in (a 6-digit code from an authenticator app after the password).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TwoFactorMode {
    /// Not used: nobody is asked for a code (codes set up earlier are kept, but not asked).
    #[default]
    Off,
    /// Each person may switch it on for their own account (My account).
    Optional,
    /// Everybody must use it: an account without it sets it up at its next sign-in.
    Required,
}

/// One automatic forwarding rule (Settings > Auto-forward): every study that arrives and matches
/// is sent to `target`. Empty conditions match everything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardRule {
    /// Shown in the Transfers list ("auto: <name>") and in the audit log.
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Any of these modality codes is in the study (empty = any). RX = CR + DX.
    #[serde(default)]
    pub modalities: Vec<String>,
    /// The AE title of the system that sent the study to Orthanc (empty = any; `*` wildcards).
    #[serde(default)]
    pub calling_aet: String,
    /// The institution name contains this text (empty = any; case does not matter).
    #[serde(default)]
    pub institution: String,
    /// The study description contains this text (empty = any; case does not matter).
    #[serde(default)]
    pub description: String,
    /// The study carries any of these labels when it becomes stable (empty = any).
    #[serde(default)]
    pub labels: Vec<String>,
    /// The destination: a DICOM node alias, `peer:<name>` or `dicomweb:<name>` (see routes/send.rs).
    /// Empty = the study is not sent anywhere (a rule that only notifies).
    #[serde(default)]
    pub target: String,
    /// Email these addresses ("a@b.org, c@d.org") when a study matches: once per study and rule.
    /// Empty = nobody. The email never names the patient.
    #[serde(default)]
    pub notify: String,
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
    /// The Orthanc servers its users may use, by key: `None` = all of them. Given to the users
    /// of the role like its permissions.
    #[serde(default)]
    pub servers: Option<Vec<String>>,
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
    /// `#rrggbb` of the button and of the modality tags of its codes; empty = the built-in colours.
    #[serde(default)]
    pub color: String,
}

/// The modality buttons every new installation starts with.
pub fn default_modality_buttons() -> Vec<ModalityButton> {
    let button = |label: &str, codes: &[&str], title: &str| ModalityButton {
        label: label.into(),
        codes: codes.iter().map(|c| c.to_string()).collect(),
        title: title.into(),
        visible: true,
        color: String::new(),
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
        servers: None,
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
    /// The folder where Orthanc writes its log files (its `--logdir`), as seen by the portal.
    /// Empty = the System page cannot show Orthanc's log (Orthanc's REST API cannot read it).
    #[serde(default)]
    pub orthanc_log_dir: String,
    /// The name of the default Orthanc server when there are several ("Main PACS"). Empty = "Main".
    #[serde(default)]
    pub orthanc_label: String,
    /// The other Orthanc servers (the default one is described above, with the key `main`).
    #[serde(default)]
    pub orthanc_servers: Vec<OrthancServer>,

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
    /// The Share window offers "share an anonymized copy" (made in Orthanc, deleted when the link ends).
    #[serde(default = "default_true")]
    pub share_anonymize: bool,
    /// The Share window offers "email me when the link is opened or downloaded" (needs a mail server
    /// and an email address on the account).
    #[serde(default = "default_true")]
    pub share_notify: bool,
    /// "Send the invitation by email" starts ticked in the windows that create a link (share and
    /// upload links). Only matters when a mail server is set up. Off by default.
    #[serde(default)]
    pub share_email_default: bool,
    /// The Share window offers "every study of this patient, in one link" (also needs `multi_study_shares`).
    #[serde(default = "default_true")]
    pub share_patient: bool,
    /// The Shares page offers "every study with a label, in one link" (also needs `multi_study_shares`).
    #[serde(default = "default_true")]
    pub share_label: bool,
    /// QR codes of share and upload links (Share window, Shares list).
    #[serde(default = "default_true")]
    pub share_qr: bool,

    // ---- Upload links: people outside SEND studies in (Settings > Features) -----------
    /// Staff with the `share` and `upload` permissions may create upload links.
    #[serde(default)]
    pub upload_links: bool,
    /// Suggested validity of a new upload link (hours; at most `share_max_hours`).
    #[serde(default = "default_upload_link_hours")]
    pub upload_link_default_hours: u64,
    /// The most an upload link may receive in total (MB).
    #[serde(default = "default_upload_link_max_mb")]
    pub upload_link_max_mb: u64,
    /// Label put on the studies received through upload links (empty = none).
    #[serde(default = "default_upload_link_label")]
    pub upload_link_label: String,

    // ---- Two-step sign-in (Settings > Security & roles) -------------------------------
    #[serde(default)]
    pub two_factor: TwoFactorMode,
    /// My account: each person may change their own password and email address. Off = only an
    /// administrator can (Users); My account then only holds two-step sign-in, when it is used.
    #[serde(default = "default_true")]
    pub account_self_service: bool,

    // ---- Auto-forward (Settings > Auto-forward) ---------------------------------------
    /// The rules are applied (they are kept when this is off).
    #[serde(default)]
    pub forward_enabled: bool,
    #[serde(default)]
    pub forward_rules: Vec<ForwardRule>,

    // ---- Patient CD / USB export (Settings > Features) --------------------------------
    /// "Patient CD / USB" in the download menus: DICOMDIR + a page for the patient + a viewer.
    #[serde(default = "default_true")]
    pub cd_export: bool,
    /// Folder (on the portal's computer) with a portable DICOM viewer, copied onto every patient
    /// CD / USB export. Empty = no viewer, only the images and the page for the patient.
    #[serde(default)]
    pub cd_viewer_dir: String,

    // ---- Backups of the portal's own database (Settings > Backups) ---------------------
    /// Backups at all: automatic ones, "Back up now" and "Download a copy". Off = none of them.
    #[serde(default = "default_true")]
    pub backups_enabled: bool,
    /// Folder where copies of the database are written. Empty = no automatic backups.
    #[serde(default)]
    pub backup_dir: String,
    /// Hours between two automatic backups.
    #[serde(default = "default_backup_hours")]
    pub backup_interval_hours: u64,
    /// How many automatic backups are kept (the oldest are deleted).
    #[serde(default = "default_backup_keep")]
    pub backup_keep: u64,

    // ---- Home page: what each person looked at last -------------------------------------
    #[serde(default = "default_true")]
    pub recently_viewed: bool,
    #[serde(default = "default_recently_viewed_count")]
    pub recently_viewed_count: u64,
    /// Configuration > Statistics (administrators).
    #[serde(default = "default_true")]
    pub stats_page: bool,

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
    /// Password, or Microsoft 365 OAuth (then `smtp_username` is the mailbox it signs in as).
    #[serde(default)]
    pub smtp_auth: SmtpAuth,
    /// Microsoft 365: Directory (tenant) ID, or the tenant's domain (contoso.onmicrosoft.com).
    #[serde(default)]
    pub smtp_oauth_tenant: String,
    /// Microsoft 365: Application (client) ID of the app registration.
    #[serde(default)]
    pub smtp_oauth_client_id: String,
    /// Microsoft 365: a client secret of that app (its Value). Never sent to the browser.
    #[serde(default)]
    pub smtp_oauth_client_secret: Secret,
    /// Advanced: another token address (national clouds...). Empty = Microsoft's, for the tenant.
    #[serde(default)]
    pub smtp_oauth_token_url: String,
    /// Advanced: another scope. Empty = Exchange Online's ([`MICROSOFT_SMTP_SCOPE`]).
    #[serde(default)]
    pub smtp_oauth_scope: String,
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
    /// Which upgrades of `study_columns` saved settings already had: 1 = the Server column was
    /// offered (added once at the end, so people who use several servers see it; it can then be
    /// moved or hidden like the others).
    #[serde(default)]
    pub columns_version: u32,
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
    /// The colour of some labels (`#rrggbb`), for their tags and filter buttons; the others keep the accent.
    #[serde(default)]
    pub label_colors: std::collections::BTreeMap<String, String>,
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
    /// The language of the interface, the invitation page and the invitation emails.
    #[serde(default)]
    pub language: Language,
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
pub const STUDY_COLUMNS: [&str; 15] = [
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
    // The Orthanc server(s) holding the study: shown only to people who use several servers.
    "server",
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
    ["patient", "patient_id", "study_date", "description", "modalities", "series", "labels", "server"]
        .map(String::from)
        .to_vec()
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
fn default_upload_link_hours() -> u64 {
    168
}
fn default_upload_link_max_mb() -> u64 {
    4096
}
fn default_upload_link_label() -> String {
    "received".into()
}
fn default_backup_hours() -> u64 {
    24
}
fn default_backup_keep() -> u64 {
    7
}
fn default_recently_viewed_count() -> u64 {
    8
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

/// Saved settings from before the Server column: it is added once at the end of their columns
/// (afterwards, hiding it is respected).
fn offer_server_column(stored: &mut Value) {
    let Some(obj) = stored.as_object_mut() else { return };
    if obj.contains_key("columns_version") {
        return;
    }
    if let Some(columns) = obj.get_mut("study_columns").and_then(Value::as_array_mut) {
        if !columns.iter().any(|c| c == "server") {
            columns.push(json!("server"));
        }
    }
    obj.insert("columns_version".into(), json!(1));
}

fn forget_retired_permissions(stored: &mut Value) {
    if let Some(roles) = stored.get_mut("roles").and_then(Value::as_array_mut) {
        for role in roles {
            if let Some(list) = role.get_mut("permissions").and_then(Value::as_array_mut) {
                list.retain(|p| !p.as_str().is_some_and(|p| RETIRED_PERMISSIONS.contains(&p)));
            }
        }
    }
}

/// A list of server keys as typed: trimmed, without empty entries or repeats (order kept).
pub fn tidy_server_keys(keys: &mut Vec<String>) {
    let mut seen: Vec<String> = Vec::new();
    for key in keys.drain(..) {
        let key = key.trim().to_string();
        if !key.is_empty() && !seen.contains(&key) {
            seen.push(key);
        }
    }
    *keys = seen;
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
            orthanc_log_dir: String::new(),
            orthanc_label: String::new(),
            orthanc_servers: Vec::new(),
            bind_addr: "0.0.0.0:8080".into(),
            remote_enabled: true,
            remote_bind_addr: "0.0.0.0:8081".into(),
            share_public_url: String::new(),
            share_default_hours: 48,
            share_max_hours: 720,
            share_default_downloads: 1,
            share_max_downloads: 20,
            share_keep_passwords: true,
            share_anonymize: true,
            share_notify: true,
            share_email_default: false,
            share_patient: true,
            share_label: true,
            share_qr: true,
            backups_enabled: true,
            account_self_service: true,
            upload_links: false,
            upload_link_default_hours: default_upload_link_hours(),
            upload_link_max_mb: default_upload_link_max_mb(),
            upload_link_label: default_upload_link_label(),
            two_factor: TwoFactorMode::Off,
            forward_enabled: false,
            forward_rules: Vec::new(),
            cd_export: true,
            cd_viewer_dir: String::new(),
            backup_dir: String::new(),
            backup_interval_hours: default_backup_hours(),
            backup_keep: default_backup_keep(),
            recently_viewed: true,
            recently_viewed_count: default_recently_viewed_count(),
            stats_page: true,
            smtp_host: String::new(),
            smtp_port: default_smtp_port(),
            smtp_security: SmtpSecurity::Starttls,
            smtp_username: String::new(),
            smtp_password: Secret::default(),
            smtp_auth: SmtpAuth::Password,
            smtp_oauth_tenant: String::new(),
            smtp_oauth_client_id: String::new(),
            smtp_oauth_client_secret: Secret::default(),
            smtp_oauth_token_url: String::new(),
            smtp_oauth_scope: String::new(),
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
            columns_version: 1,
            study_search_fields: default_study_search_fields(),
            search_as_you_type: false,
            search_min_chars: default_search_min_chars(),
            infinite_scroll: false,
            available_labels: Vec::new(),
            label_counts: true,
            label_colors: Default::default(),
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
            language: Language::En,
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
        if let Some(v) = env_opt("ORTHANC_LOG_DIR") {
            s.orthanc_log_dir = v;
        }
        if let Some(v) = env_opt("PORTAL_LANGUAGE") {
            s.language = Language::parse(&v).ok_or_else(|| anyhow!("PORTAL_LANGUAGE must be en or es"))?;
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
        // Docker: a folder of the host mounted for the backups / the portable viewer of patient CDs.
        if let Some(v) = env_opt("BACKUP_DIR") {
            s.backup_dir = v;
        }
        if let Some(v) = env_opt("CD_VIEWER_DIR") {
            s.cd_viewer_dir = v;
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
            offer_server_column(&mut stored);
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
        self.orthanc_log_dir = self.orthanc_log_dir.trim().to_string();
        self.orthanc_username = self.orthanc_username.trim().to_string();
        // No account means no password: never keep a hidden, unused secret around.
        if self.orthanc_username.is_empty() {
            self.orthanc_password = Secret::default();
        }
        self.orthanc_label = self.orthanc_label.trim().to_string();
        for server in self.orthanc_servers.iter_mut() {
            server.key = server.key.trim().to_ascii_lowercase();
            server.label = server.label.trim().to_string();
            server.url = server.url.trim().trim_end_matches('/').to_string();
            server.username = server.username.trim().to_string();
            server.log_dir = server.log_dir.trim().to_string();
            server.color = server.color.trim().to_ascii_lowercase();
            if server.username.is_empty() {
                server.password = Secret::default();
            }
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
        if self.smtp_username.is_empty() && self.smtp_auth == SmtpAuth::Password {
            self.smtp_password = Secret::default();
        }
        self.smtp_oauth_tenant = self.smtp_oauth_tenant.trim().to_string();
        self.smtp_oauth_client_id = self.smtp_oauth_client_id.trim().to_string();
        self.smtp_oauth_client_secret.0 = self.smtp_oauth_client_secret.0.trim().to_string();
        self.smtp_oauth_token_url = self.smtp_oauth_token_url.trim().to_string();
        self.smtp_oauth_scope = self.smtp_oauth_scope.trim().to_string();
        for button in &mut self.modality_buttons {
            button.label = button.label.trim().to_string();
            button.title = button.title.trim().to_string();
            button.color = button.color.trim().to_lowercase();
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
        // Colours: lower case; a label set back to "Default" (empty) is simply forgotten.
        self.label_colors = std::mem::take(&mut self.label_colors)
            .into_iter()
            .map(|(label, color)| (label.trim().to_string(), color.trim().to_lowercase()))
            .filter(|(label, color)| !label.is_empty() && !color.is_empty())
            .collect();
        self.available_labels.dedup();
        self.simplified_upload_text = self.simplified_upload_text.trim().to_string();
        self.meddream_url = self.meddream_url.trim().to_string();
        self.download_name_template = self.download_name_template.trim().to_string();
        self.upload_link_label = self.upload_link_label.trim().to_string();
        self.cd_viewer_dir = self.cd_viewer_dir.trim().to_string();
        self.backup_dir = self.backup_dir.trim().to_string();
        for rule in &mut self.forward_rules {
            rule.name = rule.name.trim().to_string();
            rule.calling_aet = rule.calling_aet.trim().to_string();
            rule.institution = rule.institution.trim().to_string();
            rule.description = rule.description.trim().to_string();
            rule.target = rule.target.trim().to_string();
            rule.notify = match crate::mailer::parse_recipients(&rule.notify) {
                Ok(to) => crate::mailer::addresses(&to),
                Err(_) => rule.notify.trim().to_string(), // empty, or refused by validate()
            };
            let mut labels: Vec<String> = Vec::new();
            for label in rule.labels.iter().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()) {
                if !labels.contains(&label) {
                    labels.push(label);
                }
            }
            rule.labels = labels;
            let mut codes: Vec<String> = Vec::new();
            for code in rule.modalities.iter().map(|c| c.trim().to_uppercase()).filter(|c| !c.is_empty()) {
                if !codes.contains(&code) {
                    codes.push(code);
                }
            }
            rule.modalities = codes;
        }
        for role in &mut self.roles {
            role.key = role.key.trim().to_string();
            role.label = role.label.trim().to_string();
            role.permissions.sort();
            role.permissions.dedup();
            if let Some(servers) = &mut role.servers {
                tidy_server_keys(servers);
            }
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
        validate_orthanc_access(&self.orthanc_url, &self.orthanc_username, &self.orthanc_password.0)
            .map_err(AppError::BadRequest)?;
        if self.orthanc_label.chars().count() > 60 || has_control(&self.orthanc_label) {
            return bad("The name of an Orthanc server can have at most 60 characters".into());
        }
        if self.orthanc_servers.len() > MAX_EXTRA_SERVERS {
            return bad(format!("At most {} Orthanc servers can be added", MAX_EXTRA_SERVERS));
        }
        let mut keys: Vec<&str> = vec![MAIN_SERVER];
        for server in &self.orthanc_servers {
            let key_ok = !server.key.is_empty()
                && server.key.len() <= 24
                && server.key.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
            if !key_ok {
                return bad(format!(
                    "The key of the Orthanc server '{}' must have 1 to 24 letters a-z, digits or '-'",
                    server.key
                ));
            }
            if keys.contains(&server.key.as_str()) {
                return bad(format!("Two Orthanc servers have the key '{}'", server.key));
            }
            keys.push(&server.key);
            let name = server.name();
            if server.label.chars().count() > 60 || has_control(&server.label) {
                return bad("The name of an Orthanc server can have at most 60 characters".into());
            }
            validate_orthanc_access(&server.url, &server.username, &server.password.0)
                .map_err(|e| AppError::BadRequest(format!("{name}: {e}")))?;
            if server.log_dir.chars().count() > 300 || has_control(&server.log_dir) {
                return bad("The folder of Orthanc's log files can have at most 300 characters".into());
            }
            if !valid_color(&server.color) {
                return bad(format!("The colour of the Orthanc server '{name}' must look like #1a2b3c"));
            }
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
        if self.smtp_auth == SmtpAuth::Microsoft365 && !self.smtp_host.is_empty() {
            self.validate_microsoft365()?;
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
            if !valid_color(&button.color) {
                return bad(format!(
                    "The colour of the modality button '{}' must look like #1a2b3c (or be empty for the default)",
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
            self.validate_server_access(&role.servers)
                .map_err(|e| AppError::BadRequest(format!("Role \"{}\": {e}", role.label)))?;
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
        if self.orthanc_log_dir.chars().count() > 300 || has_control(&self.orthanc_log_dir) {
            return bad("The folder of Orthanc's log files can have at most 300 characters".into());
        }
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
        // A backslash is refused too: CSS escapes could spell those words differently (`u\72l(` is `url(`).
        let css = self.custom_css.to_lowercase();
        if ["</", "\\", "@import", "url(", "image-set(", "image(", "src(", "expression("]
            .iter()
            .any(|bad| css.contains(bad))
        {
            return bad(
                "The custom CSS cannot contain @import, url(...), image-set(...), expression(...), backslashes or HTML tags"
                    .into(),
            );
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
        if self.label_colors.len() > 200 {
            return bad("At most 200 label colours".into());
        }
        for (label, color) in &self.label_colors {
            if !crate::routes::labels::valid_label(label) {
                return bad(format!("'{label}' is not a valid label (label colours)"));
            }
            if !valid_color(color) {
                return bad(format!("The colour of the label '{label}' must look like #1a2b3c"));
            }
        }
        if let Some(bad_label) = self.available_labels.iter().find(|l| !crate::routes::labels::valid_label(l)) {
            return bad(format!(
                "'{bad_label}' is not a valid label: 1 to 64 letters, digits, '-' or '_' (no spaces or accents)"
            ));
        }

        // --- upload links
        // (Never longer than the maximum of share links: that cap is applied when a link is made, so
        // lowering the maximum never makes the whole form fail.)
        range("Upload link validity, default (hours)", self.upload_link_default_hours, 1, 8760)?;
        range("Upload link size limit (MB)", self.upload_link_max_mb, 1, 1_048_576)?;
        if !self.upload_link_label.is_empty() && !crate::routes::labels::valid_label(&self.upload_link_label) {
            return bad(format!(
                "'{}' is not a valid label for received studies: 1 to 64 letters, digits, '-' or '_'",
                self.upload_link_label
            ));
        }

        // --- folders on the portal's computer (backups, the viewer of patient CDs)
        for (what, dir) in [("backup folder", &self.backup_dir), ("folder of the CD viewer", &self.cd_viewer_dir)] {
            if dir.is_empty() {
                continue;
            }
            if dir.chars().count() > 300 || has_control(dir) || !std::path::Path::new(dir).is_absolute() {
                return bad(format!(
                    "The {what} must be a full path (e.g. /data/backups or D:\\Backups), at most 300 characters"
                ));
            }
        }
        range("Hours between two backups", self.backup_interval_hours, 1, 24 * 30)?;
        range("Backups to keep", self.backup_keep, 1, 365)?;
        range("Recently viewed studies on the home page", self.recently_viewed_count, 1, 30)?;

        // --- auto-forward rules
        if self.forward_rules.len() > 50 {
            return bad("At most 50 forwarding rules".into());
        }
        for rule in &self.forward_rules {
            if !(1..=60).contains(&rule.name.chars().count()) || has_control(&rule.name) {
                return bad("Each forwarding rule needs a name of 1 to 60 characters".into());
            }
            if rule.target.is_empty() && rule.notify.is_empty() {
                return bad(format!(
                    "The rule '{}' must send the study somewhere or notify somebody by email (or both)",
                    rule.name
                ));
            }
            if rule.target.len() > 80
                || !rule.target.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
            {
                return bad(format!("The forwarding rule '{}' needs a destination", rule.name));
            }
            if !rule.notify.is_empty() {
                if let Err(e) = crate::mailer::parse_recipients(&rule.notify) {
                    return bad(format!("The rule '{}': {e}", rule.name));
                }
            }
            if rule.labels.len() > 20 || rule.labels.iter().any(|l| !crate::routes::labels::valid_label(l)) {
                return bad(format!(
                    "The labels of the rule '{}' are made of letters, digits, '-' or '_' (at most 20)",
                    rule.name
                ));
            }
            if rule.modalities.len() > 20
                || rule.modalities.iter().any(|c| c.len() > 16 || !c.chars().all(|ch| ch.is_ascii_alphanumeric()))
            {
                return bad(format!(
                    "The modalities of the forwarding rule '{}' are DICOM codes made of letters and digits, e.g. CT, MR",
                    rule.name
                ));
            }
            let aet_ok = rule.calling_aet.len() <= 16
                && rule.calling_aet.chars().all(|c| c.is_ascii_graphic() && c != '\\' || c == ' ');
            if !aet_ok {
                return bad(format!("The calling AE title of the rule '{}' has at most 16 characters", rule.name));
            }
            if [&rule.institution, &rule.description].iter().any(|t| t.chars().count() > 64 || has_control(t)) {
                return bad(format!("The texts of the rule '{}' have at most 64 characters", rule.name));
            }
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
                "orthanc_password_set" | "smtp_password_set" | "smtp_oauth_client_secret_set" => {}
                // Write-only secrets: empty = keep the saved one.
                "orthanc_password" | "smtp_password" | "smtp_oauth_client_secret" => {
                    if value.as_str().map(|p| !p.is_empty()).unwrap_or(false) {
                        obj.insert(key.clone(), value.clone());
                    }
                }
                // The other Orthanc servers: their passwords are write-only too.
                "orthanc_servers" => {
                    obj.insert(key.clone(), self.servers_with_saved_passwords(value)?);
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
        self.check_secret_destinations(&updated, patch, clear_password, clear_smtp_password)?;
        updated.validate()?;
        Ok(updated)
    }

    /// The `orthanc_servers` of a save, with each server's saved password kept when the form sends
    /// none (the page never receives passwords), unless `"clear_password": true` removes it.
    fn servers_with_saved_passwords(&self, value: &Value) -> Result<Value, AppError> {
        let list =
            value.as_array().ok_or_else(|| AppError::BadRequest("orthanc_servers must be a list of servers".into()))?;
        let mut out = Vec::with_capacity(list.len());
        for entry in list {
            let mut server = entry
                .as_object()
                .cloned()
                .ok_or_else(|| AppError::BadRequest("Each Orthanc server must be an object".into()))?;
            server.remove("password_set");
            let clear = server.remove("clear_password").and_then(|v| v.as_bool()).unwrap_or(false);
            let typed = server.get("password").and_then(Value::as_str).is_some_and(|p| !p.is_empty());
            if !typed {
                let key = server.get("key").and_then(Value::as_str).unwrap_or_default().trim().to_ascii_lowercase();
                let saved = self.orthanc_servers.iter().find(|s| s.key == key).map(|s| s.password.0.clone());
                server.insert("password".into(), json!(if clear { String::new() } else { saved.unwrap_or_default() }));
            }
            out.push(Value::Object(server));
        }
        Ok(Value::Array(out))
    }

    /// A saved secret is only sent to the server it was entered for: when that server's address
    /// changes, the secret must be typed again (or removed). Otherwise anyone at an administrator's
    /// browser could point the address at their own server and receive the saved password, which
    /// the settings page never shows.
    fn check_secret_destinations(
        &self,
        updated: &Settings,
        patch: &serde_json::Map<String, Value>,
        clear_password: bool,
        clear_smtp_password: bool,
    ) -> Result<(), AppError> {
        let given = |key: &str| patch.get(key).and_then(Value::as_str).is_some_and(|s| !s.is_empty());
        let origin = |url: &str| {
            reqwest::Url::parse(url).map(|u| u.origin().ascii_serialization()).unwrap_or_else(|_| url.to_lowercase())
        };
        let bad = |m: &str| Err(AppError::BadRequest(m.into()));
        if !self.orthanc_password.0.is_empty()
            && !updated.orthanc_password.0.is_empty()
            && !clear_password
            && !given("orthanc_password")
            && origin(&self.orthanc_url) != origin(&updated.orthanc_url)
        {
            return bad("The Orthanc address changed: enter its password again, or tick \"Remove the saved password\" (a saved password is only sent to the server it was entered for)");
        }
        if !self.smtp_password.0.is_empty()
            && !updated.smtp_password.0.is_empty()
            && !clear_smtp_password
            && !given("smtp_password")
            && !updated.smtp_host.is_empty()
            && !self.smtp_host.eq_ignore_ascii_case(&updated.smtp_host)
        {
            return bad("The mail server changed: enter its password again, or tick \"Remove the saved password\" (a saved password is only sent to the server it was entered for)");
        }
        if !self.smtp_oauth_client_secret.0.is_empty()
            && updated.smtp_auth == SmtpAuth::Microsoft365
            && !given("smtp_oauth_client_secret")
            && origin(&crate::mailer::token_endpoint(self).0) != origin(&crate::mailer::token_endpoint(updated).0)
        {
            return bad("The token address changed: enter the client secret again (a saved secret is only sent to the server it was entered for)");
        }
        // The other Orthanc servers, each with its own saved password.
        let typed: Vec<String> = patch
            .get("orthanc_servers")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter(|s| s.get("password").and_then(Value::as_str).is_some_and(|p| !p.is_empty()))
                    .filter_map(|s| s.get("key").and_then(Value::as_str).map(|k| k.trim().to_ascii_lowercase()))
                    .collect()
            })
            .unwrap_or_default();
        for after in &updated.orthanc_servers {
            let Some(before) = self.orthanc_servers.iter().find(|s| s.key == after.key) else { continue };
            if !before.password.0.is_empty()
                && !after.password.0.is_empty()
                && !typed.contains(&after.key)
                && origin(&before.url) != origin(&after.url)
            {
                return Err(AppError::BadRequest(format!(
                    "The address of the Orthanc server '{}' changed: enter its password again, or remove it (a saved password is only sent to the server it was entered for)",
                    after.name()
                )));
            }
        }
        Ok(())
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
            obj.remove("smtp_oauth_client_secret");
            obj.insert("smtp_oauth_client_secret_set".into(), json!(!self.smtp_oauth_client_secret.0.is_empty()));
            if let Some(Value::Array(servers)) = obj.get_mut("orthanc_servers") {
                for server in servers.iter_mut().filter_map(Value::as_object_mut) {
                    let set = server.remove("password").and_then(|p| p.as_str().map(|p| !p.is_empty()));
                    server.insert("password_set".into(), json!(set.unwrap_or(false)));
                }
            }
        }
        value
    }

    /// The Microsoft 365 (OAuth) sign-in of Settings > Email is complete and makes sense.
    fn validate_microsoft365(&self) -> Result<(), AppError> {
        let bad = |m: &str| Err(AppError::BadRequest(m.into()));
        let has_control = |s: &str| s.chars().any(|c| c.is_control());
        if self.smtp_security == SmtpSecurity::None {
            return bad("Microsoft 365 needs encryption: choose STARTTLS (smtp.office365.com, port 587)");
        }
        if self.smtp_username.parse::<lettre::Address>().is_err() {
            return bad(
                "With Microsoft 365, the user name is the mailbox the portal sends from, e.g. pacs@example.com",
            );
        }
        let id_like = |v: &str, extra: &[char]| {
            !v.is_empty()
                && v.len() <= 128
                && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || extra.contains(&c))
        };
        if !id_like(&self.smtp_oauth_tenant, &['.']) {
            return bad("Enter the Directory (tenant) ID of Microsoft 365 (Entra ID > Overview), or its domain, e.g. contoso.onmicrosoft.com");
        }
        if !id_like(&self.smtp_oauth_client_id, &[]) {
            return bad("Enter the Application (client) ID of the app registered in Microsoft Entra ID");
        }
        if self.smtp_oauth_client_secret.0.is_empty() || self.smtp_oauth_client_secret.0.len() > 512 {
            return bad(
                "Enter a client secret of the app (Entra ID > App registrations > Certificates & secrets: its Value)",
            );
        }
        if !self.smtp_oauth_token_url.is_empty() {
            let url = self.smtp_oauth_token_url.as_str();
            let local = url.starts_with("http://127.0.0.1:") || url.starts_with("http://localhost:");
            if url.len() > 500 || has_control(url) || url.contains(' ') || !(url.starts_with("https://") || local) {
                return bad("The token address must start with https://");
            }
        }
        if self.smtp_oauth_scope.len() > 200 || has_control(&self.smtp_oauth_scope) {
            return bad("The scope is not valid");
        }
        Ok(())
    }

    /// Can the portal send emails? (A mail server and a sender are set.)
    pub fn email_configured(&self) -> bool {
        !self.smtp_host.is_empty() && !self.smtp_from.is_empty()
    }

    /// Is two-step sign-in in use at all (optional or required)?
    pub fn two_factor_on(&self) -> bool {
        self.two_factor != TwoFactorMode::Off
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

    /// A list of servers an account or a role may use: `None` (all), or at least one server that
    /// exists. The message names the problem.
    pub fn validate_server_access(&self, servers: &Option<Vec<String>>) -> Result<(), String> {
        let Some(servers) = servers else { return Ok(()) };
        if servers.is_empty() {
            return Err("choose at least one Orthanc server, or all of them".into());
        }
        if servers.len() > MAX_EXTRA_SERVERS + 1 {
            return Err("too many Orthanc servers".into());
        }
        let known = self.servers();
        match servers.iter().find(|key| !known.iter().any(|s| &s.key == *key)) {
            Some(key) => Err(format!(
                "there is no Orthanc server '{key}' (a server used by a role can only be removed once                  the role no longer uses it)"
            )),
            None => Ok(()),
        }
    }

    /// Every Orthanc server, the default one (`main`) first, switched-off ones included.
    pub fn servers(&self) -> Vec<OrthancServer> {
        let main = OrthancServer {
            key: MAIN_SERVER.into(),
            label: if self.orthanc_label.is_empty() { "Main".into() } else { self.orthanc_label.clone() },
            url: self.orthanc_url.clone(),
            username: self.orthanc_username.clone(),
            password: self.orthanc_password.clone(),
            log_dir: self.orthanc_log_dir.clone(),
            color: String::new(),
            enabled: true,
            searched_by_default: true,
        };
        std::iter::once(main).chain(self.orthanc_servers.iter().cloned()).collect()
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

    /// Microsoft 365: complete, encrypted, and the client secret is never shown.
    #[test]
    fn microsoft_365_sign_in() {
        let base = Settings::defaults();
        let m365 = |extra: Value| {
            let mut patch = json!({
                "smtp_host": "smtp.office365.com", "smtp_port": 587, "smtp_security": "starttls",
                "smtp_from": "PACS <pacs@example.com>", "smtp_auth": "microsoft365", "smtp_username": "pacs@example.com",
                "smtp_oauth_tenant": "contoso.onmicrosoft.com", "smtp_oauth_client_id": "11111111-2222-3333-4444-555555555555",
                "smtp_oauth_client_secret": "s3cr3t~value"
            });
            for (k, v) in extra.as_object().unwrap() {
                patch[k] = v.clone();
            }
            base.with_patch(&patch)
        };
        let ok = m365(json!({})).unwrap();
        assert_eq!(ok.smtp_auth, SmtpAuth::Microsoft365);
        let view = ok.to_view();
        assert!(view.get("smtp_oauth_client_secret").is_none());
        assert_eq!(view["smtp_oauth_client_secret_set"], json!(true));
        // An empty secret in a later save keeps the saved one.
        assert_eq!(
            ok.with_patch(&json!({ "smtp_oauth_client_secret": "" })).unwrap().smtp_oauth_client_secret.0,
            "s3cr3t~value"
        );
        assert!(m365(json!({ "smtp_security": "none" })).is_err(), "no encryption");
        assert!(m365(json!({ "smtp_username": "pacs" })).is_err(), "the user name is a mailbox");
        assert!(m365(json!({ "smtp_oauth_tenant": "" })).is_err());
        assert!(m365(json!({ "smtp_oauth_tenant": "bad tenant/x" })).is_err());
        assert!(m365(json!({ "smtp_oauth_client_id": "" })).is_err());
        assert!(m365(json!({ "smtp_oauth_client_secret": "" })).is_err(), "nothing saved yet");
        assert!(m365(json!({ "smtp_oauth_token_url": "http://login.example.com/token" })).is_err(), "https only");
        assert!(m365(json!({ "smtp_oauth_token_url": "https://login.microsoftonline.us/x/oauth2/v2.0/token" })).is_ok());
        // Email switched off: nothing is asked for.
        assert!(m365(json!({ "smtp_host": "", "smtp_oauth_client_id": "" })).is_ok());
    }

    /// Other Orthanc servers: checked, tidied, their passwords write-only and kept with them.
    /// Roles may be limited to servers that exist, and to at least one.
    #[test]
    fn roles_limited_to_some_servers() {
        let base = Settings::defaults()
            .with_patch(&json!({ "orthanc_servers": [{ "key": "archive", "url": "http://archive:8042" }] }))
            .unwrap();
        let with = |servers: Value| {
            let mut roles = serde_json::to_value(&base.roles).unwrap();
            roles[0]["servers"] = servers;
            base.with_patch(&json!({ "roles": roles }))
        };
        assert_eq!(with(json!(null)).unwrap().roles[0].servers, None, "null = every server");
        assert_eq!(
            with(json!([" archive ", "archive", "main"])).unwrap().roles[0].servers,
            Some(vec!["archive".to_string(), "main".to_string()]),
            "tidied"
        );
        assert!(with(json!([])).is_err(), "no server at all");
        assert!(with(json!(["research"])).is_err(), "a server that does not exist");
        // A server a role uses cannot be removed.
        let limited = with(json!(["archive"])).unwrap();
        assert!(limited.with_patch(&json!({ "orthanc_servers": [] })).is_err());
        assert_eq!(Settings::defaults().roles[0].servers, None, "roles start with every server");
    }

    /// Saved settings from before the Server column get it once; hiding it afterwards is kept.
    #[test]
    fn the_server_column_is_offered_once() {
        let old = json!({ "study_columns": ["patient", "study_date"] }).to_string();
        let upgraded = Settings::merged_with_stored(Settings::defaults(), Some(old));
        assert_eq!(upgraded.study_columns, ["patient", "study_date", "server"]);
        assert_eq!(upgraded.columns_version, 1);
        let hidden = json!({ "study_columns": ["patient"], "columns_version": 1 }).to_string();
        assert_eq!(Settings::merged_with_stored(Settings::defaults(), Some(hidden)).study_columns, ["patient"]);
        assert!(Settings::defaults().study_columns.contains(&"server".to_string()));
    }

    #[test]
    fn other_orthanc_servers() {
        let base = Settings::defaults();
        let with = |servers: Value| base.with_patch(&json!({ "orthanc_servers": servers }));
        let saved = with(json!([
            { "key": " Archive ", "label": "Archive 2015", "url": "http://archive:8042/", "username": "portal",
              "password": "pw1", "color": "#1A2B3C" },
            { "key": "research", "url": "https://research.local", "enabled": false }
        ]))
        .unwrap();
        let archive = &saved.orthanc_servers[0];
        assert_eq!(
            (archive.key.as_str(), archive.url.as_str(), archive.color.as_str()),
            ("archive", "http://archive:8042", "#1a2b3c")
        );
        assert!(archive.enabled && archive.searched_by_default, "on by default");
        assert!(!saved.orthanc_servers[1].enabled);
        let keys: Vec<String> = saved.servers().into_iter().map(|s| s.key).collect();
        assert_eq!(keys, ["main", "archive", "research"], "the default server first");
        assert_eq!(saved.servers()[0].label, "Main");

        // Write-only passwords: never in the view, kept when the form sends none.
        let view = saved.to_view();
        assert!(view["orthanc_servers"][0].get("password").is_none());
        assert_eq!(view["orthanc_servers"][0]["password_set"], json!(true));
        assert_eq!(view["orthanc_servers"][1]["password_set"], json!(false));
        let echoed = view["orthanc_servers"].clone();
        let again = saved.with_patch(&json!({ "orthanc_servers": echoed })).unwrap();
        assert_eq!(again.orthanc_servers[0].password.0, "pw1", "the form's echo keeps the password");
        let mut cleared = view["orthanc_servers"].clone();
        cleared[0]["clear_password"] = json!(true);
        assert_eq!(saved.with_patch(&json!({ "orthanc_servers": cleared })).unwrap().orthanc_servers[0].password.0, "");
        // Another address needs the password again; another path of the same server does not.
        let mut moved = view["orthanc_servers"].clone();
        moved[0]["url"] = json!("http://evil.example:8042");
        assert!(saved.with_patch(&json!({ "orthanc_servers": moved.clone() })).is_err());
        moved[0]["password"] = json!("pw2");
        assert_eq!(
            saved.with_patch(&json!({ "orthanc_servers": moved })).unwrap().orthanc_servers[0].password.0,
            "pw2"
        );
        let mut path = view["orthanc_servers"].clone();
        path[0]["url"] = json!("http://archive:8042/orthanc");
        assert!(saved.with_patch(&json!({ "orthanc_servers": path })).is_ok());
        // A password is never moved from one server to another by renaming keys.
        let mut renamed = view["orthanc_servers"].clone();
        renamed[0]["key"] = json!("other");
        assert_eq!(saved.with_patch(&json!({ "orthanc_servers": renamed })).unwrap().orthanc_servers[0].password.0, "");

        // Refused.
        let one = |extra: Value| {
            let mut server = json!({ "key": "archive", "url": "http://archive:8042" });
            for (k, v) in extra.as_object().unwrap() {
                server[k] = v.clone();
            }
            with(json!([server]))
        };
        assert!(one(json!({ "key": "main" })).is_err(), "the default server's key");
        assert!(one(json!({ "key": "" })).is_err());
        assert!(one(json!({ "key": "a_b" })).is_err());
        assert!(one(json!({ "key": "k".repeat(25) })).is_err());
        assert!(one(json!({ "url": "ftp://archive" })).is_err());
        assert!(one(json!({ "url": "http://a b" })).is_err());
        assert!(one(json!({ "color": "red" })).is_err());
        assert!(one(json!({ "label": "x".repeat(61) })).is_err());
        assert!(one(json!({})).is_ok());
        assert!(with(json!([{ "key": "a", "url": "http://a" }, { "key": "a", "url": "http://b" }])).is_err(), "twice");
        let eight: Vec<Value> = (0..8).map(|i| json!({ "key": format!("s{i}"), "url": "http://x" })).collect();
        assert!(with(json!(eight)).is_err(), "at most 7 more servers");
        assert!(with(json!("archive")).is_err());
    }

    /// Settings saved by a version without several servers load unchanged.
    #[test]
    fn settings_from_before_several_servers() {
        let stored = json!({ "orthanc_url": "http://pacs:8042", "orthanc_username": "u", "orthanc_password": "p" });
        let loaded = Settings::merged_with_stored(Settings::defaults(), Some(stored.to_string()));
        assert_eq!(loaded.orthanc_url, "http://pacs:8042");
        assert!(loaded.orthanc_servers.is_empty());
        assert_eq!(loaded.servers().len(), 1);
        assert_eq!(loaded.servers()[0].password.0, "p");
    }

    /// A saved secret never follows a changed address to another server without being typed again.
    #[test]
    fn secrets_stay_with_their_server() {
        let saved = Settings::defaults()
            .with_patch(&json!({
                "orthanc_url": "http://pacs.local:8042", "orthanc_username": "portal", "orthanc_password": "pw",
                "smtp_host": "smtp.example.com", "smtp_from": "PACS <pacs@example.com>", "smtp_security": "starttls",
                "smtp_username": "mailer", "smtp_password": "mailpw"
            }))
            .unwrap();
        // Orthanc: another server needs the password again (or removing it); another path does not.
        assert!(saved.with_patch(&json!({ "orthanc_url": "http://evil.example:8042" })).is_err());
        assert!(saved.with_patch(&json!({ "orthanc_url": "http://pacs.local:9999" })).is_err(), "another port");
        assert!(saved.with_patch(&json!({ "orthanc_url": "http://pacs.local:8042/orthanc" })).is_ok());
        assert!(saved.with_patch(&json!({ "orthanc_url": "http://evil.example", "orthanc_password": "new" })).is_ok());
        let cleared =
            saved.with_patch(&json!({ "orthanc_url": "http://other:8042", "clear_orthanc_password": true })).unwrap();
        assert_eq!(cleared.orthanc_password.0, "");
        // Mail server: the same, and switching email off is fine.
        assert!(saved.with_patch(&json!({ "smtp_host": "evil.example" })).is_err());
        assert!(saved.with_patch(&json!({ "smtp_host": "SMTP.example.com", "smtp_port": 465 })).is_ok());
        assert!(saved.with_patch(&json!({ "smtp_host": "evil.example", "smtp_password": "again" })).is_ok());
        assert!(saved.with_patch(&json!({ "smtp_host": "" })).is_ok());
        // Microsoft 365: another token server needs the secret again; another tenant does not.
        let m365 = saved
            .with_patch(&json!({
                "smtp_auth": "microsoft365", "smtp_username": "pacs@example.com", "smtp_oauth_tenant": "contoso.com",
                "smtp_oauth_client_id": "abc-123", "smtp_oauth_client_secret": "s3cret"
            }))
            .unwrap();
        assert!(m365.with_patch(&json!({ "smtp_oauth_token_url": "https://evil.example/token" })).is_err());
        assert!(m365.with_patch(&json!({ "smtp_oauth_tenant": "fabrikam.com" })).is_ok());
        assert!(m365
            .with_patch(
                &json!({ "smtp_oauth_token_url": "https://evil.example/token", "smtp_oauth_client_secret": "x" })
            )
            .is_ok());
    }

    /// Colours of modality buttons and labels: `#rrggbb`, tidied, and "Default" (empty) forgets a label's.
    #[test]
    fn modality_and_label_colours() {
        let base = Settings::defaults();
        let mut buttons = default_modality_buttons();
        buttons[0].color = " #1A2B3C ".into();
        let edited = base.with_patch(&json!({ "modality_buttons": buttons })).unwrap();
        assert_eq!(edited.modality_buttons[0].color, "#1a2b3c");
        assert_eq!(edited.modality_buttons[1].color, "", "the others keep the built-in colours");
        buttons[0].color = "red".into();
        assert!(base.with_patch(&json!({ "modality_buttons": buttons })).is_err());
        let labels = base.with_patch(&json!({ "label_colors": { "urgent": "#C00000", "teaching": "" } })).unwrap();
        assert_eq!(labels.label_colors.len(), 1);
        assert_eq!(labels.label_colors["urgent"], "#c00000");
        assert!(base.with_patch(&json!({ "label_colors": { "urgent": "url(x)" } })).is_err());
        assert!(base.with_patch(&json!({ "label_colors": { "no spaces": "#123456" } })).is_err());
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
        assert!(!ok(json!({ "custom_css": "body { background: u\\72l(http://x) }" })), "a CSS escape");
        assert!(!ok(json!({ "custom_css": "body { background: image-set('http://x' 1x) }" })));
        assert!(!ok(json!({ "date_format": "yyyy" })));
        assert!(ok(json!({ "language": "es" })) && ok(json!({ "language": "en" })));
        assert!(!ok(json!({ "language": "fr" })), "only the languages the portal has");
        assert_eq!(Language::parse(" es-ES "), Some(Language::Es));
        assert_eq!(Language::parse("EN"), Some(Language::En));
        assert_eq!(Language::parse("de"), None);
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
