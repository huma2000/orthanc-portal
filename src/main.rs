//! # Orthanc Portal
//!
//! A web front-end for an **Orthanc PACS** (recent releases, 1.12.5 or newer),
//! written in Rust (axum).
//!
//! * Consult: search studies and series, filter by modality (CT, MR, US, RX...).
//! * Visualise: Weasis (desktop) or OHIF (in the browser), both fed through an
//!   authenticated read-only proxy so Orthanc is never exposed.
//! * Upload DICOM files / ZIPs.   * Download studies or series as ZIP.
//! * Send a study to a remote DICOM node (an Orthanc "modality") and follow it.
//! * Users with per-user permissions (read / visualize / upload / download / send / admin).
//! * **Everything is configured from the web interface** by administrators:
//!   Orthanc connection, ports, sessions, upload limit, roles, DICOM nodes, users.
//!
//! ## Intranet vs internet
//! The program listens on **two ports** (both changeable in Settings):
//! * default 0.0.0.0:8080 - the full interface, for the intranet;
//! * default 0.0.0.0:8081 - VIEW-ONLY. Point the Cloudflare Tunnel at this one:
//!   whatever a user's permissions are, they can only search and view studies
//!   when they come in through it. Settings are never reachable from there.
//!
//! ## Start-up in one paragraph
//! Only three things come from the environment because they are needed before
//! the database can be read: `DATABASE_PATH`, the first administrator
//! (`ADMIN_USERNAME`/`ADMIN_PASSWORD`) and the recovery flag `RESET_SETTINGS`.
//! All other settings are read from the database; the old environment variables
//! (`ORTHANC_URL`, `BIND_ADDR`...) are only used as the initial values until an
//! administrator saves the Settings form.
//!
//! ## Source layout
//! | File                 | Role                                                   |
//! |----------------------|--------------------------------------------------------|
//! | `main.rs`            | start-up, housekeeping, graceful shutdown              |
//! | `config.rs`          | the few bootstrap values read from the environment     |
//! | `settings.rs`        | all settings editable from the web (stored in SQLite)  |
//! | `server.rs`          | runs / re-opens the two HTTP listeners at runtime      |
//! | `error.rs`           | `AppError` -> HTTP status + JSON                       |
//! | `permissions.rs`     | permissions, intranet/internet access scope            |
//! | `db.rs`              | SQLite: users, audit log, transfers, settings          |
//! | `auth.rs`            | passwords, sessions, tokens, scope detection, guards   |
//! | `orthanc.rs`         | client for Orthanc's REST API (reconfigurable live)    |
//! | `branding.rs`        | uploaded logo / website icon: validation + memory copy |
//! | `thumbnails.rs`      | picks the preview image of a study/series + memory cache |
//! | `models.rs`          | study / series / DICOM-node DTOs sent to the browser   |
//! | `util.rs`            | validation and helper functions                        |
//! | `state.rs`           | shared application state                               |
//! | `routes/*`           | HTTP endpoints (see `routes/mod.rs` for the URL map)   |

mod auth;
mod branding;
mod config;
mod db;
mod error;
mod models;
mod orthanc;
mod permissions;
mod routes;
mod server;
mod settings;
mod state;
mod thumbnails;
mod util;

use crate::auth::{LoginThrottle, TokenStore};
use crate::branding::BrandingStore;
use crate::config::Bootstrap;
use crate::db::Db;
use crate::orthanc::{OrthancClient, MIN_VERSION};
use crate::permissions::AccessScope;
use crate::server::ListenerManager;
use crate::settings::{Settings, SettingsStore};
use crate::state::AppState;
use crate::thumbnails::ThumbnailCache;
use crate::util::now_unix;
use anyhow::anyhow;
use std::io::IsTerminal;
use std::sync::Arc;
use std::time::Duration;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Load `.env` if present (ignored when absent), then start logging.
    //    Log level can be changed with RUST_LOG, e.g. RUST_LOG=debug.
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        // Colour codes only when a person is watching a terminal, not in log files / Docker logs.
        .with_ansi(std::io::stdout().is_terminal())
        .init();

    // 2. Database and the very first administrator account.
    let boot = Bootstrap::from_env()?;
    let db = Db::open(&boot.database_path)?;
    auth::ensure_initial_admin(&db, &boot).await?;

    // 3. Settings: saved ones win, the environment only fills what was never saved.
    if boot.reset_settings {
        db.delete_settings()?;
        tracing::warn!("RESET_SETTINGS is on: the settings saved from the web interface were discarded");
    }
    let settings = Settings::merged_with_stored(Settings::from_env()?, db.load_settings()?);

    // 4. Shared state. Each listener gets its own copy, differing only in `listener_scope`.
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let mut stop_wait = stop_rx.clone();
    let listeners = ListenerManager::new(stop_rx);
    let orthanc = OrthancClient::new(&settings)?;
    let branding = BrandingStore::load(&db)?; // uploaded logo / website icon
    let state = AppState {
        settings: SettingsStore::new(settings.clone()),
        db,
        orthanc: orthanc.clone(),
        sessions: TokenStore::default(),
        weasis_tokens: TokenStore::default(),
        throttle: LoginThrottle::default(),
        listener_scope: AccessScope::Full,
        listeners: listeners.clone(),
        thumbs: ThumbnailCache::new(),
        branding,
        admin_lock: Arc::new(tokio::sync::Mutex::new(())),
    };

    // Compute the throw-away password hash used for unknown user names now, so the very first
    // failed login does not take longer than the following ones.
    tokio::spawn(async {
        let _ = auth::verify_password(None, "warm-up".into()).await;
    });

    // 5. Orthanc check in the background: an unreachable or too-old Orthanc must not
    //    keep the portal down, because an administrator fixes it in the Settings screen.
    tokio::spawn(check_orthanc(orthanc));

    // 6. Housekeeping task: every 10 minutes forget expired sessions/tokens, and delete
    //    audit/transfer records older than the retention period (Settings; 0 = keep all).
    {
        let state = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(600));
            loop {
                tick.tick().await;
                state.sessions.purge_expired();
                state.weasis_tokens.purge_expired();
                state.throttle.purge_old();

                let days = state.settings.get().audit_retention_days;
                if days > 0 {
                    match state.db.purge_older_than(now_unix() - (days as i64) * 86_400) {
                        Ok(removed) if removed > 0 => {
                            tracing::info!("Deleted {removed} audit/transfer records older than {days} days")
                        }
                        Ok(_) => {}
                        Err(e) => tracing::warn!("audit clean-up failed: {e}"),
                    }
                }
            }
        });
    }

    // 7. Stop signal (Ctrl+C / SIGTERM = docker stop) shared by both listeners.
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = stop_tx.send(true);
    });

    // 8. Start the listeners. The intranet one is essential; if its saved address
    //    cannot be opened (port taken after a reboot...) fall back to 0.0.0.0:8080
    //    so an administrator can still reach the Settings screen and fix it.
    let mut effective = settings.clone();
    effective.remote_enabled = false;
    if let Err(e) = listeners.apply(&state, &effective).await {
        tracing::error!("{e}. Falling back to 0.0.0.0:8080 so you can correct it in Settings");
        effective.bind_addr = "0.0.0.0:8080".into();
        listeners.apply(&state, &effective).await.map_err(|e| anyhow!("cannot start the intranet listener: {e}"))?;
    }
    if settings.remote_enabled {
        effective.remote_enabled = true;
        effective.remote_bind_addr = settings.remote_bind_addr.clone();
        if let Err(e) = listeners.apply(&state, &effective).await {
            tracing::error!("The remote (view-only) listener could not start: {e}");
        }
    } else {
        tracing::warn!("The remote listener is disabled: there is no internet-facing access");
    }

    // 9. Run until asked to stop, then let the listeners finish their requests.
    let _ = stop_wait.wait_for(|stopping| *stopping).await;
    listeners.shutdown().await;
    tracing::info!("Shut down cleanly");
    Ok(())
}

/// Waits (up to ~30 s) for Orthanc and reports what it finds in the log.
async fn check_orthanc(orthanc: OrthancClient) {
    let mut last_error = String::new();
    for attempt in 1..=15 {
        match orthanc.info().await {
            Ok(info) if info.supported => {
                tracing::info!("Connected to Orthanc {}", info.version);
                return;
            }
            Ok(info) => {
                tracing::error!(
                    "Orthanc {} is not supported: this portal needs Orthanc {}.{}.{} or newer with the \
                     'HasExtendedFind' capability. Searches will fail until Orthanc is upgraded.",
                    info.version,
                    MIN_VERSION.0,
                    MIN_VERSION.1,
                    MIN_VERSION.2
                );
                return;
            }
            Err(e) => {
                last_error = e.to_string();
                if attempt < 15 {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }
    tracing::warn!("Orthanc is not reachable ({last_error}). Check Settings > Orthanc connection.");
}

/// Completes when the process is asked to stop (Ctrl+C or SIGTERM).
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
