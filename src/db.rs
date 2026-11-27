//! # db.rs
//!
//! Persistent storage (SQLite, a single file) for:
//!   * **users**: name, password hash, permissions, active flag;
//!   * **audit_log**: who did what and when (login, upload, download, user changes...);
//!   * **transfers**: studies sent to remote DICOM nodes, with the Orthanc job id;
//!   * **settings**: the configuration saved from the web interface (one JSON document);
//!   * **branding**: the logo and website icon uploaded from the Settings screen;
//!   * **shares**: invitation links that give one outside person access to one study;
//!   * **upload_links**: links that let one outside person SEND studies in;
//!   * **recent_views**: each person's recently viewed studies.
//!
//! Users also carry their email address and their two-step sign-in (secret, recovery code hashes).
//!
//! The connection is wrapped in a `std::sync::Mutex`. Queries here are tiny
//! (a handful of users), so they finish in microseconds and it is fine to run
//! them directly inside async handlers. The lock is NEVER held across an
//! `.await`, which would risk a deadlock.

use crate::error::AppError;
use crate::permissions::{Permission, PermissionSet};
use crate::util::now_unix;
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::Serialize;
use std::sync::{Arc, Mutex, MutexGuard};

/// One portal account.
#[derive(Debug, Clone, Serialize)]
pub struct User {
    pub id: i64,
    pub username: String,
    /// Argon2 hash. `skip` guarantees it is never serialised into any API answer.
    #[serde(skip)]
    pub password_hash: String,
    pub permissions: PermissionSet,
    /// Key of the role chosen for this account in the user form ("" = custom permissions).
    /// Roles are shortcuts: the permissions above are what counts for access; the role
    /// decides which view (normal or simplified) the user gets (Settings > Appearance).
    pub role: String,
    /// Disabled accounts cannot log in and their sessions stop working immediately.
    pub active: bool,
    pub created_at: i64,
    pub last_login: Option<i64>,
    /// Where the portal writes to this person (share link notifications). Empty = nowhere.
    pub email: String,
    /// Two-step sign-in is set up for this account (an authenticator app).
    pub two_factor: bool,
    /// One-time recovery codes not used yet (for a lost phone).
    pub recovery_codes_left: usize,
    /// The secret of the authenticator app (base32). Never sent to the browser.
    #[serde(skip)]
    pub totp_secret: Option<String>,
    /// A secret being set up (shown once as a QR code, confirmed with a first code).
    #[serde(skip)]
    pub totp_pending: Option<String>,
    /// The Orthanc servers this account may use, by key: `None` = all of them. Administrators
    /// always reach every server (they could give themselves access anyway).
    pub servers: Option<Vec<String>>,
}

impl User {
    /// Server-side permission gate used at the top of every protected handler.
    pub fn require(&self, permission: Permission) -> Result<(), AppError> {
        if self.permissions.contains(permission) {
            Ok(())
        } else {
            Err(AppError::Forbidden(permission.as_str()))
        }
    }

    /// The permission, or administrator (who may do everything the optional features offer).
    pub fn require_or_admin(&self, permission: Permission) -> Result<(), AppError> {
        if self.permissions.contains(Permission::Admin) {
            Ok(())
        } else {
            self.require(permission)
        }
    }

    /// Does the user have the permission, or are they an administrator?
    pub fn may(&self, permission: Permission) -> bool {
        self.permissions.contains(permission) || self.permissions.contains(Permission::Admin)
    }

    /// May this account use the Orthanc server `key`? Checked by `servers::OrthancPool` on
    /// every request: the studies of the other servers do not exist for this person.
    pub fn may_use_server(&self, key: &str) -> bool {
        self.permissions.contains(Permission::Admin)
            || self.servers.as_ref().is_none_or(|servers| servers.iter().any(|s| s == key))
    }
}

/// An invitation: a password-protected link that lets ONE outside person look at ONE study
/// (and download it a limited number of times) until it expires. See `routes/guest.rs`.
#[derive(Debug, Clone, Serialize)]
pub struct Share {
    pub id: i64,
    /// Random, unguessable part of the link (`/s/<token>`).
    pub token: String,
    /// Argon2 hash of the generated password: what the guest's password is checked against.
    #[serde(skip)]
    pub password_hash: String,
    pub created_at: i64,
    pub expires_at: i64,
    pub created_by: String,
    /// Orthanc id of the study (used to read and download it).
    pub study_id: String,
    /// DICOM StudyInstanceUID (what the viewer asks for; the only study a guest may reach).
    pub study_uid: String,
    /// Readable description kept for the Shares list: patient / date / description.
    pub label: String,
    /// Optional free text of the person who created it ("Dr. Smith, second opinion").
    pub note: String,
    pub max_downloads: i64,
    pub downloads_used: i64,
    pub revoked_at: Option<i64>,
    /// How many times the password was entered correctly, and the last time.
    pub access_count: i64,
    pub last_access_at: Option<i64>,
    /// A copy of the password is kept, so the Shares list can show it again (see
    /// [`Db::share_password`]). The copy itself is never read with the other fields.
    #[serde(skip)]
    pub has_password: bool,
    /// The other studies of a link that shares several (the first one is `study_id` / `study_uid`).
    pub extra_studies: Vec<SharedStudy>,
    /// Address told when the guest opens the link or downloads (the creator's, if they asked).
    #[serde(skip)]
    pub notify_email: Option<String>,
    /// The link shares anonymized copies made for it (Orthanc ids), deleted when the link ends.
    pub owned_copies: Vec<String>,
    /// Those copies were deleted: the link cannot be used or extended any more.
    pub copies_deleted: bool,
}

/// One more study of an invitation link.
#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SharedStudy {
    pub id: String,
    pub uid: String,
}

impl Share {
    /// Every study of the link, the first one first.
    pub fn studies(&self) -> Vec<SharedStudy> {
        let mut all = vec![SharedStudy { id: self.study_id.clone(), uid: self.study_uid.clone() }];
        all.extend(self.extra_studies.iter().cloned());
        all
    }

    /// The StudyInstanceUIDs a guest of this link may read.
    pub fn study_uids(&self) -> Vec<String> {
        self.studies().into_iter().map(|s| s.uid).collect()
    }

    /// Can the link still be used at time `now`? (Viewing does not depend on downloads left.)
    pub fn is_active(&self, now: i64) -> bool {
        self.revoked_at.is_none() && now < self.expires_at
    }

    pub fn downloads_left(&self) -> i64 {
        (self.max_downloads - self.downloads_used).max(0)
    }
}

/// What is needed to create a [`Share`].
pub struct NewShare<'a> {
    pub token: &'a str,
    pub password_hash: &'a str,
    pub expires_at: i64,
    pub created_by: &'a str,
    pub study_id: &'a str,
    pub study_uid: &'a str,
    pub label: &'a str,
    pub note: &'a str,
    pub max_downloads: i64,
    /// The other studies (JSON list of [`SharedStudy`], empty for one study).
    pub extra_studies: &'a [SharedStudy],
    /// The password itself, kept so it can be shown again (`None` = Settings > Share links says not to).
    pub password_copy: Option<&'a str>,
    /// Tell this address when the guest opens the link or downloads.
    pub notify_email: Option<&'a str>,
    /// Anonymized copies made for this link (deleted when it ends).
    pub owned_copies: &'a [String],
}

/// How long the kept password of an expired link is kept: it can be extended meanwhile and keep it.
pub const EXPIRED_PASSWORD_GRACE: i64 = 30 * 86_400;

const SHARE_COLUMNS: &str =
    "id, token, password_hash, created_at, expires_at, created_by, study_id, study_uid, label, \
                             note, max_downloads, downloads_used, revoked_at, access_count, last_access_at, \
                             password_copy IS NOT NULL, extra_studies, notify_email, owned_copies, copies_deleted";

/// A JSON list stored in a text column (`NULL` = empty).
fn json_list<T: serde::de::DeserializeOwned>(text: Option<String>) -> Vec<T> {
    text.and_then(|json| serde_json::from_str(&json).ok()).unwrap_or_default()
}

/// The opposite of [`json_list`]: `None` for an empty list.
fn list_json<T: Serialize>(list: &[T]) -> Option<String> {
    if list.is_empty() {
        None
    } else {
        serde_json::to_string(list).ok()
    }
}

/// A link that lets ONE outside person SEND studies in (a CD of earlier imaging, a referral...),
/// with a generated password, until it expires or its size limit is reached. The person sees
/// nothing of the archive. See `routes/inbound.rs`.
#[derive(Debug, Clone, Serialize)]
pub struct UploadLink {
    pub id: i64,
    pub token: String,
    #[serde(skip)]
    pub password_hash: String,
    #[serde(skip)]
    pub has_password: bool,
    pub created_at: i64,
    pub expires_at: i64,
    pub created_by: String,
    /// For the staff ("Mrs Doe's previous MRI").
    pub note: String,
    /// Orthanc label put on every study received (empty = none).
    pub label: String,
    pub max_bytes: i64,
    pub bytes_received: i64,
    pub files_received: i64,
    /// Files Orthanc did not accept (not DICOM: a viewer program on a CD, a text file...).
    pub files_rejected: i64,
    /// Orthanc ids of the studies received.
    pub studies: Vec<String>,
    pub revoked_at: Option<i64>,
    pub access_count: i64,
    pub last_upload_at: Option<i64>,
    #[serde(skip)]
    pub notify_email: Option<String>,
}

impl UploadLink {
    pub fn is_active(&self, now: i64) -> bool {
        self.revoked_at.is_none() && now < self.expires_at
    }
}

/// What is needed to create an [`UploadLink`].
pub struct NewUploadLink<'a> {
    pub token: &'a str,
    pub password_hash: &'a str,
    pub password_copy: Option<&'a str>,
    pub expires_at: i64,
    pub created_by: &'a str,
    pub note: &'a str,
    pub label: &'a str,
    pub max_bytes: i64,
    pub notify_email: Option<&'a str>,
}

const UPLOAD_LINK_COLUMNS: &str = "id, token, password_hash, password_copy IS NOT NULL, created_at, expires_at, \
     created_by, note, label, max_bytes, bytes_received, files_received, files_rejected, studies, revoked_at, \
     access_count, last_upload_at, notify_email";

fn row_to_upload_link(row: &Row) -> rusqlite::Result<UploadLink> {
    Ok(UploadLink {
        id: row.get(0)?,
        token: row.get(1)?,
        password_hash: row.get(2)?,
        has_password: row.get(3)?,
        created_at: row.get(4)?,
        expires_at: row.get(5)?,
        created_by: row.get(6)?,
        note: row.get(7)?,
        label: row.get(8)?,
        max_bytes: row.get(9)?,
        bytes_received: row.get(10)?,
        files_received: row.get(11)?,
        files_rejected: row.get(12)?,
        studies: json_list(row.get(13)?),
        revoked_at: row.get(14)?,
        access_count: row.get(15)?,
        last_upload_at: row.get(16)?,
        notify_email: row.get(17)?,
    })
}

/// At most this many studies are remembered per upload link (more still arrive, only the list stops).
pub const MAX_UPLOAD_LINK_STUDIES: usize = 500;

/// How many studies each person's "recently viewed" list remembers.
pub const RECENT_VIEWS_KEPT: i64 = 30;

fn row_to_share(row: &Row) -> rusqlite::Result<Share> {
    Ok(Share {
        id: row.get(0)?,
        token: row.get(1)?,
        password_hash: row.get(2)?,
        created_at: row.get(3)?,
        expires_at: row.get(4)?,
        created_by: row.get(5)?,
        study_id: row.get(6)?,
        study_uid: row.get(7)?,
        label: row.get(8)?,
        note: row.get(9)?,
        max_downloads: row.get(10)?,
        downloads_used: row.get(11)?,
        revoked_at: row.get(12)?,
        access_count: row.get(13)?,
        has_password: row.get(15)?,
        last_access_at: row.get(14)?,
        extra_studies: json_list(row.get(16)?),
        notify_email: row.get(17)?,
        owned_copies: json_list(row.get(18)?),
        copies_deleted: row.get::<_, i64>(19)? != 0,
    })
}

/// One line of the audit trail.
#[derive(Debug, Serialize)]
pub struct AuditEntry {
    pub id: i64,
    pub ts: i64,
    pub username: String,
    pub action: String,
    pub detail: String,
}

/// A "send study to a remote DICOM node" request started from the portal.
/// The live state (pending / running / success / failure) is read from the
/// Orthanc job when the Transfers tab is displayed.
#[derive(Debug, Clone, Serialize)]
pub struct Transfer {
    pub id: i64,
    pub ts: i64,
    pub username: String,
    /// Orthanc id of the study that was sent.
    pub study_id: String,
    /// Human readable description (patient / date / description).
    pub label: String,
    /// Alias of the destination in Orthanc's `DicomModalities`.
    pub target: String,
    /// Orthanc job id, used to follow the progress.
    pub job_id: String,
    /// Final outcome (`Success` / `Failure` / `Unknown`) once it has been seen. From then
    /// on Orthanc is no longer asked about this transfer (see `routes/send.rs`).
    #[serde(skip)]
    pub final_state: Option<String>,
    #[serde(skip)]
    pub final_error: Option<String>,
    /// What a finished transfer did, as JSON `{ "instances", "failed", "runtime" }` (kept so the
    /// Transfers tab can still say "120 images in 2 min" once Orthanc has forgotten the job).
    #[serde(skip)]
    pub final_info: Option<String>,
    /// `send` (to `target`) or `retrieve` (from `target`; `study_id` is then the StudyInstanceUID).
    pub direction: String,
}

/// One stored branding image: `(kind, content type, bytes, updated_at)`.
pub type BrandingRow = (String, String, Vec<u8>, i64);

/// Cleans a text before it is written to the audit log: no control characters, bounded size.
fn clean_for_audit(text: &str, max_chars: usize) -> String {
    text.chars().map(|c| if c.is_control() { ' ' } else { c }).take(max_chars).collect()
}

/// Cheap-to-clone handle to the database.
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
    /// The file, for backups (made through a second connection, see [`Db::backup_into`]).
    path: Arc<String>,
}

/// Columns read for a user, always in this order (see [`row_to_user`]).
const USER_COLUMNS: &str = "id, username, password_hash, permissions, active, created_at, last_login, role, \
                            email, totp_enabled, totp_secret, totp_pending, recovery_codes, servers";

/// Converts one SQL row into a [`User`].
fn row_to_user(row: &Row) -> rusqlite::Result<User> {
    Ok(User {
        id: row.get(0)?,
        username: row.get(1)?,
        password_hash: row.get(2)?,
        permissions: PermissionSet::from_db_string(&row.get::<_, String>(3)?),
        active: row.get::<_, i64>(4)? != 0,
        created_at: row.get(5)?,
        last_login: row.get(6)?,
        role: row.get(7)?,
        email: row.get(8)?,
        two_factor: row.get::<_, i64>(9)? != 0,
        totp_secret: row.get(10)?,
        totp_pending: row.get(11)?,
        recovery_codes_left: json_list::<String>(row.get(12)?).len(),
        servers: server_list(row.get(13)?),
    })
}

/// The stored list of servers of an account: NULL = every server, otherwise a JSON list. A list
/// that cannot be read gives NO server rather than all of them.
fn server_list(stored: Option<String>) -> Option<Vec<String>> {
    stored.map(|s| serde_json::from_str(&s).unwrap_or_default())
}

/// How [`server_list`] reads it back.
fn stored_server_list(servers: &Option<Vec<String>>) -> Option<String> {
    servers.as_ref().map(|list| serde_json::to_string(list).unwrap_or_else(|_| "[]".into()))
}

/// Adds the columns a table lacks (databases created by older versions). `columns` = (name, type).
fn add_missing_columns(conn: &Connection, table: &str, columns: &[(&str, &str)]) -> rusqlite::Result<()> {
    for (name, kind) in columns {
        let exists = conn
            .prepare(&format!("SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1"))?
            .exists(params![name])?;
        if !exists {
            conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {name} {kind}"), [])?;
        }
    }
    Ok(())
}

/// Makes a file readable and writable by its owner only (Unix; no-op elsewhere).
fn restrict_file_permissions(path: &str) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::path::Path::new(path).exists() {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

impl Db {
    /// Opens (or creates) the database file and makes sure the tables exist.
    pub fn open(path: &str) -> Result<Self, AppError> {
        let conn = Connection::open(path)?;
        restrict_file_permissions(path);
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;      -- better behaviour with concurrent readers
             PRAGMA synchronous = NORMAL;

             CREATE TABLE IF NOT EXISTS users (
                 id            INTEGER PRIMARY KEY AUTOINCREMENT,
                 username      TEXT    NOT NULL UNIQUE COLLATE NOCASE, -- case-insensitive login
                 password_hash TEXT    NOT NULL,
                 permissions   TEXT    NOT NULL,                       -- e.g. 'read,visualize'
                 active        INTEGER NOT NULL DEFAULT 1,
                 created_at    INTEGER NOT NULL,                       -- unix seconds
                 last_login    INTEGER,
                 role          TEXT    NOT NULL DEFAULT ''                -- role key chosen in the user form
             );

             CREATE TABLE IF NOT EXISTS audit_log (
                 id       INTEGER PRIMARY KEY AUTOINCREMENT,
                 ts       INTEGER NOT NULL,
                 username TEXT    NOT NULL,
                 action   TEXT    NOT NULL,
                 detail   TEXT    NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_audit_ts ON audit_log (ts);
             -- Statistics: entries of one action over a period (Configuration > Statistics).
             CREATE INDEX IF NOT EXISTS idx_audit_action_ts ON audit_log (action, ts);

             CREATE TABLE IF NOT EXISTS transfers (
                 id       INTEGER PRIMARY KEY AUTOINCREMENT,
                 ts       INTEGER NOT NULL,
                 username TEXT    NOT NULL,
                 study_id TEXT    NOT NULL,
                 label    TEXT    NOT NULL,
                 target   TEXT    NOT NULL,
                 job_id   TEXT    NOT NULL,
                 final_state TEXT,                                      -- set once the job has ended
                 final_error TEXT,
                 final_info  TEXT                                       -- JSON: images, failed, run time
             );

             -- Invitation links for people outside (see routes/guest.rs).
             CREATE TABLE IF NOT EXISTS shares (
                 id             INTEGER PRIMARY KEY AUTOINCREMENT,
                 token          TEXT    NOT NULL UNIQUE,
                 password_hash  TEXT    NOT NULL,
                 created_at     INTEGER NOT NULL,
                 expires_at     INTEGER NOT NULL,
                 created_by     TEXT    NOT NULL,
                 study_id       TEXT    NOT NULL,
                 study_uid      TEXT    NOT NULL,
                 label          TEXT    NOT NULL,
                 note           TEXT    NOT NULL DEFAULT '',
                 max_downloads  INTEGER NOT NULL,
                 downloads_used INTEGER NOT NULL DEFAULT 0,
                 revoked_at     INTEGER,
                 access_count   INTEGER NOT NULL DEFAULT 0,
                 last_access_at INTEGER,
                 password_copy  TEXT                                    -- kept to show it again; erased when the link ends
             );

             -- Logo and website icon uploaded from the Settings screen (kind = 'logo' | 'favicon').
             CREATE TABLE IF NOT EXISTS branding (
                 kind         TEXT PRIMARY KEY,
                 content_type TEXT    NOT NULL,
                 data         BLOB    NOT NULL,
                 updated_at   INTEGER NOT NULL
             );

             -- Configuration saved from the Settings screen: a single row, key 'main'.
             -- Descriptions of the DICOM nodes (Settings > DICOM nodes), shown to the staff:
             -- Orthanc itself only knows the short alias, the AE title and the address.
             CREATE TABLE IF NOT EXISTS node_descriptions (
                 alias       TEXT PRIMARY KEY,
                 description TEXT NOT NULL
             );

             CREATE TABLE IF NOT EXISTS settings (
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );

             -- Upload links: people outside SEND studies in (see routes/inbound.rs).
             CREATE TABLE IF NOT EXISTS upload_links (
                 id               INTEGER PRIMARY KEY AUTOINCREMENT,
                 token            TEXT    NOT NULL UNIQUE,
                 password_hash    TEXT    NOT NULL,
                 password_copy    TEXT,
                 created_at       INTEGER NOT NULL,
                 expires_at       INTEGER NOT NULL,
                 created_by       TEXT    NOT NULL,
                 note             TEXT    NOT NULL DEFAULT '',
                 label            TEXT    NOT NULL DEFAULT '',
                 max_bytes        INTEGER NOT NULL,
                 bytes_received   INTEGER NOT NULL DEFAULT 0,
                 files_received   INTEGER NOT NULL DEFAULT 0,
                 files_rejected   INTEGER NOT NULL DEFAULT 0,
                 studies          TEXT,                                  -- JSON list of Orthanc study ids
                 revoked_at       INTEGER,
                 access_count     INTEGER NOT NULL DEFAULT 0,
                 last_upload_at   INTEGER,
                 notify_email     TEXT,
                 last_notified_at INTEGER
             );

             -- The studies each person opened last (home page > Recently viewed). Personal.
             CREATE TABLE IF NOT EXISTS recent_views (
                 user_id   INTEGER NOT NULL,
                 study_id  TEXT    NOT NULL,
                 study_uid TEXT    NOT NULL,
                 ts        INTEGER NOT NULL,
                 PRIMARY KEY (user_id, study_id)
             );

             -- The emails the auto-forward rules sent: one per study and rule, even when the study
             -- gets more images later and becomes stable again.
             CREATE TABLE IF NOT EXISTS rule_notices (
                 rule     TEXT    NOT NULL,
                 study_id TEXT    NOT NULL,
                 ts       INTEGER NOT NULL,
                 PRIMARY KEY (rule, study_id)
             );
             CREATE INDEX IF NOT EXISTS rule_notices_ts ON rule_notices (ts);",
        )?;
        add_missing_columns(
            &conn,
            "users",
            &[
                ("email", "TEXT NOT NULL DEFAULT ''"),
                ("totp_enabled", "INTEGER NOT NULL DEFAULT 0"),
                ("totp_secret", "TEXT"),
                ("totp_pending", "TEXT"),
                ("totp_last_step", "INTEGER"),
                ("recovery_codes", "TEXT"),
                // The Orthanc servers the account may use (NULL = all).
                ("servers", "TEXT"),
            ],
        )?;
        add_missing_columns(
            &conn,
            "shares",
            &[
                ("notify_email", "TEXT"),
                ("notified_open", "INTEGER NOT NULL DEFAULT 0"),
                ("owned_copies", "TEXT"),
                ("copies_deleted", "INTEGER NOT NULL DEFAULT 0"),
            ],
        )?;

        // Databases created by older versions have no `final_state` / `final_error` / `final_info` columns.
        for column in ["final_state", "final_error", "final_info", "direction"] {
            let exists = conn
                .prepare("SELECT 1 FROM pragma_table_info('transfers') WHERE name = ?1")?
                .exists(params![column])?;
            if !exists {
                conn.execute(&format!("ALTER TABLE transfers ADD COLUMN {column} TEXT"), [])?;
            }
        }

        // Databases created by older versions keep no copy of the share passwords.
        let has_copy =
            conn.prepare("SELECT 1 FROM pragma_table_info('shares') WHERE name = 'password_copy'")?.exists([])?;
        if !has_copy {
            conn.execute("ALTER TABLE shares ADD COLUMN password_copy TEXT", [])?;
        }
        // ...nor the other studies of a link that shares several.
        let has_extra =
            conn.prepare("SELECT 1 FROM pragma_table_info('shares') WHERE name = 'extra_studies'")?.exists([])?;
        if !has_extra {
            conn.execute("ALTER TABLE shares ADD COLUMN extra_studies TEXT", [])?;
        }

        // Databases created by older versions do not remember the role of each user.
        let has_role = conn.prepare("SELECT 1 FROM pragma_table_info('users') WHERE name = 'role'")?.exists([])?;
        if !has_role {
            conn.execute("ALTER TABLE users ADD COLUMN role TEXT NOT NULL DEFAULT ''", [])?;
        }

        // Room for every statement of this file in the prepared-statement cache (see `lock`).
        conn.set_prepared_statement_cache_capacity(64);

        // The file holds password hashes and the Orthanc password: only its owner may read it.
        // (The -wal / -shm files SQLite creates next to it copy these permissions.)
        restrict_file_permissions(path);
        Ok(Self { conn: Arc::new(Mutex::new(conn)), path: Arc::new(path.to_string()) })
    }

    /// The folder of the database file: where the portal may write (backups made for a download,
    /// patient CD exports being assembled). In Docker this is the `/data` volume.
    pub fn data_dir(&self) -> std::path::PathBuf {
        let path = std::path::Path::new(self.path.as_str());
        match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => std::path::PathBuf::from("."),
        }
    }

    /// Writes a consistent copy of the whole database to `dest` (`VACUUM INTO`), through a second
    /// connection so the portal keeps answering meanwhile. `dest` must not exist yet.
    pub fn backup_into(&self, dest: &std::path::Path) -> Result<(), AppError> {
        let conn = Connection::open(self.path.as_str())?;
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        conn.execute("VACUUM INTO ?1", params![dest.to_string_lossy()])?;
        restrict_file_permissions(&dest.to_string_lossy());
        Ok(())
    }

    // ------------------------------------------------------- small stored values

    /// A value kept between restarts (the last change of Orthanc already looked at...).
    pub fn get_value(&self, key: &str) -> Result<Option<String>, AppError> {
        let conn = self.lock();
        let value = conn
            .prepare_cached("SELECT value FROM settings WHERE key = ?1")?
            .query_row(params![format!("value:{key}")], |r| r.get(0))
            .optional()?;
        Ok(value)
    }

    pub fn set_value(&self, key: &str, value: &str) -> Result<(), AppError> {
        let conn = self.lock();
        conn.prepare_cached(
            "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )?
        .execute(params![format!("value:{key}"), value])?;
        Ok(())
    }

    /// Locks the connection. A poisoned mutex (a panic while locked) is recovered
    /// because SQLite itself stays consistent.
    ///
    /// Every query below goes through `prepare_cached`: the SQL is compiled once and the
    /// compiled statement is reused. That matters for the lookups done on EVERY request
    /// (the user behind a session, the invitation behind a guest cookie), which the viewers
    /// repeat hundreds of times while a study loads.
    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ----------------------------------------------------------------- users

    pub fn count_users(&self) -> Result<i64, AppError> {
        let conn = self.lock();
        let value = conn.prepare_cached("SELECT COUNT(*) FROM users")?.query_row([], |r| r.get(0))?;
        Ok(value)
    }

    /// Inserts a new active user. Returns `Conflict` if the name is already taken.
    pub fn create_user(
        &self,
        username: &str,
        password_hash: &str,
        permissions: &PermissionSet,
        role: &str,
    ) -> Result<User, AppError> {
        let id = {
            let conn = self.lock();
            let result = conn
                .prepare_cached(
                    "INSERT INTO users (username, password_hash, permissions, active, created_at, role)
                 VALUES (?1, ?2, ?3, 1, ?4, ?5)",
                )?
                .execute(params![username, password_hash, permissions.to_db_string(), now_unix(), role]);
            match result {
                Ok(_) => conn.last_insert_rowid(),
                Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::ConstraintViolation => {
                    return Err(AppError::Conflict("That username is already taken".into()));
                }
                Err(e) => return Err(e.into()),
            }
        }; // lock released here, before get_user() locks again
        self.get_user(id)?.ok_or(AppError::NotFound)
    }

    pub fn get_user(&self, id: i64) -> Result<Option<User>, AppError> {
        let conn = self.lock();
        let sql = format!("SELECT {USER_COLUMNS} FROM users WHERE id = ?1");
        let value = conn.prepare_cached(&sql)?.query_row(params![id], row_to_user).optional()?;
        Ok(value)
    }

    pub fn get_user_by_name(&self, username: &str) -> Result<Option<User>, AppError> {
        let conn = self.lock();
        let sql = format!("SELECT {USER_COLUMNS} FROM users WHERE username = ?1");
        let value = conn.prepare_cached(&sql)?.query_row(params![username], row_to_user).optional()?;
        Ok(value)
    }

    pub fn list_users(&self) -> Result<Vec<User>, AppError> {
        let conn = self.lock();
        let sql = format!("SELECT {USER_COLUMNS} FROM users ORDER BY username");
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map([], row_to_user)?;
        let users = rows.collect::<Result<Vec<_>, _>>()?;
        Ok(users)
    }

    /// Applies only the fields that are `Some`, all inside one transaction.
    pub fn update_user(
        &self,
        id: i64,
        permissions: Option<&PermissionSet>,
        active: Option<bool>,
        password_hash: Option<&str>,
        role: Option<&str>,
    ) -> Result<(), AppError> {
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        if let Some(r) = role {
            tx.execute("UPDATE users SET role = ?1 WHERE id = ?2", params![r, id])?;
        }
        if let Some(p) = permissions {
            tx.execute("UPDATE users SET permissions = ?1 WHERE id = ?2", params![p.to_db_string(), id])?;
        }
        if let Some(a) = active {
            tx.execute("UPDATE users SET active = ?1 WHERE id = ?2", params![a as i64, id])?;
        }
        if let Some(h) = password_hash {
            tx.execute("UPDATE users SET password_hash = ?1 WHERE id = ?2", params![h, id])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The Orthanc servers an account may use (`None` = all).
    pub fn set_user_servers(&self, id: i64, servers: &Option<Vec<String>>) -> Result<(), AppError> {
        let conn = self.lock();
        conn.prepare_cached("UPDATE users SET servers = ?1 WHERE id = ?2")?
            .execute(params![stored_server_list(servers), id])?;
        Ok(())
    }

    /// Gives several accounts new permissions, servers and role (a role edited in Settings), all or none.
    pub fn set_users_role_permissions(
        &self,
        changes: &[(i64, &str, &PermissionSet, &Option<Vec<String>>)],
    ) -> Result<(), AppError> {
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        for (id, role, permissions, servers) in changes {
            tx.execute(
                "UPDATE users SET permissions = ?1, role = ?2, servers = ?3 WHERE id = ?4",
                params![permissions.to_db_string(), role, stored_server_list(servers), id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn delete_user(&self, id: i64) -> Result<(), AppError> {
        let conn = self.lock();
        conn.prepare_cached("DELETE FROM users WHERE id = ?1")?.execute(params![id])?;
        conn.prepare_cached("DELETE FROM recent_views WHERE user_id = ?1")?.execute(params![id])?;
        Ok(())
    }

    /// The address the portal writes to this person ("" = none).
    pub fn set_user_email(&self, id: i64, email: &str) -> Result<(), AppError> {
        let conn = self.lock();
        conn.prepare_cached("UPDATE users SET email = ?2 WHERE id = ?1")?.execute(params![id, email])?;
        Ok(())
    }

    // ------------------------------------------------------- two-step sign-in

    /// Remembers the secret being set up (`None` forgets it).
    pub fn set_totp_pending(&self, id: i64, secret: Option<&str>) -> Result<(), AppError> {
        let conn = self.lock();
        conn.prepare_cached("UPDATE users SET totp_pending = ?2 WHERE id = ?1")?.execute(params![id, secret])?;
        Ok(())
    }

    /// Switches two-step sign-in on with `secret`, whose code at time step `step` was just typed
    /// (so that code cannot be used again), and these hashed recovery codes.
    pub fn enable_totp(&self, id: i64, secret: &str, step: i64, recovery_hashes: &[String]) -> Result<(), AppError> {
        let conn = self.lock();
        conn.prepare_cached(
            "UPDATE users SET totp_enabled = 1, totp_secret = ?2, totp_pending = NULL, totp_last_step = ?3,
                              recovery_codes = ?4 WHERE id = ?1",
        )?
        .execute(params![id, secret, step, list_json(recovery_hashes)])?;
        Ok(())
    }

    /// Switches two-step sign-in off and forgets its secret and recovery codes.
    pub fn disable_totp(&self, id: i64) -> Result<(), AppError> {
        let conn = self.lock();
        conn.prepare_cached(
            "UPDATE users SET totp_enabled = 0, totp_secret = NULL, totp_pending = NULL, totp_last_step = NULL,
                              recovery_codes = NULL WHERE id = ?1",
        )?
        .execute(params![id])?;
        Ok(())
    }

    /// Accepts the code of time step `step` once: false if that step (or a later one) was used.
    /// One statement, so two sign-ins with the same code at the same moment cannot both pass.
    pub fn use_totp_step(&self, id: i64, step: i64) -> Result<bool, AppError> {
        let conn = self.lock();
        let changed = conn
            .prepare_cached(
                "UPDATE users SET totp_last_step = ?2 WHERE id = ?1 AND (totp_last_step IS NULL OR totp_last_step < ?2)",
            )?
            .execute(params![id, step])?;
        Ok(changed == 1)
    }

    /// Replaces the recovery codes (their hashes).
    pub fn set_recovery_codes(&self, id: i64, hashes: &[String]) -> Result<(), AppError> {
        let conn = self.lock();
        conn.prepare_cached("UPDATE users SET recovery_codes = ?2 WHERE id = ?1")?
            .execute(params![id, list_json(hashes)])?;
        Ok(())
    }

    /// Uses up the recovery code with this hash: true if it was one of the account's (it is gone now).
    pub fn use_recovery_code(&self, id: i64, hash: &str) -> Result<bool, AppError> {
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        let stored: Option<String> = tx
            .query_row("SELECT recovery_codes FROM users WHERE id = ?1", params![id], |r| r.get(0))
            .optional()?
            .flatten();
        let mut codes: Vec<String> = json_list(stored);
        let before = codes.len();
        codes.retain(|c| c != hash);
        if codes.len() == before {
            return Ok(false);
        }
        tx.execute("UPDATE users SET recovery_codes = ?2 WHERE id = ?1", params![id, list_json(&codes)])?;
        tx.commit()?;
        Ok(true)
    }

    /// Remembers the time of the last successful login.
    pub fn touch_login(&self, id: i64) -> Result<(), AppError> {
        let conn = self.lock();
        conn.prepare_cached("UPDATE users SET last_login = ?1 WHERE id = ?2")?.execute(params![now_unix(), id])?;
        Ok(())
    }

    /// Number of enabled administrators. Used to refuse changes that would leave
    /// the portal without anyone able to manage users.
    pub fn count_active_admins(&self) -> Result<usize, AppError> {
        Ok(self.list_users()?.iter().filter(|u| u.active && u.permissions.contains(Permission::Admin)).count())
    }

    // ----------------------------------------------------------------- audit

    /// Appends an entry to the audit trail. Failures are logged, never propagated:
    /// a broken audit table must not block clinical work.
    pub fn audit(&self, username: &str, action: &str, detail: &str) {
        let (username, action, detail) =
            (clean_for_audit(username, 64), clean_for_audit(action, 40), clean_for_audit(detail, 500));
        let conn = self.lock();
        let written = conn
            .prepare_cached("INSERT INTO audit_log (ts, username, action, detail) VALUES (?1, ?2, ?3, ?4)")
            .and_then(|mut insert| insert.execute(params![now_unix(), username, action, detail]));
        if let Err(e) = written {
            tracing::warn!("could not write audit log entry: {e}");
        }
    }

    /// The most recent audit entries, newest first.
    pub fn recent_audit(&self, limit: i64) -> Result<Vec<AuditEntry>, AppError> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare_cached("SELECT id, ts, username, action, detail FROM audit_log ORDER BY id DESC LIMIT ?1")?;
        let rows = stmt.query_map(params![limit], |r| {
            Ok(AuditEntry { id: r.get(0)?, ts: r.get(1)?, username: r.get(2)?, action: r.get(3)?, detail: r.get(4)? })
        })?;
        let entries = rows.collect::<Result<Vec<_>, _>>()?;
        Ok(entries)
    }

    // ------------------------------------------------------------- transfers

    /// Remembers that `username` started sending a study; returns the new row id.
    pub fn add_transfer(
        &self,
        username: &str,
        study_id: &str,
        label: &str,
        target: &str,
        job_id: &str,
        direction: &str,
    ) -> Result<i64, AppError> {
        let conn = self.lock();
        conn.prepare_cached(
            "INSERT INTO transfers (ts, username, study_id, label, target, job_id, direction)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?
        .execute(params![now_unix(), username, study_id, label, target, job_id, direction])?;
        Ok(conn.last_insert_rowid())
    }

    /// Latest transfers, newest first. `only_user = Some(name)` restricts the list
    /// to that user's own transfers (administrators pass `None` to see them all).
    pub fn recent_transfers(&self, only_user: Option<&str>, limit: i64) -> Result<Vec<Transfer>, AppError> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT id, ts, username, study_id, label, target, job_id, final_state, final_error, final_info, direction FROM transfers
             WHERE (?1 IS NULL OR username = ?1 COLLATE NOCASE)
             ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![only_user, limit], |r| {
            Ok(Transfer {
                id: r.get(0)?,
                ts: r.get(1)?,
                username: r.get(2)?,
                study_id: r.get(3)?,
                label: r.get(4)?,
                target: r.get(5)?,
                job_id: r.get(6)?,
                final_state: r.get(7)?,
                final_error: r.get(8)?,
                final_info: r.get(9)?,
                direction: r.get::<_, Option<String>>(10)?.unwrap_or_else(|| "send".into()),
            })
        })?;
        let transfers = rows.collect::<Result<Vec<_>, _>>()?;
        Ok(transfers)
    }

    // -------------------------------------------------------------- settings

    /// The settings document saved from the web interface, if any (JSON text).
    pub fn load_settings(&self) -> Result<Option<String>, AppError> {
        let conn = self.lock();
        let value = conn
            .prepare_cached("SELECT value FROM settings WHERE key = 'main'")?
            .query_row([], |r| r.get(0))
            .optional()?;
        Ok(value)
    }

    /// Stores (replaces) the settings document.
    pub fn save_settings(&self, json: &str) -> Result<(), AppError> {
        let conn = self.lock();
        conn.prepare_cached(
            "INSERT INTO settings (key, value) VALUES ('main', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )?
        .execute(params![json])?;
        Ok(())
    }

    /// Forgets the saved settings (emergency recovery with `RESET_SETTINGS=true`).
    pub fn delete_settings(&self) -> Result<(), AppError> {
        let conn = self.lock();
        conn.prepare_cached("DELETE FROM settings WHERE key = 'main'")?.execute([])?;
        Ok(())
    }

    // -------------------------------------------------------------- branding

    /// Every uploaded branding image: `(kind, content type, bytes, updated_at)`.
    pub fn load_branding(&self) -> Result<Vec<BrandingRow>, AppError> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached("SELECT kind, content_type, data, updated_at FROM branding")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        let all = rows.collect::<Result<Vec<_>, _>>()?;
        Ok(all)
    }

    /// Stores (replaces) the image of one kind.
    pub fn save_branding(&self, kind: &str, content_type: &str, data: &[u8], updated_at: i64) -> Result<(), AppError> {
        let conn = self.lock();
        conn.prepare_cached(
            "INSERT INTO branding (kind, content_type, data, updated_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(kind) DO UPDATE SET content_type = excluded.content_type,
                                             data = excluded.data, updated_at = excluded.updated_at",
        )?
        .execute(params![kind, content_type, data, updated_at])?;
        Ok(())
    }

    /// Removes the image of one kind (the default is used again).
    pub fn delete_branding(&self, kind: &str) -> Result<(), AppError> {
        let conn = self.lock();
        conn.prepare_cached("DELETE FROM branding WHERE kind = ?1")?.execute(params![kind])?;
        Ok(())
    }

    /// Remembers how a transfer ended, so Orthanc is not asked about it again.
    pub fn set_transfer_final(&self, id: i64, state: &str, error: Option<&str>, info: &str) -> Result<(), AppError> {
        let conn = self.lock();
        conn.prepare_cached("UPDATE transfers SET final_state = ?1, final_error = ?2, final_info = ?3 WHERE id = ?4")?
            .execute(params![state, error, info, id])?;
        Ok(())
    }

    // ------------------------------------------------------- DICOM node descriptions

    /// Every description, by alias.
    pub fn node_descriptions(&self) -> Result<std::collections::HashMap<String, String>, AppError> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached("SELECT alias, description FROM node_descriptions")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Sets the description of a node; an empty one removes it.
    pub fn set_node_description(&self, alias: &str, description: &str) -> Result<(), AppError> {
        let conn = self.lock();
        if description.is_empty() {
            conn.prepare_cached("DELETE FROM node_descriptions WHERE alias = ?1")?.execute(params![alias])?;
        } else {
            conn.prepare_cached(
                "INSERT INTO node_descriptions (alias, description) VALUES (?1, ?2)
                 ON CONFLICT(alias) DO UPDATE SET description = excluded.description",
            )?
            .execute(params![alias, description])?;
        }
        Ok(())
    }

    /// Housekeeping: deletes audit entries and transfers older than `cutoff` (unix
    /// seconds). Returns how many rows were removed.
    pub fn purge_older_than(&self, cutoff: i64) -> Result<usize, AppError> {
        let conn = self.lock();
        let audit = conn.prepare_cached("DELETE FROM audit_log WHERE ts < ?1")?.execute(params![cutoff])?;
        let transfers = conn.prepare_cached("DELETE FROM transfers WHERE ts < ?1")?.execute(params![cutoff])?;
        // Links that expired before the cut-off are no use any more either (once the anonymized
        // copies they own are deleted: the row is what remembers them).
        let shares = conn
            .prepare_cached(
                "DELETE FROM shares WHERE expires_at < ?1 AND (owned_copies IS NULL OR copies_deleted = 1)",
            )?
            .execute(params![cutoff])?;
        let uploads =
            conn.prepare_cached("DELETE FROM upload_links WHERE expires_at < ?1")?.execute(params![cutoff])?;
        Ok(audit + transfers + shares + uploads)
    }

    // ------------------------------------------------------------ statistics

    /// How many audit entries of each of `actions` there were per month (`YYYY-MM`, UTC) since `since`.
    pub fn audit_per_month(&self, actions: &[&str], since: i64) -> Result<Vec<(String, String, i64)>, AppError> {
        let conn = self.lock();
        let mut out = Vec::new();
        let mut stmt = conn.prepare_cached(
            "SELECT strftime('%Y-%m', ts, 'unixepoch'), COUNT(*) FROM audit_log
             WHERE action = ?1 AND ts >= ?2 GROUP BY 1",
        )?;
        for action in actions {
            let rows = stmt.query_map(params![action, since], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
            for row in rows {
                let (month, count) = row?;
                out.push((action.to_string(), month, count));
            }
        }
        Ok(out)
    }

    /// Who did `action` most often since `since`: (user name, count), most first.
    pub fn top_users(&self, action: &str, since: i64, limit: i64) -> Result<Vec<(String, i64)>, AppError> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT username, COUNT(*) FROM audit_log WHERE action = ?1 AND ts >= ?2
             GROUP BY username COLLATE NOCASE ORDER BY 2 DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![action, since, limit], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Rows of `table` per month of `column` since `since` (only the tables and columns below).
    pub fn rows_per_month(&self, table: &str, since: i64) -> Result<Vec<(String, i64)>, AppError> {
        let (table, column) = match table {
            "shares" => ("shares", "created_at"),
            "upload_links" => ("upload_links", "created_at"),
            "transfers" => ("transfers", "ts"),
            _ => return Err(AppError::Internal(format!("no statistics for {table}"))),
        };
        let conn = self.lock();
        let sql = format!(
            "SELECT strftime('%Y-%m', {column}, 'unixepoch'), COUNT(*) FROM {table} WHERE {column} >= ?1 GROUP BY 1"
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params![since], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Invitation links and upload links that work at `now`.
    pub fn active_link_counts(&self, now: i64) -> Result<(i64, i64), AppError> {
        let conn = self.lock();
        let shares = conn
            .prepare_cached("SELECT COUNT(*) FROM shares WHERE revoked_at IS NULL AND expires_at > ?1")?
            .query_row(params![now], |r| r.get(0))?;
        let uploads = conn
            .prepare_cached("SELECT COUNT(*) FROM upload_links WHERE revoked_at IS NULL AND expires_at > ?1")?
            .query_row(params![now], |r| r.get(0))?;
        Ok((shares, uploads))
    }

    // --------------------------------------------------------- recently viewed

    /// Notes that the rule `rule` emailed about `study_id`. `false` when it already had (the study
    /// became stable again after more images): no second email. Forgotten after 90 days.
    pub fn first_rule_notice(&self, rule: &str, study_id: &str, now: i64) -> Result<bool, AppError> {
        let conn = self.lock();
        conn.prepare_cached("DELETE FROM rule_notices WHERE ts < ?1")?.execute(params![now - 90 * 86_400])?;
        let added = conn
            .prepare_cached("INSERT OR IGNORE INTO rule_notices (rule, study_id, ts) VALUES (?1, ?2, ?3)")?
            .execute(params![rule, study_id, now])?;
        Ok(added == 1)
    }

    /// `user` looked at a study: it goes to the top of their list (which keeps [`RECENT_VIEWS_KEPT`]).
    pub fn note_view(&self, user: i64, study_id: &str, study_uid: &str, now: i64) -> Result<(), AppError> {
        let conn = self.lock();
        conn.prepare_cached(
            "INSERT INTO recent_views (user_id, study_id, study_uid, ts) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(user_id, study_id) DO UPDATE SET ts = excluded.ts, study_uid = excluded.study_uid",
        )?
        .execute(params![user, study_id, study_uid, now])?;
        conn.prepare_cached(
            "DELETE FROM recent_views WHERE user_id = ?1 AND study_id NOT IN
               (SELECT study_id FROM recent_views WHERE user_id = ?1 ORDER BY ts DESC LIMIT ?2)",
        )?
        .execute(params![user, RECENT_VIEWS_KEPT])?;
        Ok(())
    }

    /// The studies `user` looked at last, newest first: (Orthanc id, StudyInstanceUID).
    pub fn recent_views(&self, user: i64, limit: i64) -> Result<Vec<(String, String)>, AppError> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT study_id, study_uid FROM recent_views WHERE user_id = ?1 ORDER BY ts DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![user, limit], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Forgets studies of the list (gone from the archive), or the whole list when `study_ids` is empty.
    pub fn forget_views(&self, user: i64, study_ids: &[String]) -> Result<(), AppError> {
        let conn = self.lock();
        if study_ids.is_empty() {
            conn.prepare_cached("DELETE FROM recent_views WHERE user_id = ?1")?.execute(params![user])?;
        }
        for id in study_ids {
            conn.prepare_cached("DELETE FROM recent_views WHERE user_id = ?1 AND study_id = ?2")?
                .execute(params![user, id])?;
        }
        Ok(())
    }

    // ---------------------------------------------------------------- shares

    /// Creates an invitation and returns it.
    pub fn create_share(&self, new: &NewShare) -> Result<Share, AppError> {
        let id = {
            let conn = self.lock();
            conn.prepare_cached(
                "INSERT INTO shares (token, password_hash, created_at, expires_at, created_by, study_id, study_uid,
                                     label, note, max_downloads, password_copy, extra_studies, notify_email,
                                     owned_copies)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            )?
            .execute(params![
                new.token,
                new.password_hash,
                now_unix(),
                new.expires_at,
                new.created_by,
                new.study_id,
                new.study_uid,
                new.label,
                new.note,
                new.max_downloads,
                new.password_copy,
                list_json(new.extra_studies),
                new.notify_email,
                list_json(new.owned_copies),
            ])?;
            conn.last_insert_rowid()
        };
        self.get_share(id)?.ok_or(AppError::NotFound)
    }

    /// The kept password of a link that still works at `now` (`None`: no copy, or the link ended).
    pub fn share_password(&self, id: i64, now: i64) -> Result<Option<String>, AppError> {
        let conn = self.lock();
        let value = conn
            .prepare_cached(
                "SELECT password_copy FROM shares
                 WHERE id = ?1 AND revoked_at IS NULL AND expires_at > ?2 AND password_copy IS NOT NULL",
            )?
            .query_row(params![id, now], |r| r.get(0))
            .optional()?;
        Ok(value)
    }

    /// Erases the kept passwords that are no longer needed at `now`: revoked links, and links that
    /// expired more than [`EXPIRED_PASSWORD_GRACE`] ago (until then an expired link can be extended
    /// and keep its password). Every kept password when `all` (showing passwords again was switched
    /// off). Returns how many were erased.
    pub fn forget_share_passwords(&self, now: i64, all: bool) -> Result<usize, AppError> {
        let conn = self.lock();
        let erased = conn
            .prepare_cached(
                "UPDATE shares SET password_copy = NULL
                 WHERE password_copy IS NOT NULL AND (?1 OR revoked_at IS NOT NULL OR expires_at <= ?2)",
            )?
            .execute(params![all, now - EXPIRED_PASSWORD_GRACE])?;
        // The same for upload links.
        let erased_uploads = conn
            .prepare_cached(
                "UPDATE upload_links SET password_copy = NULL
                 WHERE password_copy IS NOT NULL AND (?1 OR revoked_at IS NOT NULL OR expires_at <= ?2)",
            )?
            .execute(params![all, now - EXPIRED_PASSWORD_GRACE])?;
        Ok(erased + erased_uploads)
    }

    /// Changes an invitation that was not revoked (Shares > Edit). `None` keeps a value:
    /// `expires_at` (unix seconds), `downloads_left` (how many more downloads from now on) and `note`.
    /// Returns false when the link was revoked meanwhile.
    pub fn update_share(
        &self,
        id: i64,
        expires_at: Option<i64>,
        downloads_left: Option<i64>,
        note: Option<&str>,
    ) -> Result<bool, AppError> {
        let conn = self.lock();
        // "downloads_used + left" in SQL: a download finishing at this very moment is still counted.
        let changed = conn
            .prepare_cached(
                "UPDATE shares SET expires_at = COALESCE(?2, expires_at),
                                   max_downloads = COALESCE(downloads_used + ?3, max_downloads),
                                   note = COALESCE(?4, note)
                 WHERE id = ?1 AND revoked_at IS NULL",
            )?
            .execute(params![id, expires_at, downloads_left, note])?;
        Ok(changed == 1)
    }

    /// Gives a link that was not revoked a new password (its hash, and the copy when passwords are kept).
    pub fn set_share_password(&self, id: i64, hash: &str, copy: Option<&str>) -> Result<bool, AppError> {
        let conn = self.lock();
        let changed = conn
            .prepare_cached(
                "UPDATE shares SET password_hash = ?2, password_copy = ?3 WHERE id = ?1 AND revoked_at IS NULL",
            )?
            .execute(params![id, hash, copy])?;
        Ok(changed == 1)
    }

    pub fn get_share(&self, id: i64) -> Result<Option<Share>, AppError> {
        let conn = self.lock();
        let sql = format!("SELECT {SHARE_COLUMNS} FROM shares WHERE id = ?1");
        let value = conn.prepare_cached(&sql)?.query_row(params![id], row_to_share).optional()?;
        Ok(value)
    }

    pub fn get_share_by_token(&self, token: &str) -> Result<Option<Share>, AppError> {
        let conn = self.lock();
        let sql = format!("SELECT {SHARE_COLUMNS} FROM shares WHERE token = ?1");
        let value = conn.prepare_cached(&sql)?.query_row(params![token], row_to_share).optional()?;
        Ok(value)
    }

    /// Newest first. `only_user = Some(name)` lists just the invitations that user created.
    pub fn list_shares(&self, only_user: Option<&str>, limit: i64) -> Result<Vec<Share>, AppError> {
        let conn = self.lock();
        let sql = format!(
            "SELECT {SHARE_COLUMNS} FROM shares WHERE (?1 IS NULL OR created_by = ?1 COLLATE NOCASE)
             ORDER BY id DESC LIMIT ?2"
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params![only_user, limit], row_to_share)?;
        let shares = rows.collect::<Result<Vec<_>, _>>()?;
        Ok(shares)
    }

    /// Takes ONE download from the invitation, atomically: the condition and the increment
    /// are a single SQL statement, so two simultaneous downloads cannot both use the last
    /// one. Returns `false` when nothing was taken (none left, expired or revoked).
    pub fn reserve_download(&self, id: i64, now: i64) -> Result<bool, AppError> {
        let conn = self.lock();
        let changed = conn
            .prepare_cached(
                "UPDATE shares SET downloads_used = downloads_used + 1
             WHERE id = ?1 AND revoked_at IS NULL AND expires_at > ?2 AND downloads_used < max_downloads",
            )?
            .execute(params![id, now])?;
        Ok(changed == 1)
    }

    /// Gives a reserved download back (the archive could not even be started).
    pub fn refund_download(&self, id: i64) -> Result<(), AppError> {
        let conn = self.lock();
        conn.prepare_cached("UPDATE shares SET downloads_used = MAX(downloads_used - 1, 0) WHERE id = ?1")?
            .execute(params![id])?;
        Ok(())
    }

    /// Switches an invitation off. Returns `false` if it was already revoked or does not exist.
    pub fn revoke_share(&self, id: i64, now: i64) -> Result<bool, AppError> {
        let conn = self.lock();
        let changed = conn
            .prepare_cached(
                "UPDATE shares SET revoked_at = ?2, password_copy = NULL WHERE id = ?1 AND revoked_at IS NULL",
            )?
            .execute(params![id, now])?;
        Ok(changed == 1)
    }

    /// Counts a successful password entry.
    pub fn record_share_access(&self, id: i64, now: i64) -> Result<(), AppError> {
        let conn = self.lock();
        conn.prepare_cached("UPDATE shares SET access_count = access_count + 1, last_access_at = ?2 WHERE id = ?1")?
            .execute(params![id, now])?;
        Ok(())
    }

    /// True the FIRST time it is called for a link: "your link was opened" is said once.
    pub fn first_open_notice(&self, id: i64) -> Result<bool, AppError> {
        let conn = self.lock();
        let changed = conn
            .prepare_cached("UPDATE shares SET notified_open = 1 WHERE id = ?1 AND notified_open = 0")?
            .execute(params![id])?;
        Ok(changed == 1)
    }

    /// Links whose anonymized copies must go: revoked links, and links that ended more than
    /// [`EXPIRED_PASSWORD_GRACE`] ago (until then they can be extended). `(id, copies)`.
    pub fn copies_to_delete(&self, now: i64) -> Result<Vec<(i64, Vec<String>)>, AppError> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT id, owned_copies FROM shares WHERE owned_copies IS NOT NULL AND copies_deleted = 0
               AND (revoked_at IS NOT NULL OR expires_at <= ?1)",
        )?;
        let rows = stmt.query_map(params![now - EXPIRED_PASSWORD_GRACE], |r| Ok((r.get(0)?, json_list(r.get(1)?))))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The anonymized copies of a link were deleted: it cannot be used or extended any more.
    pub fn mark_copies_deleted(&self, id: i64) -> Result<(), AppError> {
        let conn = self.lock();
        conn.prepare_cached("UPDATE shares SET copies_deleted = 1, password_copy = NULL WHERE id = ?1")?
            .execute(params![id])?;
        Ok(())
    }

    // ---------------------------------------------------------- upload links

    pub fn create_upload_link(&self, new: &NewUploadLink) -> Result<UploadLink, AppError> {
        let id = {
            let conn = self.lock();
            conn.prepare_cached(
                "INSERT INTO upload_links (token, password_hash, password_copy, created_at, expires_at, created_by,
                                           note, label, max_bytes, notify_email)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )?
            .execute(params![
                new.token,
                new.password_hash,
                new.password_copy,
                now_unix(),
                new.expires_at,
                new.created_by,
                new.note,
                new.label,
                new.max_bytes,
                new.notify_email
            ])?;
            conn.last_insert_rowid()
        };
        self.get_upload_link(id)?.ok_or(AppError::NotFound)
    }

    pub fn get_upload_link(&self, id: i64) -> Result<Option<UploadLink>, AppError> {
        let conn = self.lock();
        let sql = format!("SELECT {UPLOAD_LINK_COLUMNS} FROM upload_links WHERE id = ?1");
        let value = conn.prepare_cached(&sql)?.query_row(params![id], row_to_upload_link).optional()?;
        Ok(value)
    }

    pub fn get_upload_link_by_token(&self, token: &str) -> Result<Option<UploadLink>, AppError> {
        let conn = self.lock();
        let sql = format!("SELECT {UPLOAD_LINK_COLUMNS} FROM upload_links WHERE token = ?1");
        let value = conn.prepare_cached(&sql)?.query_row(params![token], row_to_upload_link).optional()?;
        Ok(value)
    }

    /// Newest first; `only_user` = the links that person created.
    pub fn list_upload_links(&self, only_user: Option<&str>, limit: i64) -> Result<Vec<UploadLink>, AppError> {
        let conn = self.lock();
        let sql = format!(
            "SELECT {UPLOAD_LINK_COLUMNS} FROM upload_links WHERE (?1 IS NULL OR created_by = ?1 COLLATE NOCASE)
             ORDER BY id DESC LIMIT ?2"
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params![only_user, limit], row_to_upload_link)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn revoke_upload_link(&self, id: i64, now: i64) -> Result<bool, AppError> {
        let conn = self.lock();
        let changed = conn
            .prepare_cached(
                "UPDATE upload_links SET revoked_at = ?2, password_copy = NULL WHERE id = ?1 AND revoked_at IS NULL",
            )?
            .execute(params![id, now])?;
        Ok(changed == 1)
    }

    /// Changes an upload link that was not revoked: more time (unix seconds) and a bigger size limit.
    pub fn update_upload_link(
        &self,
        id: i64,
        expires_at: Option<i64>,
        max_bytes: Option<i64>,
    ) -> Result<bool, AppError> {
        let conn = self.lock();
        let changed = conn
            .prepare_cached(
                "UPDATE upload_links SET expires_at = COALESCE(?2, expires_at), max_bytes = COALESCE(?3, max_bytes)
                 WHERE id = ?1 AND revoked_at IS NULL",
            )?
            .execute(params![id, expires_at, max_bytes])?;
        Ok(changed == 1)
    }

    /// The kept password of an upload link that still works.
    pub fn upload_link_password(&self, id: i64, now: i64) -> Result<Option<String>, AppError> {
        let conn = self.lock();
        let value = conn
            .prepare_cached(
                "SELECT password_copy FROM upload_links
                 WHERE id = ?1 AND revoked_at IS NULL AND expires_at > ?2 AND password_copy IS NOT NULL",
            )?
            .query_row(params![id, now], |r| r.get(0))
            .optional()?;
        Ok(value)
    }

    /// Takes `bytes` of the link's size limit for a file about to be sent, atomically. False when
    /// it would go over the limit (or the link ended).
    pub fn reserve_upload_bytes(&self, id: i64, bytes: i64, now: i64) -> Result<bool, AppError> {
        let conn = self.lock();
        let changed = conn
            .prepare_cached(
                "UPDATE upload_links SET bytes_received = bytes_received + ?2
                 WHERE id = ?1 AND revoked_at IS NULL AND expires_at > ?3 AND bytes_received + ?2 <= max_bytes",
            )?
            .execute(params![id, bytes, now])?;
        Ok(changed == 1)
    }

    /// Gives reserved bytes back (Orthanc did not take the file).
    pub fn refund_upload_bytes(&self, id: i64, bytes: i64) -> Result<(), AppError> {
        let conn = self.lock();
        conn.prepare_cached("UPDATE upload_links SET bytes_received = MAX(bytes_received - ?2, 0) WHERE id = ?1")?
            .execute(params![id, bytes])?;
        Ok(())
    }

    /// Records a received file: accepted (and the study it went to) or rejected.
    pub fn record_upload_file(&self, id: i64, study: Option<&str>, now: i64) -> Result<(), AppError> {
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        match study {
            Some(study) => {
                let stored: Option<String> = tx
                    .query_row("SELECT studies FROM upload_links WHERE id = ?1", params![id], |r| r.get(0))
                    .optional()?
                    .flatten();
                let mut studies: Vec<String> = json_list(stored);
                if !studies.iter().any(|s| s == study) && studies.len() < MAX_UPLOAD_LINK_STUDIES {
                    studies.push(study.to_string());
                }
                tx.execute(
                    "UPDATE upload_links SET files_received = files_received + 1, last_upload_at = ?2, studies = ?3
                     WHERE id = ?1",
                    params![id, now, list_json(&studies)],
                )?;
            }
            None => {
                tx.execute(
                    "UPDATE upload_links SET files_rejected = files_rejected + 1, last_upload_at = ?2 WHERE id = ?1",
                    params![id, now],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn record_upload_link_access(&self, id: i64) -> Result<(), AppError> {
        let conn = self.lock();
        conn.prepare_cached("UPDATE upload_links SET access_count = access_count + 1 WHERE id = ?1")?
            .execute(params![id])?;
        Ok(())
    }

    /// True when the creator should hear about new files now: never told, or told more than
    /// `quiet_secs` ago (one email per batch, not one per file). The time is noted at once.
    pub fn upload_notice_due(&self, id: i64, now: i64, quiet_secs: i64) -> Result<bool, AppError> {
        let conn = self.lock();
        let changed = conn
            .prepare_cached(
                "UPDATE upload_links SET last_notified_at = ?2
                 WHERE id = ?1 AND notify_email IS NOT NULL AND (last_notified_at IS NULL OR last_notified_at < ?3)",
            )?
            .execute(params![id, now, now - quiet_secs])?;
        Ok(changed == 1)
    }
}
