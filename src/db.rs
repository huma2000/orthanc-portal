//! # db.rs
//!
//! Persistent storage (SQLite, a single file) for:
//!   * **users**: name, password hash, permissions, active flag;
//!   * **audit_log**: who did what and when (login, upload, download, user changes...);
//!   * **transfers**: studies sent to remote DICOM nodes, with the Orthanc job id;
//!   * **settings**: the configuration saved from the web interface (one JSON document);
//!   * **branding**: the logo and website icon uploaded from the Settings screen;
//!   * **shares**: invitation links that give one outside person access to one study.
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
    /// Disabled accounts cannot log in and their sessions stop working immediately.
    pub active: bool,
    pub created_at: i64,
    pub last_login: Option<i64>,
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
}

/// An invitation: a password-protected link that lets ONE outside person look at ONE study
/// (and download it a limited number of times) until it expires. See `routes/guest.rs`.
#[derive(Debug, Clone, Serialize)]
pub struct Share {
    pub id: i64,
    /// Random, unguessable part of the link (`/s/<token>`).
    pub token: String,
    /// Argon2 hash of the generated password; the password itself is shown once, at creation.
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
}

impl Share {
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
}

const SHARE_COLUMNS: &str =
    "id, token, password_hash, created_at, expires_at, created_by, study_id, study_uid, label, \
                             note, max_downloads, downloads_used, revoked_at, access_count, last_access_at";

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
        last_access_at: row.get(14)?,
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
}

/// Columns read for a user, always in this order (see [`row_to_user`]).
const USER_COLUMNS: &str = "id, username, password_hash, permissions, active, created_at, last_login";

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
    })
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
                 last_login    INTEGER
             );

             CREATE TABLE IF NOT EXISTS audit_log (
                 id       INTEGER PRIMARY KEY AUTOINCREMENT,
                 ts       INTEGER NOT NULL,
                 username TEXT    NOT NULL,
                 action   TEXT    NOT NULL,
                 detail   TEXT    NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_audit_ts ON audit_log (ts);

             CREATE TABLE IF NOT EXISTS transfers (
                 id       INTEGER PRIMARY KEY AUTOINCREMENT,
                 ts       INTEGER NOT NULL,
                 username TEXT    NOT NULL,
                 study_id TEXT    NOT NULL,
                 label    TEXT    NOT NULL,
                 target   TEXT    NOT NULL,
                 job_id   TEXT    NOT NULL,
                 final_state TEXT,                                      -- set once the job has ended
                 final_error TEXT
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
                 last_access_at INTEGER
             );

             -- Logo and website icon uploaded from the Settings screen (kind = 'logo' | 'favicon').
             CREATE TABLE IF NOT EXISTS branding (
                 kind         TEXT PRIMARY KEY,
                 content_type TEXT    NOT NULL,
                 data         BLOB    NOT NULL,
                 updated_at   INTEGER NOT NULL
             );

             -- Configuration saved from the Settings screen: a single row, key 'main'.
             CREATE TABLE IF NOT EXISTS settings (
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );",
        )?;

        // Databases created by older versions have no `final_state` / `final_error` columns.
        for column in ["final_state", "final_error"] {
            let exists = conn
                .prepare("SELECT 1 FROM pragma_table_info('transfers') WHERE name = ?1")?
                .exists(params![column])?;
            if !exists {
                conn.execute(&format!("ALTER TABLE transfers ADD COLUMN {column} TEXT"), [])?;
            }
        }

        // The file holds password hashes and the Orthanc password: only its owner may read it.
        // (The -wal / -shm files SQLite creates next to it copy these permissions.)
        restrict_file_permissions(path);
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    /// Locks the connection. A poisoned mutex (a panic while locked) is recovered
    /// because SQLite itself stays consistent.
    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ----------------------------------------------------------------- users

    pub fn count_users(&self) -> Result<i64, AppError> {
        let conn = self.lock();
        Ok(conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?)
    }

    /// Inserts a new active user. Returns `Conflict` if the name is already taken.
    pub fn create_user(
        &self,
        username: &str,
        password_hash: &str,
        permissions: &PermissionSet,
    ) -> Result<User, AppError> {
        let id = {
            let conn = self.lock();
            let result = conn.execute(
                "INSERT INTO users (username, password_hash, permissions, active, created_at)
                 VALUES (?1, ?2, ?3, 1, ?4)",
                params![username, password_hash, permissions.to_db_string(), now_unix()],
            );
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
        Ok(conn.query_row(&sql, params![id], row_to_user).optional()?)
    }

    pub fn get_user_by_name(&self, username: &str) -> Result<Option<User>, AppError> {
        let conn = self.lock();
        let sql = format!("SELECT {USER_COLUMNS} FROM users WHERE username = ?1");
        Ok(conn.query_row(&sql, params![username], row_to_user).optional()?)
    }

    pub fn list_users(&self) -> Result<Vec<User>, AppError> {
        let conn = self.lock();
        let sql = format!("SELECT {USER_COLUMNS} FROM users ORDER BY username");
        let mut stmt = conn.prepare(&sql)?;
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
    ) -> Result<(), AppError> {
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
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

    pub fn delete_user(&self, id: i64) -> Result<(), AppError> {
        let conn = self.lock();
        conn.execute("DELETE FROM users WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Remembers the time of the last successful login.
    pub fn touch_login(&self, id: i64) -> Result<(), AppError> {
        let conn = self.lock();
        conn.execute("UPDATE users SET last_login = ?1 WHERE id = ?2", params![now_unix(), id])?;
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
        if let Err(e) = conn.execute(
            "INSERT INTO audit_log (ts, username, action, detail) VALUES (?1, ?2, ?3, ?4)",
            params![now_unix(), username, action, detail],
        ) {
            tracing::warn!("could not write audit log entry: {e}");
        }
    }

    /// The most recent audit entries, newest first.
    pub fn recent_audit(&self, limit: i64) -> Result<Vec<AuditEntry>, AppError> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare("SELECT id, ts, username, action, detail FROM audit_log ORDER BY id DESC LIMIT ?1")?;
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
    ) -> Result<i64, AppError> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO transfers (ts, username, study_id, label, target, job_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![now_unix(), username, study_id, label, target, job_id],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Latest transfers, newest first. `only_user = Some(name)` restricts the list
    /// to that user's own transfers (administrators pass `None` to see them all).
    pub fn recent_transfers(&self, only_user: Option<&str>, limit: i64) -> Result<Vec<Transfer>, AppError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, ts, username, study_id, label, target, job_id, final_state, final_error FROM transfers
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
            })
        })?;
        let transfers = rows.collect::<Result<Vec<_>, _>>()?;
        Ok(transfers)
    }

    // -------------------------------------------------------------- settings

    /// The settings document saved from the web interface, if any (JSON text).
    pub fn load_settings(&self) -> Result<Option<String>, AppError> {
        let conn = self.lock();
        Ok(conn.query_row("SELECT value FROM settings WHERE key = 'main'", [], |r| r.get(0)).optional()?)
    }

    /// Stores (replaces) the settings document.
    pub fn save_settings(&self, json: &str) -> Result<(), AppError> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO settings (key, value) VALUES ('main', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![json],
        )?;
        Ok(())
    }

    /// Forgets the saved settings (emergency recovery with `RESET_SETTINGS=true`).
    pub fn delete_settings(&self) -> Result<(), AppError> {
        let conn = self.lock();
        conn.execute("DELETE FROM settings WHERE key = 'main'", [])?;
        Ok(())
    }

    // -------------------------------------------------------------- branding

    /// Every uploaded branding image: `(kind, content type, bytes, updated_at)`.
    pub fn load_branding(&self) -> Result<Vec<BrandingRow>, AppError> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT kind, content_type, data, updated_at FROM branding")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        let all = rows.collect::<Result<Vec<_>, _>>()?;
        Ok(all)
    }

    /// Stores (replaces) the image of one kind.
    pub fn save_branding(&self, kind: &str, content_type: &str, data: &[u8], updated_at: i64) -> Result<(), AppError> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO branding (kind, content_type, data, updated_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(kind) DO UPDATE SET content_type = excluded.content_type,
                                             data = excluded.data, updated_at = excluded.updated_at",
            params![kind, content_type, data, updated_at],
        )?;
        Ok(())
    }

    /// Removes the image of one kind (the default is used again).
    pub fn delete_branding(&self, kind: &str) -> Result<(), AppError> {
        let conn = self.lock();
        conn.execute("DELETE FROM branding WHERE kind = ?1", params![kind])?;
        Ok(())
    }

    /// Remembers how a transfer ended, so Orthanc is not asked about it again.
    pub fn set_transfer_final(&self, id: i64, state: &str, error: Option<&str>) -> Result<(), AppError> {
        let conn = self.lock();
        conn.execute(
            "UPDATE transfers SET final_state = ?1, final_error = ?2 WHERE id = ?3",
            params![state, error, id],
        )?;
        Ok(())
    }

    /// Housekeeping: deletes audit entries and transfers older than `cutoff` (unix
    /// seconds). Returns how many rows were removed.
    pub fn purge_older_than(&self, cutoff: i64) -> Result<usize, AppError> {
        let conn = self.lock();
        let audit = conn.execute("DELETE FROM audit_log WHERE ts < ?1", params![cutoff])?;
        let transfers = conn.execute("DELETE FROM transfers WHERE ts < ?1", params![cutoff])?;
        // Links that expired before the cut-off are no use any more either.
        let shares = conn.execute("DELETE FROM shares WHERE expires_at < ?1", params![cutoff])?;
        Ok(audit + transfers + shares)
    }

    // ---------------------------------------------------------------- shares

    /// Creates an invitation and returns it.
    pub fn create_share(&self, new: &NewShare) -> Result<Share, AppError> {
        let id = {
            let conn = self.lock();
            conn.execute(
                "INSERT INTO shares (token, password_hash, created_at, expires_at, created_by, study_id, study_uid,
                                     label, note, max_downloads)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    new.token,
                    new.password_hash,
                    now_unix(),
                    new.expires_at,
                    new.created_by,
                    new.study_id,
                    new.study_uid,
                    new.label,
                    new.note,
                    new.max_downloads
                ],
            )?;
            conn.last_insert_rowid()
        };
        self.get_share(id)?.ok_or(AppError::NotFound)
    }

    pub fn get_share(&self, id: i64) -> Result<Option<Share>, AppError> {
        let conn = self.lock();
        let sql = format!("SELECT {SHARE_COLUMNS} FROM shares WHERE id = ?1");
        Ok(conn.query_row(&sql, params![id], row_to_share).optional()?)
    }

    pub fn get_share_by_token(&self, token: &str) -> Result<Option<Share>, AppError> {
        let conn = self.lock();
        let sql = format!("SELECT {SHARE_COLUMNS} FROM shares WHERE token = ?1");
        Ok(conn.query_row(&sql, params![token], row_to_share).optional()?)
    }

    /// Newest first. `only_user = Some(name)` lists just the invitations that user created.
    pub fn list_shares(&self, only_user: Option<&str>, limit: i64) -> Result<Vec<Share>, AppError> {
        let conn = self.lock();
        let sql = format!(
            "SELECT {SHARE_COLUMNS} FROM shares WHERE (?1 IS NULL OR created_by = ?1 COLLATE NOCASE)
             ORDER BY id DESC LIMIT ?2"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![only_user, limit], row_to_share)?;
        let shares = rows.collect::<Result<Vec<_>, _>>()?;
        Ok(shares)
    }

    /// Takes ONE download from the invitation, atomically: the condition and the increment
    /// are a single SQL statement, so two simultaneous downloads cannot both use the last
    /// one. Returns `false` when nothing was taken (none left, expired or revoked).
    pub fn reserve_download(&self, id: i64, now: i64) -> Result<bool, AppError> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE shares SET downloads_used = downloads_used + 1
             WHERE id = ?1 AND revoked_at IS NULL AND expires_at > ?2 AND downloads_used < max_downloads",
            params![id, now],
        )?;
        Ok(changed == 1)
    }

    /// Gives a reserved download back (the archive could not even be started).
    pub fn refund_download(&self, id: i64) -> Result<(), AppError> {
        let conn = self.lock();
        conn.execute("UPDATE shares SET downloads_used = MAX(downloads_used - 1, 0) WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Switches an invitation off. Returns `false` if it was already revoked or does not exist.
    pub fn revoke_share(&self, id: i64, now: i64) -> Result<bool, AppError> {
        let conn = self.lock();
        let changed =
            conn.execute("UPDATE shares SET revoked_at = ?2 WHERE id = ?1 AND revoked_at IS NULL", params![id, now])?;
        Ok(changed == 1)
    }

    /// Counts a successful password entry.
    pub fn record_share_access(&self, id: i64, now: i64) -> Result<(), AppError> {
        let conn = self.lock();
        conn.execute(
            "UPDATE shares SET access_count = access_count + 1, last_access_at = ?2 WHERE id = ?1",
            params![id, now],
        )?;
        Ok(())
    }
}
