//! # routes/session.rs
//!
//! Login / logout / "who am I" endpoints.
//!
//! * `POST /api/login`  body `{ "username": "...", "password": "..." }`
//! * `POST /api/logout`
//! * `GET  /api/me`     -> the logged-in user, **effective** permissions, access
//!   scope and available features. The web UI uses it to decide which tabs and
//!   buttons to show.
//!
//! "Effective" permissions are the user's own permissions narrowed to what the
//! access scope allows: on the internet (remote scope) only `read` and
//! `visualize` ever appear, even for an administrator.

use crate::auth::{self, AuthUser, SESSION_COOKIE};
use crate::db::User;
use crate::error::{AppError, AppResult};
use crate::permissions::AccessScope;
use crate::state::AppState;
use axum::{
    extract::{ConnectInfo, State},
    http::{header, HeaderMap, HeaderValue},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

/// Optional capabilities of the installation, shown or hidden by the UI.
#[derive(Serialize)]
pub struct Features {
    /// Orthanc's OHIF plugin is available: offer "View in browser".
    ohif: bool,
    /// Preview thumbnails are switched on (Settings > Viewers and uploads).
    thumbnails: bool,
    /// Weasis may be used: on the intranet only. Over the internet images can only be
    /// looked at in the browser viewer, because Weasis would download them.
    weasis: bool,
}

/// Answer of `/api/login` and `/api/me`: the user fields (id, username,
/// permissions, scope...) plus the features.
#[derive(Serialize)]
pub struct Me {
    #[serde(flatten)]
    user: User,
    features: Features,
}

async fn me_response(state: &AppState, user: User) -> Me {
    let settings = state.settings.get();
    let ohif = state.orthanc.ohif_available(settings.ohif).await;
    let weasis = user.scope == AccessScope::Full;
    Me { user, features: Features { ohif, thumbnails: settings.thumbnails_enabled, weasis } }
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
    let scope = auth::effective_scope(&state, &headers);
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
            state.db.audit(&username, "login_failed", &format!("ip={ip} scope={scope:?}"));
            // One generic message for every failure reason.
            return Err(AppError::BadRequest("Wrong username or password".into()));
        }
    };

    // 3. Success: create the session and send the cookie.
    state.throttle.record_success(&username);
    state.db.touch_login(user.id)?;
    state.db.audit(&user.username, "login", &format!("ip={ip} scope={scope:?}"));

    let ttl = settings.session_ttl_secs();
    let token = state.sessions.issue(user.id, ttl as i64, auth::MAX_SESSIONS_PER_USER);
    let secure = auth::cookie_secure(&settings, &headers);
    let cookie = auth::session_cookie(&token, ttl, secure);

    let mut response = Json(me_response(&state, user.with_scope(scope)).await).into_response();
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
