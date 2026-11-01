//! # state.rs
//!
//! `AppState` bundles everything the request handlers share. axum clones it for
//! each request, so every field is cheap to clone (`Arc` inside).

use crate::auth::{LoginThrottle, TokenStore};
use crate::branding::BrandingStore;
use crate::db::Db;
use crate::orthanc::OrthancClient;
use crate::permissions::AccessScope;
use crate::server::ListenerManager;
use crate::settings::SettingsStore;
use crate::thumbnails::ThumbnailCache;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    /// The settings saved from the web interface (always read through `get()`,
    /// so a saved change applies to the very next request).
    pub settings: SettingsStore,
    /// Users + audit log (SQLite).
    pub db: Db,
    /// Client for the Orthanc REST API.
    pub orthanc: OrthancClient,
    /// Browser login sessions: cookie token -> user id.
    pub sessions: TokenStore,
    /// Short-lived tokens embedded in `weasis://` links: bearer token -> user id.
    pub weasis_tokens: TokenStore,
    /// Brute-force protection for the login form.
    pub throttle: LoginThrottle,
    /// Which listener this copy of the state serves: `Full` (intranet port) or
    /// `Remote` (the view-only port for the Cloudflare Tunnel). `main.rs` creates
    /// one copy of the state per listener.
    pub listener_scope: AccessScope,
    /// Starts, stops and moves the HTTP listeners (used by the Settings screen).
    pub listeners: ListenerManager,
    /// Memory cache of the generated preview thumbnails.
    pub thumbs: ThumbnailCache,
    /// The logo and website icon uploaded from the Settings screen.
    pub branding: BrandingStore,
    /// Held while an administrator changes settings or users, so two administrators
    /// working at the same moment cannot overwrite each other's changes (or both
    /// demote the last administrators at once).
    pub admin_lock: Arc<tokio::sync::Mutex<()>>,
}
