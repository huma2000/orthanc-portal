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
use crate::permissions::{Permission, PermissionSet};
use crate::state::AppState;
use axum::extract::{Path, Query, State};
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

/// Lists all users together with the metadata the admin form needs.
pub async fn list_users(State(state): State<AppState>, AuthUser(admin): AuthUser) -> AppResult<Json<Value>> {
    admin.require(Permission::Admin)?;
    let permissions: Vec<Value> =
        Permission::ALL.iter().map(|p| json!({ "key": p.as_str(), "description": p.description() })).collect();
    Ok(Json(json!({
        "users": state.db.list_users()?,
        "permissions": permissions,
        // Roles are a setting: edited in Settings > Roles.
        "presets": state.settings.get().roles,
    })))
}

#[derive(Deserialize)]
pub struct CreateUser {
    username: String,
    password: String,
    /// Permission names, e.g. `["read", "visualize"]`.
    permissions: Vec<String>,
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

    let hash = auth::hash_password(req.password).await?;
    let user = state.db.create_user(&username, &hash, &permissions)?;
    state.db.audit(&admin.username, "user_created", &format!("{} [{}]", user.username, permissions.to_db_string()));
    Ok(Json(user))
}

/// Every field is optional: only the ones present are changed.
#[derive(Deserialize)]
pub struct UpdateUser {
    permissions: Option<Vec<String>>,
    active: Option<bool>,
    /// New password (reset). Leave out to keep the current one.
    password: Option<String>,
}

pub async fn update_user(
    State(state): State<AppState>,
    AuthUser(admin): AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<UpdateUser>,
) -> AppResult<Json<User>> {
    admin.require(Permission::Admin)?;
    // Serialised: the "is another administrator left?" check and the change must be one step.
    let _one_at_a_time = state.admin_lock.lock().await;
    let target = state.db.get_user(id)?.ok_or(AppError::NotFound)?;

    let new_permissions = req.permissions.as_deref().map(PermissionSet::parse_list).transpose()?;
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

    let hash = match password {
        Some(p) => Some(auth::hash_password(p).await?),
        None => None,
    };
    state.db.update_user(id, new_permissions.as_ref(), req.active, hash.as_deref())?;

    // Kick the user out when the account is disabled or the password changed
    // (but do not log the administrator out of their own session).
    if target.id != admin.id && (hash.is_some() || req.active == Some(false)) {
        state.sessions.revoke_user(id);
        state.weasis_tokens.revoke_user(id);
    }

    let updated = state.db.get_user(id)?.ok_or(AppError::NotFound)?;
    state.db.audit(
        &admin.username,
        "user_updated",
        &format!(
            "{} [{}] active={}{}",
            updated.username,
            updated.permissions.to_db_string(),
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
