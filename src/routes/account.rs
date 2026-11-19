//! # routes/account.rs
//!
//! **My account**: what every signed-in person may change about their OWN account. Intranet only.
//!
//! * `PUT  /api/account/email`                 `{ email }` where the portal writes to me ("" = nowhere)
//! * `POST /api/account/password`              `{ current, new }` change my password
//! * `POST /api/account/two-factor/setup`      `{ password }` -> secret + QR code (not active yet)
//! * `POST /api/account/two-factor/enable`     `{ code }` the first code confirms it -> recovery codes
//! * `POST /api/account/two-factor/disable`    `{ password }` (refused when it is required)
//! * `POST /api/account/two-factor/recovery`   `{ password }` -> ten new recovery codes
//!
//! Two-step sign-in exists only when an administrator switched it on (Settings > Security & roles):
//! "optional" lets each person choose here, "required" makes everybody set it up at their next
//! sign-in. Every change that weakens or replaces it asks for the password again, so a session left
//! open on a shared computer is not enough to take over the account.

use crate::auth::{self, AuthUser};
use crate::db::User;
use crate::error::{AppError, AppResult};
use crate::mailer;
use crate::routes::session::setup_material;
use crate::settings::TwoFactorMode;
use crate::state::AppState;
use crate::totp;
use crate::util::now_unix;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

/// Checks the password typed again; failures count like failed sign-ins.
async fn confirm_password(state: &AppState, user: &User, password: String) -> AppResult<()> {
    let settings = state.settings.get();
    state.throttle.check(&user.username)?;
    if password.chars().count() > auth::MAX_PASSWORD_LEN
        || !auth::verify_password(Some(user.password_hash.clone()), password).await
    {
        state.throttle.record_failure(&user.username, settings.login_max_failures, settings.lockout_secs());
        return Err(AppError::BadRequest("Your current password is not correct".into()));
    }
    Ok(())
}

fn two_factor_available(state: &AppState) -> AppResult<TwoFactorMode> {
    match state.settings.get().two_factor {
        TwoFactorMode::Off => Err(AppError::Denied(
            "Two-step sign-in is not used in this portal (an administrator can switch it on in Settings > Security & roles)"
                .into(),
        )),
        mode => Ok(mode),
    }
}

/// Changing one's own email and password (Settings > Security & roles; an administrator can always, in Users).
fn self_service(state: &AppState) -> AppResult<()> {
    crate::util::feature(
        state.settings.get().account_self_service,
        "Changing your own password and email is switched off: ask an administrator",
    )
}

#[derive(Deserialize)]
pub struct EmailRequest {
    email: String,
}

/// Where the portal writes to me (notifications of my share and upload links).
pub async fn set_email(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(req): Json<EmailRequest>,
) -> AppResult<Json<Value>> {
    self_service(&state)?;
    let email = mailer::single_address(&req.email).map_err(AppError::BadRequest)?;
    state.db.set_user_email(user.id, &email)?;
    state.db.audit(&user.username, "account_email", if email.is_empty() { "removed" } else { &email });
    Ok(Json(json!({ "email": email })))
}

#[derive(Deserialize)]
pub struct PasswordChange {
    current: String,
    new: String,
}

/// Changes my own password. My other sessions (and Weasis links) end; this one stays.
pub async fn change_password(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    headers: HeaderMap,
    Json(req): Json<PasswordChange>,
) -> AppResult<Json<Value>> {
    self_service(&state)?;
    confirm_password(&state, &user, req.current).await?;
    auth::validate_password(&req.new)?;
    let hash = auth::hash_password(req.new).await?;
    state.db.update_user(user.id, None, None, Some(&hash), None)?;
    let current = auth::read_cookie(&headers, auth::SESSION_COOKIE).unwrap_or_default();
    state.sessions.revoke_user_except(user.id, &current);
    state.weasis_tokens.revoke_user(user.id);
    state.db.audit(&user.username, "password_changed", "own account");
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct PasswordOnly {
    password: String,
}

/// Starts setting up two-step sign-in: a new secret, shown as a QR code. Nothing changes until
/// the first code from the app is typed (enable).
pub async fn setup_two_factor(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(req): Json<PasswordOnly>,
) -> AppResult<Json<Value>> {
    two_factor_available(&state)?;
    confirm_password(&state, &user, req.password).await?;
    let secret = totp::new_secret();
    state.db.set_totp_pending(user.id, Some(&secret))?;
    Ok(Json(setup_material(&state.settings.get().portal_name, &user.username, &secret)))
}

#[derive(Deserialize)]
pub struct CodeOnly {
    code: String,
}

/// The first code of the app confirms the set-up: two-step sign-in is on from now. The answer holds
/// the recovery codes, shown only now.
pub async fn enable_two_factor(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(req): Json<CodeOnly>,
) -> AppResult<Json<Value>> {
    two_factor_available(&state)?;
    let settings = state.settings.get();
    let key = format!("2fa:{}", user.id);
    state.throttle.check(&key)?;
    let pending = user
        .totp_pending
        .clone()
        .ok_or_else(|| AppError::Conflict("Start the set-up again: no QR code is waiting to be confirmed".into()))?;
    let Some(step) = totp::verify(&pending, &req.code, now_unix()) else {
        state.throttle.record_failure(&key, settings.login_max_failures, settings.lockout_secs());
        return Err(AppError::BadRequest(
            "The code is not correct. Check that the phone's clock is right and type the code the app shows now."
                .into(),
        ));
    };
    state.throttle.record_success(&key);
    let codes = totp::new_recovery_codes();
    let hashes: Vec<String> = codes.iter().map(|c| totp::recovery_hash(c)).collect();
    state.db.enable_totp(user.id, &pending, step, &hashes)?;
    state.db.audit(&user.username, "two_factor_enabled", "own account");
    Ok(Json(json!({ "recovery_codes": codes })))
}

/// Switches two-step sign-in off for my account (not allowed when the portal requires it).
pub async fn disable_two_factor(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(req): Json<PasswordOnly>,
) -> AppResult<Json<Value>> {
    if two_factor_available(&state)? == TwoFactorMode::Required {
        return Err(AppError::Denied("Two-step sign-in is required in this portal: it cannot be switched off".into()));
    }
    confirm_password(&state, &user, req.password).await?;
    state.db.disable_totp(user.id)?;
    state.db.audit(&user.username, "two_factor_disabled", "own account");
    Ok(Json(json!({ "ok": true })))
}

/// Ten new recovery codes (the old ones stop working).
pub async fn new_recovery_codes(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(req): Json<PasswordOnly>,
) -> AppResult<Json<Value>> {
    two_factor_available(&state)?;
    if !user.two_factor {
        return Err(AppError::Conflict("Two-step sign-in is not set up for your account".into()));
    }
    confirm_password(&state, &user, req.password).await?;
    let codes = totp::new_recovery_codes();
    let hashes: Vec<String> = codes.iter().map(|c| totp::recovery_hash(c)).collect();
    state.db.set_recovery_codes(user.id, &hashes)?;
    state.db.audit(&user.username, "recovery_codes_renewed", "own account");
    Ok(Json(json!({ "recovery_codes": codes })))
}
