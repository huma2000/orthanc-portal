//! # server.rs
//!
//! Runs the two HTTP listeners and can **re-open them on other addresses while
//! the program is running**, which is what lets an administrator change the
//! ports from the Settings screen without restarting anything.
//!
//! * the **intranet** listener (full features), and
//! * the optional **remote** listener (view only, for the Cloudflare Tunnel).
//!
//! Changing an address is done carefully:
//!   1. the new address is opened FIRST; if that fails nothing changes and the
//!      administrator gets the reason (port in use, invalid address...);
//!   2. only then is the old listener stopped. Connections already open (such as
//!      the one that carried the "Save" request) are not cut;
//!   3. if the second listener cannot be changed, the first one is put back.

use crate::error::AppError;
use crate::permissions::AccessScope;
use crate::routes;
use crate::settings::Settings;
use crate::state::AppState;
use serde::Serialize;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{watch, Mutex};
use tokio::task::JoinHandle;

/// One running listener: the address it uses and the task serving requests.
struct Running {
    addr: String,
    task: JoinHandle<()>,
}

/// The two possible listeners.
#[derive(Default)]
struct Slots {
    full: Option<Running>,
    remote: Option<Running>,
}

/// What the Settings screen shows about a listener.
#[derive(Serialize)]
pub struct ListenerStatus {
    pub name: &'static str,
    pub addr: Option<String>,
    pub running: bool,
}

/// Cheap-to-clone handle that owns the listeners.
#[derive(Clone)]
pub struct ListenerManager {
    slots: Arc<Mutex<Slots>>,
    /// Becomes `true` when the whole program must stop (Ctrl+C / SIGTERM).
    stop: watch::Receiver<bool>,
}

/// Stops accepting connections on a listener and waits until its port is free.
async fn stop_listener(running: Running) {
    running.task.abort();
    let _ = running.task.await; // returns "cancelled" once the listener is dropped
}

impl ListenerManager {
    pub fn new(stop: watch::Receiver<bool>) -> Self {
        Self { slots: Arc::new(Mutex::new(Slots::default())), stop }
    }

    /// Starts serving `listener` with the router of the given scope.
    fn spawn(&self, listener: TcpListener, addr: String, scope: AccessScope, state: &AppState) -> Running {
        let app = routes::router(state.clone(), scope);
        let mut stop = self.stop.clone();
        let log_addr = addr.clone();
        let task = tokio::spawn(async move {
            tracing::info!("Listening on http://{log_addr}  [{scope:?}]");
            // `ConnectInfo` gives handlers the TCP peer address (used in the audit log).
            let served = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
                .with_graceful_shutdown(async move {
                    let _ = stop.wait_for(|stopping| *stopping).await.is_ok();
                })
                .await;
            if let Err(e) = served {
                tracing::error!("Listener {log_addr} stopped with an error: {e}");
            }
        });
        Running { addr, task }
    }

    /// Makes one listener match the wanted address (`None` = it must not run).
    /// Returns a sentence describing what changed, or `None` if nothing did.
    async fn ensure(
        &self,
        slot: &mut Option<Running>,
        scope: AccessScope,
        wanted: Option<&str>,
        state: &AppState,
    ) -> Result<Option<String>, AppError> {
        if slot.as_ref().map(|r| r.addr.as_str()) == wanted {
            return Ok(None);
        }
        let label = match scope {
            AccessScope::Full => "The intranet listener",
            AccessScope::Remote => "The remote (view-only) listener",
        };

        // Nothing wanted: just stop it.
        let Some(addr) = wanted else {
            if let Some(old) = slot.take() {
                stop_listener(old).await;
            }
            return Ok(Some(format!("{label} was stopped.")));
        };

        // Open the new address FIRST.
        let listener = match TcpListener::bind(addr).await {
            Ok(listener) => listener,
            Err(e) => match slot.take() {
                // The address may be held by our own current listener (same port, different
                // IP): release it and try once more, restoring it if that fails too.
                Some(old) if e.kind() == ErrorKind::AddrInUse => {
                    let old_addr = old.addr.clone();
                    stop_listener(old).await;
                    match TcpListener::bind(addr).await {
                        Ok(listener) => listener,
                        Err(e2) => {
                            if let Ok(previous) = TcpListener::bind(&old_addr).await {
                                *slot = Some(self.spawn(previous, old_addr, scope, state));
                            }
                            return Err(AppError::BadRequest(format!(
                                "Cannot listen on {addr}: {e2}. The previous address was kept."
                            )));
                        }
                    }
                }
                // Any other failure: whatever was running keeps running.
                previous => {
                    *slot = previous;
                    return Err(AppError::BadRequest(format!("Cannot listen on {addr}: {e}")));
                }
            },
        };

        let running = self.spawn(listener, addr.to_string(), scope, state);
        if let Some(old) = slot.replace(running) {
            stop_listener(old).await;
        }
        Ok(Some(format!("{label} now listens on {addr}.")))
    }

    /// Brings the listeners in line with the settings. On failure nothing is left
    /// half-changed: the intranet listener is restored if the remote one fails.
    pub async fn apply(&self, state: &AppState, settings: &Settings) -> Result<Vec<String>, AppError> {
        let mut slots = self.slots.lock().await;
        let wanted_full = settings.bind_addr.clone();
        let wanted_remote = settings.remote_enabled.then(|| settings.remote_bind_addr.clone());
        let previous_full = slots.full.as_ref().map(|r| r.addr.clone());

        let mut messages = Vec::new();
        if let Some(m) = self.ensure(&mut slots.full, AccessScope::Full, Some(&wanted_full), state).await? {
            messages.push(m);
        }
        match self.ensure(&mut slots.remote, AccessScope::Remote, wanted_remote.as_deref(), state).await {
            Ok(Some(m)) => messages.push(m),
            Ok(None) => {}
            Err(e) => {
                // Put the intranet listener back where it was.
                if slots.full.as_ref().map(|r| r.addr.clone()) != previous_full {
                    let _ = self.ensure(&mut slots.full, AccessScope::Full, previous_full.as_deref(), state).await;
                }
                return Err(e);
            }
        }
        Ok(messages)
    }

    /// Current state of both listeners, for the Settings screen.
    pub async fn status(&self) -> Vec<ListenerStatus> {
        let slots = self.slots.lock().await;
        let describe = |name: &'static str, slot: &Option<Running>| ListenerStatus {
            name,
            addr: slot.as_ref().map(|r| r.addr.clone()),
            running: slot.as_ref().map(|r| !r.task.is_finished()).unwrap_or(false),
        };
        vec![describe("Intranet (full access)", &slots.full), describe("Remote (view only)", &slots.remote)]
    }

    /// Waits (up to 10 s each) for the listeners to finish after the stop signal.
    pub async fn shutdown(&self) {
        let mut slots = self.slots.lock().await;
        for running in [slots.full.take(), slots.remote.take()].into_iter().flatten() {
            let _ = tokio::time::timeout(Duration::from_secs(10), running.task).await;
        }
    }
}
