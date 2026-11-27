//! # state.rs
//!
//! `AppState` bundles everything the request handlers share. axum clones it for
//! each request, so every field is cheap to clone (`Arc` inside).

use crate::auth::{LoginThrottle, TokenStore};
use crate::branding::BrandingStore;
use crate::db::Db;
use crate::orthanc::OrthancClient;
use crate::server::ListenerManager;
use crate::servers::OrthancPool;
use crate::settings::SettingsStore;
use crate::sharing::MemberCache;
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
    /// Every Orthanc server, by key (the default one shares the client above). See `servers.rs`.
    pub orthancs: OrthancPool,
    /// Browser login sessions: cookie token -> user id.
    pub sessions: TokenStore,
    /// Short-lived tokens embedded in `weasis://` links: bearer token -> user id.
    pub weasis_tokens: TokenStore,
    /// Sessions of outside GUESTS who entered the password of a share link:
    /// cookie token -> share id (see `routes/guest.rs`).
    pub share_sessions: TokenStore,
    /// Series and images Orthanc confirmed to belong to a guest's shared study (`sharing::Member`).
    pub guest_members: MemberCache,
    /// Two-step sign-in: the password was right, the code is still to come (ticket -> user id, 5 min).
    pub login_tickets: TokenStore,
    /// Outside people who entered the password of an UPLOAD link: cookie token -> upload link id.
    pub upload_sessions: TokenStore,
    /// Brute-force protection for the login form.
    pub throttle: LoginThrottle,
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
    /// `LISTENERS_FROM_ENV`: the listening addresses are fixed by the environment
    /// (Docker) and the Settings screen must not change them.
    pub listeners_from_env: bool,
}
