//! # routes/admin.rs
//!
//! User management and audit trail. Every handler requires the `admin` permission.
//!
//! * `GET    /api/admin/users`       list users + available permissions + role presets
//! * `POST   /api/admin/users`       create a user
//! * `PUT    /api/admin/users/{id}`  change permissions / enable-disable / reset password
//! * `DELETE /api/admin/users/{id}`  delete a user
//! * `GET    /api/admin/audit`       latest audit entries
//!
//! Safety rules enforced here:
//!   * an administrator cannot disable, delete or demote **themself**;
//!   * the last active administrator can never be removed.

use crate::auth::{self, AuthUser};
use crate::db::User;
use crate::error::{AppError, AppResult};
use crate::mailer;
use crate::permissions::{Permission, PermissionSet};
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

/// Usernames: 3-64 characters, letters, digits and `. _ @ -`.
fn validate_username(name: &str) -> AppResult<()> {
    let ok = (3..=64).contains(&name.chars().count())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '@' | '-'));
    if ok {
        Ok(())
    } else {
        Err(AppError::BadRequest("The username must have 3-64 characters: letters, digits and . _ @ -".into()))
    }
}

/// A role key sent by the user form: empty (custom permissions) or one of the roles of
/// Settings > Security & roles.
fn validate_role(state: &AppState, role: &str) -> AppResult<String> {
    let role = role.trim();
    if role.is_empty() || state.settings.get().roles.iter().any(|r| r.key == role) {
        Ok(role.to_string())
    } else {
        Err(AppError::BadRequest(format!("Unknown role '{role}'")))
    }
}

/// Lists all users together with the metadata the admin form needs.
pub async fn list_users(State(state): State<AppState>, AuthUser(admin): AuthUser) -> AppResult<Json<Value>> {
    admin.require(Permission::Admin)?;
    let permissions: Vec<Value> =
        Permission::ALL.iter().map(|p| json!({ "key": p.as_str(), "description": p.description() })).collect();
    // Each user with the role it is treated as and the view that gives (Settings > Appearance).
    let settings = state.settings.get();
    let users: Vec<Value> = state
        .db
        .list_users()?
        .into_iter()
        .map(|u| {
            let role = settings.role_of(&u.role, &u.permissions);
            let mut value = serde_json::to_value(&u).unwrap_or_default();
            value["effective_role"] = json!(role.map(|r| &r.key));
            value
        })
        .collect();
    Ok(Json(json!({
        "users": users,
        "permissions": permissions,
        // Roles are a setting: edited in Settings > Roles.
        "presets": settings.roles,
    })))
}

#[derive(Deserialize)]
pub struct CreateUser {
    username: String,
    password: String,
    /// Permission names, e.g. `["read", "visualize"]`.
    permissions: Vec<String>,
    /// Key of the role picked in the form (empty or absent = custom).
    #[serde(default)]
    role: String,
    /// Where the portal writes to this person (optional).
    #[serde(default)]
    email: String,
}

pub async fn create_user(
    State(state): State<AppState>,
    AuthUser(admin): AuthUser,
    Json(req): Json<CreateUser>,
) -> AppResult<Json<User>> {
    admin.require(Permission::Admin)?;
    let _one_at_a_time = state.admin_lock.lock().await;
    let username = req.username.trim().to_string();
    validate_username(&username)?;
    auth::validate_password(&req.password)?;
    let permissions = PermissionSet::parse_list(&req.permissions)?;
    let role = validate_role(&state, &req.role)?;
    let email = mailer::single_address(&req.email).map_err(AppError::BadRequest)?;

    let hash = auth::hash_password(req.password).await?;
    let mut user = state.db.create_user(&username, &hash, &permissions, &role)?;
    if !email.is_empty() {
        state.db.set_user_email(user.id, &email)?;
        user.email = email;
    }
    state.db.audit(
        &admin.username,
        "user_created",
        &format!(
            "{} [{}] role={}",
            user.username,
            permissions.to_db_string(),
            if role.is_empty() { "custom" } else { &role }
        ),
    );
    Ok(Json(user))
}

/// Every field is optional: only the ones present are changed.
#[derive(Deserialize)]
pub struct UpdateUser {
    permissions: Option<Vec<String>>,
    active: Option<bool>,
    /// New password (reset). Leave out to keep the current one.
    password: Option<String>,
    /// Role key ("" = custom). Leave out to keep the current one.
    role: Option<String>,
    /// Email address ("" = none). Leave out to keep it.
    email: Option<String>,
    /// Forget the account's two-step sign-in (a lost phone): it signs in with the password alone,
    /// or sets it up again at the next sign-in when it is required.
    #[serde(default)]
    reset_two_factor: bool,
}

pub async fn update_user(
    State(state): State<AppState>,
    AuthUser(admin): AuthUser,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(req): Json<UpdateUser>,
) -> AppResult<Json<User>> {
    admin.require(Permission::Admin)?;
    // Serialised: the "is another administrator left?" check and the change must be one step.
    let _one_at_a_time = state.admin_lock.lock().await;
    let target = state.db.get_user(id)?.ok_or(AppError::NotFound)?;

    let new_permissions = req.permissions.as_deref().map(PermissionSet::parse_list).transpose()?;
    let role = req.role.as_deref().map(|r| validate_role(&state, r)).transpose()?;
    let password = req.password.filter(|p| !p.is_empty());
    if let Some(p) = &password {
        auth::validate_password(p)?;
    }

    // Lock-out protection.
    let final_permissions = new_permissions.as_ref().unwrap_or(&target.permissions);
    let final_active = req.active.unwrap_or(target.active);
    let keeps_admin_rights = final_active && final_permissions.contains(Permission::Admin);
    let is_active_admin = target.active && target.permissions.contains(Permission::Admin);
    if is_active_admin && !keeps_admin_rights {
        if target.id == admin.id {
            return Err(AppError::Conflict(
                "You cannot disable or remove the administrator rights of your own account".into(),
            ));
        }
        if state.db.count_active_admins()? <= 1 {
            return Err(AppError::Conflict("At least one active administrator is required".into()));
        }
    }

    let email = req.email.as_deref().map(mailer::single_address).transpose().map_err(AppError::BadRequest)?;

    let hash = match password {
        Some(p) => Some(auth::hash_password(p).await?),
        None => None,
    };
    state.db.update_user(id, new_permissions.as_ref(), req.active, hash.as_deref(), role.as_deref())?;
    if let Some(email) = &email {
        state.db.set_user_email(id, email)?;
    }
    if req.reset_two_factor && (target.two_factor || target.totp_pending.is_some()) {
        state.db.disable_totp(id)?;
        state.db.audit(&admin.username, "two_factor_reset", &target.username);
    }

    // A new password (or a disabled account) ends every session and Weasis link of that user.
    // An administrator changing their OWN password keeps only the session they are using now:
    // if the old password had leaked, whoever used it is signed out too.
    if hash.is_some() || req.active == Some(false) {
        if target.id == admin.id {
            let current = auth::read_cookie(&headers, auth::SESSION_COOKIE).unwrap_or_default();
            state.sessions.revoke_user_except(id, &current);
        } else {
            state.sessions.revoke_user(id);
        }
        state.weasis_tokens.revoke_user(id);
    }

    let updated = state.db.get_user(id)?.ok_or(AppError::NotFound)?;
    state.db.audit(
        &admin.username,
        "user_updated",
        &format!(
            "{} [{}] role={} active={}{}",
            updated.username,
            updated.permissions.to_db_string(),
            if updated.role.is_empty() { "custom" } else { &updated.role },
            updated.active,
            if hash.is_some() { " password reset" } else { "" }
        ),
    );
    Ok(Json(updated))
}

pub async fn delete_user(
    State(state): State<AppState>,
    AuthUser(admin): AuthUser,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    admin.require(Permission::Admin)?;
    let _one_at_a_time = state.admin_lock.lock().await;
    let target = state.db.get_user(id)?.ok_or(AppError::NotFound)?;
    if target.id == admin.id {
        return Err(AppError::Conflict("You cannot delete your own account".into()));
    }
    if target.active && target.permissions.contains(Permission::Admin) && state.db.count_active_admins()? <= 1 {
        return Err(AppError::Conflict("At least one active administrator is required".into()));
    }

    state.db.delete_user(id)?;
    state.sessions.revoke_user(id);
    state.weasis_tokens.revoke_user(id);
    state.db.audit(&admin.username, "user_deleted", &target.username);
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct AuditParams {
    limit: Option<i64>,
}

pub async fn audit_log(
    State(state): State<AppState>,
    AuthUser(admin): AuthUser,
    Query(params): Query<AuditParams>,
) -> AppResult<Json<Value>> {
    admin.require(Permission::Admin)?;
    let limit = params.limit.unwrap_or(200).clamp(1, 1000);
    Ok(Json(json!({ "entries": state.db.recent_audit(limit)? })))
}
