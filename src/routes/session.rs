//! # routes/session.rs
//!
//! Login / logout / "who am I" endpoints.
//!
//! * `POST /api/login`  body `{ "username": "...", "password": "..." }`
//! * `POST /api/logout`
//! * `GET  /api/me`     -> the logged-in user, their permissions and the available
//!   features. The web UI uses it to decide which tabs and buttons to show.

use crate::auth::{self, AuthUser, SESSION_COOKIE};
use crate::db::User;
use crate::error::{AppError, AppResult};
use crate::permissions::Permission;
use crate::settings::{CardSize, ModalityButton, RecentLayout, RecentOrder};
use crate::state::AppState;
use axum::{
    extract::{ConnectInfo, State},
    http::{header, HeaderMap, HeaderValue},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

/// What the "Share..." dialog needs to know about sharing.
#[derive(Serialize)]
pub struct ShareFeature {
    /// The public address for share links is set (Settings > Network): without it nothing can be shared.
    configured: bool,
    /// Suggested validity (hours) and downloads of a new link, and the limits an administrator allows.
    default_hours: u64,
    max_hours: u64,
    default_downloads: u64,
    max_downloads: u64,
    /// A mail server is set up: the Share windows offer to send the invitation by email.
    email: bool,
}

/// Options of the simplified view (Settings > Appearance).
#[derive(Serialize)]
pub struct SimplifiedFeature {
    recent_count: u64,
    recent_order: RecentOrder,
    recent_layout: RecentLayout,
    card_size: CardSize,
    show_total: bool,
    show_labels: bool,
    recent_title: String,
    upload_text: String,
    recent_auto_refresh: bool,
    transfers_auto_refresh: bool,
    refresh_seconds: u64,
}

/// Settings > Search & labels, as the pages need them.
#[derive(Serialize)]
pub struct SearchFeature {
    columns: Vec<String>,
    fields: Vec<String>,
    live: bool,
    min_chars: u64,
    infinite: bool,
    /// Labels that may be put on studies (empty = any).
    allowed_labels: Vec<String>,
    label_counts: bool,
    /// This user may put labels on studies.
    can_edit_labels: bool,
}

/// Optional capabilities of the installation, shown or hidden by the UI.
#[derive(Serialize)]
pub struct Features {
    /// Offer the Weasis desktop viewer (Settings > General > Viewers and uploads).
    weasis: bool,
    /// Orthanc's OHIF plugin is available: offer the browser viewer.
    ohif: bool,
    /// Preview thumbnails are switched on (Settings > Viewers and uploads).
    thumbnails: bool,
    /// The quick modality filters of the search pages, in order (Settings > General).
    modality_buttons: Vec<ModalityButton>,
    /// Sharing with people outside (invitation links).
    share: ShareFeature,
    /// Options of the home page and the icons (Settings > Appearance).
    simplified: SimplifiedFeature,
    /// Columns, search boxes, live search, endless lists and labels (Settings > Search & labels).
    search: SearchFeature,
    /// The optional features of Settings > Features, as far as this user may use them.
    options: serde_json::Value,
    /// The other viewers (Stone, VolView, WSI, MedDream) and the OHIF modes offered.
    viewers: serde_json::Value,
    /// How dates and names are written (Settings > Appearance).
    format: serde_json::Value,
}

/// Answer of `/api/login` and `/api/me`: the user fields (id, username,
/// permissions...) plus the features.
#[derive(Serialize)]
pub struct Me {
    #[serde(flatten)]
    user: User,
    features: Features,
}

async fn me_response(state: &AppState, user: User) -> Me {
    let settings = state.settings.get();
    let orthanc = &state.orthanc;
    let (ohif, stone, volview, wsi) = tokio::join!(
        orthanc.ohif_available(settings.ohif),
        orthanc.viewer_available(settings.stone_viewer, "stone"),
        orthanc.viewer_available(settings.volview_viewer, "volview"),
        orthanc.viewer_available(settings.wsi_viewer, "wsi"),
    );
    let may = |p: Permission| user.may(p);
    let is_admin = user.permissions.contains(Permission::Admin);
    let options = serde_json::json!({
        "labels": settings.labels_enabled,
        "detail_fields": settings.study_detail_fields,
        "bulk": settings.bulk_actions,
        "instances": settings.instance_browser,
        "single_file": settings.single_file_download,
        "nifti": settings.nifti_export,
        "delete": settings.allow_delete && may(Permission::Delete),
        "modify": settings.allow_modify && may(Permission::Edit),
        "anonymize": settings.allow_anonymize && may(Permission::Edit),
        "add_series": settings.allow_add_series && may(Permission::Edit),
        "multi_share": settings.multi_study_shares,
        "retrieve": settings.query_retrieve && may(Permission::Retrieve),
        "worklists": settings.worklists_enabled && may(Permission::Worklists),
        "jobs_page": settings.jobs_page && is_admin,
        "system_page": settings.system_page && is_admin,
        "raw_api": settings.raw_api && is_admin,
    });
    let viewers = serde_json::json!({
        "stone": stone,
        "volview": volview,
        "wsi": wsi,
        "meddream_url": settings.meddream_url,
        "ohif_modes": settings.ohif_modes,
    });
    let format = serde_json::json!({ "date": settings.date_format, "name": settings.name_format });
    let share = ShareFeature {
        configured: !settings.share_public_url.is_empty(),
        default_hours: settings.share_default_hours,
        max_hours: settings.share_max_hours,
        default_downloads: settings.share_default_downloads,
        max_downloads: settings.share_max_downloads,
        email: settings.email_configured(),
    };
    let search = SearchFeature {
        columns: settings.study_columns.clone(),
        fields: settings.study_search_fields.clone(),
        live: settings.search_as_you_type,
        min_chars: settings.search_min_chars,
        infinite: settings.infinite_scroll,
        allowed_labels: settings.available_labels.clone(),
        label_counts: settings.label_counts,
        can_edit_labels: crate::routes::labels::can_edit_labels(&user),
    };
    let simplified = SimplifiedFeature {
        recent_count: settings.simplified_recent_count,
        recent_order: settings.simplified_recent_order,
        recent_layout: settings.simplified_recent_layout,
        card_size: settings.simplified_card_size,
        show_total: settings.simplified_show_total,
        show_labels: settings.simplified_show_labels,
        recent_title: settings.simplified_recent_title.clone(),
        upload_text: settings.simplified_upload_text.clone(),
        recent_auto_refresh: settings.recent_auto_refresh,
        transfers_auto_refresh: settings.transfers_auto_refresh,
        refresh_seconds: settings.auto_refresh_seconds,
    };
    Me {
        user,
        features: Features {
            weasis: settings.weasis_enabled,
            ohif,
            thumbnails: settings.thumbnails_enabled,
            modality_buttons: settings.visible_modality_buttons(),
            share,
            // Only meaningful while the simplified view is switched on.
            simplified,
            search,
            options,
            viewers,
            format,
        },
    }
}

#[derive(Deserialize)]
pub struct LoginRequest {
    username: String,
    password: String,
}

/// Verifies the credentials and, on success, starts a session (cookie).
pub async fn login(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<LoginRequest>,
) -> AppResult<Response> {
    let username = req.username.trim().to_string();
    let settings = state.settings.get();
    let ip = auth::client_ip(&headers, peer);
    let ip_key = auth::ip_key(&ip);
    // One address may fail more often than one account (an office shares an address),
    // but not without limit: that would allow trying thousands of different names.
    let ip_limit = settings.login_max_failures.saturating_mul(4);

    // 0. Refuse absurd input at once: nothing is hashed, stored or logged for it.
    if !auth::plausible_username(&username) || req.password.chars().count() > auth::MAX_PASSWORD_LEN {
        state.throttle.record_failure(&ip_key, ip_limit, settings.lockout_secs());
        return Err(AppError::BadRequest("Wrong username or password".into()));
    }

    // 1. Refuse early if this address or this account is temporarily locked.
    state.throttle.check(&ip_key)?;
    state.throttle.check(&username)?;

    // 2. Verify the password. For an unknown user a dummy hash is checked, so
    //    the answer takes the same time and reveals nothing.
    let user = state.db.get_user_by_name(&username)?;
    let hash = user.as_ref().map(|u| u.password_hash.clone());
    let password_ok = auth::verify_password(hash, req.password).await;

    let user: User = match user {
        Some(u) if password_ok && u.active => u,
        _ => {
            state.throttle.record_failure(&username, settings.login_max_failures, settings.lockout_secs());
            state.throttle.record_failure(&ip_key, ip_limit, settings.lockout_secs());
            state.db.audit(&username, "login_failed", &format!("ip={ip}"));
            // One generic message for every failure reason.
            return Err(AppError::BadRequest("Wrong username or password".into()));
        }
    };

    // 3. Success: create the session and send the cookie.
    state.throttle.record_success(&username);
    state.db.touch_login(user.id)?;
    state.db.audit(&user.username, "login", &format!("ip={ip}"));

    let ttl = settings.session_ttl_secs();
    let token = state.sessions.issue(user.id, ttl as i64, auth::MAX_SESSIONS_PER_USER);
    let secure = auth::cookie_secure(&settings, &headers);
    let cookie = auth::session_cookie(&token, ttl, secure);

    let mut response = Json(me_response(&state, user).await).into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, HeaderValue::from_str(&cookie).map_err(|e| AppError::Internal(e.to_string()))?);
    Ok(response)
}

/// Ends the session: forgets the token server-side and clears the cookie.
pub async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
    AuthUser(user): AuthUser,
) -> AppResult<Response> {
    if let Some(token) = auth::read_cookie(&headers, SESSION_COOKIE) {
        state.sessions.revoke(&token);
    }
    state.db.audit(&user.username, "logout", "");

    let secure = auth::cookie_secure(&state.settings.get(), &headers);
    let mut response = Json(serde_json::json!({ "ok": true })).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&auth::clear_session_cookie(secure)).map_err(|e| AppError::Internal(e.to_string()))?,
    );
    Ok(response)
}

/// Returns the current user (401 when not logged in -> the UI shows the login form).
pub async fn me(State(state): State<AppState>, AuthUser(user): AuthUser) -> Json<Me> {
    Json(me_response(&state, user).await)
}
