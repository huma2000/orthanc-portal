//! # routes/backup.rs
//!
//! **Backups of the portal's own database**: users, settings, the audit log, share and upload links.
//! (The images live in Orthanc, which has its own backups.) Administrators, intranet only.
//!
//! * Automatic: every `backup_interval_hours` (Settings > Backups) a copy is written to `backup_dir`
//!   as `portal-backup-YYYYMMDD-HHMMSS.db`; only the newest `backup_keep` are kept.
//! * `GET  /api/admin/backups`           the copies in the folder, and when the next one is due
//! * `POST /api/admin/backups`           make one now (in the folder)
//! * `GET  /api/admin/backup/download`   a fresh copy, downloaded by the browser (nothing kept)
//!
//! The copy is made with SQLite's `VACUUM INTO`: consistent even while the portal is working, and
//! compact. It holds password hashes and the Orthanc / mail server passwords: keep it safe (the
//! files are readable by their owner only). To restore: stop the portal, put the copy in place of
//! the database file (DATABASE_PATH, in Docker /data/portal.db), start it again.

use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use crate::permissions::Permission;
use crate::state::AppState;
use crate::util::{now_unix, random_token};
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::Json;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tokio::io::AsyncReadExt;

const PREFIX: &str = "portal-backup-";

/// `portal-backup-20241119-031500.db` for a moment (UTC).
fn backup_name(now: i64) -> String {
    let (y, m, d) = civil(now.div_euclid(86_400));
    let secs = now.rem_euclid(86_400);
    format!("{PREFIX}{y:04}{m:02}{d:02}-{:02}{:02}{:02}.db", secs / 3600, (secs % 3600) / 60, secs % 60)
}

fn civil(days: i64) -> (i64, u32, u32) {
    // Same algorithm as routes/export.rs (Howard Hinnant's days-from-civil, inverted).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// The automatic backups in `dir`: (name, size, modified unix time), newest first.
fn list(dir: &Path) -> Vec<(String, u64, i64)> {
    let mut found: Vec<(String, u64, i64)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with(PREFIX) || !name.ends_with(".db") {
                return None;
            }
            let meta = entry.metadata().ok()?;
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            Some((name, meta.len(), modified))
        })
        .collect();
    // The names sort by time.
    found.sort_by(|a, b| b.0.cmp(&a.0));
    found
}

/// Writes one backup into the folder of the settings and deletes the oldest beyond `backup_keep`.
/// Returns the file written.
pub async fn run_backup(state: &AppState, by: &str) -> AppResult<PathBuf> {
    let settings = state.settings.get();
    if settings.backup_dir.is_empty() {
        return Err(AppError::BadRequest("Choose the backup folder first (Settings > Backups)".into()));
    }
    let dir = PathBuf::from(&settings.backup_dir);
    let keep = settings.backup_keep as usize;
    let db = state.db.clone();
    let target = dir.join(backup_name(now_unix()));
    let written = target.clone();
    tokio::task::spawn_blocking(move || -> AppResult<()> {
        std::fs::create_dir_all(&dir)
            .map_err(|e| AppError::BadRequest(format!("The backup folder {} cannot be created: {e}", dir.display())))?;
        if target.exists() {
            return Ok(()); // two backups in the same second: the first one is enough
        }
        db.backup_into(&target).map_err(|e| match e {
            AppError::Internal(detail) => {
                AppError::BadRequest(format!("The backup could not be written to {}: {detail}", dir.display()))
            }
            other => other,
        })?;
        for (old, _, _) in list(&dir).into_iter().skip(keep) {
            let _ = std::fs::remove_file(dir.join(old));
        }
        Ok(())
    })
    .await
    .map_err(|e| AppError::Internal(format!("backup task failed: {e}")))??;
    state.db.audit(by, "backup_written", &written.display().to_string());
    Ok(written)
}

/// Housekeeping (main.rs): a backup when the newest one is older than the interval.
pub async fn backup_if_due(state: &AppState) {
    let settings = state.settings.get();
    if settings.backup_dir.is_empty() {
        return;
    }
    let newest = list(Path::new(&settings.backup_dir)).first().map(|b| b.2).unwrap_or(0);
    if now_unix() - newest < (settings.backup_interval_hours as i64) * 3600 {
        return;
    }
    match run_backup(state, "portal").await {
        Ok(path) => tracing::info!("Backup of the portal's database written to {}", path.display()),
        Err(e) => {
            tracing::error!("The automatic backup failed: {e}");
            state.db.audit("portal", "backup_failed", &e.to_string());
        }
    }
}

pub async fn list_backups(State(state): State<AppState>, AuthUser(user): AuthUser) -> AppResult<Json<Value>> {
    user.require(Permission::Admin)?;
    let settings = state.settings.get();
    let backups = if settings.backup_dir.is_empty() { Vec::new() } else { list(Path::new(&settings.backup_dir)) };
    let next = backups.first().map(|b| b.2 + (settings.backup_interval_hours as i64) * 3600);
    Ok(Json(json!({
        "dir": settings.backup_dir,
        "backups": backups.iter().map(|(name, size, at)| json!({ "name": name, "size": size, "at": at })).collect::<Vec<_>>(),
        "next": next,
        "database": state.db.data_dir().display().to_string(),
    })))
}

pub async fn backup_now(State(state): State<AppState>, AuthUser(user): AuthUser) -> AppResult<Json<Value>> {
    user.require(Permission::Admin)?;
    let path = run_backup(&state, &user.username).await?;
    Ok(Json(json!({ "written": path.display().to_string() })))
}

/// Deletes its file when dropped (after the download, or if it fails).
struct TempFile(PathBuf);
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A fresh copy of the database, downloaded at once (nothing is kept on the server).
pub async fn download(State(state): State<AppState>, AuthUser(user): AuthUser) -> AppResult<Response> {
    user.require(Permission::Admin)?;
    let dir = crate::routes::export::export_dir(&state);
    std::fs::create_dir_all(&dir).map_err(|e| AppError::Internal(format!("cannot create {}: {e}", dir.display())))?;
    let temp = TempFile(dir.join(format!("backup-{}.db", &random_token()[..16])));
    let (db, path) = (state.db.clone(), temp.0.clone());
    tokio::task::spawn_blocking(move || db.backup_into(&path))
        .await
        .map_err(|e| AppError::Internal(format!("backup task failed: {e}")))??;
    let size = std::fs::metadata(&temp.0).map(|m| m.len()).unwrap_or(0);
    let name = backup_name(now_unix());
    state.db.audit(&user.username, "backup_downloaded", &name);
    let file =
        tokio::fs::File::open(&temp.0).await.map_err(|e| AppError::Internal(format!("cannot read the backup: {e}")))?;
    let stream = futures_util::stream::unfold((file, temp), |(mut file, guard)| async move {
        let mut buffer = vec![0u8; 256 * 1024];
        match file.read(&mut buffer).await {
            Ok(0) => None,
            Ok(n) => {
                buffer.truncate(n);
                Some((Ok::<Bytes, std::io::Error>(Bytes::from(buffer)), (file, guard)))
            }
            Err(e) => Some((Err(e), (file, guard))),
        }
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/vnd.sqlite3")
        .header(header::CONTENT_LENGTH, size)
        .header(header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\""))
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(stream))
        .map_err(|e| AppError::Internal(format!("cannot build the backup response: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_names_sort_by_time() {
        assert_eq!(backup_name(1_731_985_500), "portal-backup-20241119-030500.db");
        assert!(backup_name(1_000_000) < backup_name(2_000_000));
    }

    /// A copy made while the database is open can be opened and holds the data.
    #[test]
    fn a_backup_is_a_working_database() {
        let dir = std::env::temp_dir().join(format!("portal-backup-test-{}", random_token()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = crate::db::Db::open(&dir.join("portal.db").to_string_lossy()).unwrap();
        db.audit("tester", "something", "kept in the copy");
        let copy = dir.join(backup_name(now_unix()));
        db.backup_into(&copy).unwrap();
        let restored = crate::db::Db::open(&copy.to_string_lossy()).unwrap();
        assert_eq!(restored.recent_audit(5).unwrap()[0].detail, "kept in the copy");
        assert_eq!(list(&dir).len(), 1);
        drop((db, restored));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
