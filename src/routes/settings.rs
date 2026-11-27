//! # routes/settings.rs
//!
//! Configuration from the web interface. **Every route here requires the `admin`
//! permission and exists on the intranet listener only** (never on the
//! internet-facing one).
//!
//! * `GET  /api/public`                       branding: name, window title, logo, icon, login notice (no login needed)
//! * `GET  /api/admin/settings`               current settings (the Orthanc password is never sent)
//! * `PUT  /api/admin/settings`               save settings; they apply immediately
//! * `POST /api/admin/orthanc/test`           "Test connection" (saved or just-typed values)
//! * `POST /api/admin/email/test`             "Send a test email" (saved mail server settings)
//! * `GET  /api/admin/nodes`                  DICOM nodes ("modalities") known to Orthanc
//! * `PUT  /api/admin/nodes/{alias}`          create / update a node
//! * `DELETE /api/admin/nodes/{alias}`        delete a node
//! * `POST /api/admin/nodes/{alias}/echo`     DICOM ping (C-ECHO) to a node
//!
//! ## What "applies immediately" means
//! * Handlers read `state.settings.get()` on every request, so session length,
//!   lockout rules, upload limit, viewer options... change at once.
//! * A new Orthanc address / credentials are given to the running client.
//! * New listening addresses are opened by `server.rs` (new one first, then the old
//!   one is released); if that fails nothing is saved and the reason is shown.

use crate::auth::AuthUser;
use crate::db::User;
use crate::error::{AppError, AppResult};
use crate::orthanc::{Diagnosis, OrthancClient};
use crate::permissions::{Permission, PermissionSet};
use crate::settings::{default_modality_buttons, default_roles, Settings};
use crate::state::AppState;
use crate::util::now_unix;
use axum::extract::{Path, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

// ------------------------------------------------------------------ public

/// The languages of the interface: code and name (in that language). Each has its translation file
/// in static/i18n/ (English is the text of the code itself).
const LANGUAGES: [[&str; 2]; 2] = [["en", "English"], ["es", "Español"]];

/// Name and notice shown on the login page. Deliberately tiny: nothing sensitive.
pub async fn public_info(State(state): State<AppState>) -> Json<Value> {
    let settings = state.settings.get();
    let branding = state.branding.describe();
    Json(json!({
        "name": settings.portal_name,
        "title": settings.window_title(),
        "show_name": settings.show_portal_name,
        "login_notice": settings.login_notice,
        "logo": branding["logo"],
        "favicon": branding["favicon"],
        // Colours chosen in Settings > Appearance (the login and invitation pages use them too).
        "theme": settings.theme(),
        // How dates and names are written (the invitation page follows it too).
        "format": { "date": settings.date_format, "name": settings.name_format },
        // The colour of each modality code chosen in Settings (the invitation page shows modality tags too).
        "modality_colors": settings
            .modality_buttons
            .iter()
            .filter(|b| !b.color.is_empty())
            .flat_map(|b| {
                b.codes
                    .iter()
                    .flat_map(|c| if c == "RX" { vec!["CR".to_string(), "DX".to_string()] } else { vec![c.clone()] })
                    .map(|c| (c, b.color.clone()))
                    .collect::<Vec<_>>()
            })
            .collect::<std::collections::BTreeMap<String, String>>(),
        // The language of the interface (each browser may choose another one).
        "language": settings.language,
        "languages": LANGUAGES,
    }))
}

// ---------------------------------------------------------------- settings

/// Settings + the state of the listeners + factory roles (for "Restore defaults").
async fn settings_document(state: &AppState) -> Value {
    // The destinations come from Orthanc: asked while the listeners are looked at.
    let (listeners, destinations) =
        tokio::join!(state.listeners.status(), crate::routes::send::destinations(state, &state.orthanc));
    json!({
        "settings": state.settings.get().to_view(),
        "listeners": listeners,
        // true = the listening addresses are set by the environment (Docker): read-only here.
        "listeners_from_env": state.listeners_from_env,
        "defaults": {
            "roles": default_roles(),
            "modality_buttons": default_modality_buttons(),
            "study_columns": Settings::defaults().study_columns,
            "study_search_fields": Settings::defaults().study_search_fields,
            "study_detail_fields": Settings::defaults().study_detail_fields,
        },
        "branding": state.branding.describe(),
        // Settings > Auto-forward: what the rules did lately, and where they can send to.
        "forward": crate::forward::status(),
        "destinations": destinations.unwrap_or_default(),
        // Every permission, for the roles editor.
        "permissions": Permission::ALL
            .iter()
            .map(|p| json!({ "key": p.as_str(), "description": p.description() }))
            .collect::<Vec<_>>(),
    })
}

pub async fn get_settings(State(state): State<AppState>, AuthUser(admin): AuthUser) -> AppResult<Json<Value>> {
    admin.require(Permission::Admin)?;
    Ok(Json(settings_document(&state).await))
}

/// Saves a (partial) update of the settings and applies it to the running program.
pub async fn update_settings(
    State(state): State<AppState>,
    AuthUser(admin): AuthUser,
    Json(patch): Json<Value>,
) -> AppResult<Json<Value>> {
    admin.require(Permission::Admin)?;
    // One administrator at a time: the "read the current settings, change them, save"
    // sequence below must not interleave with another save, or one would silently
    // overwrite the other's changes.
    let _one_at_a_time = state.admin_lock.lock().await;

    // 1. Validate. Nothing has changed yet if this fails.
    let old = state.settings.get();
    let new = old.with_patch(&patch)?;
    if state.listeners_from_env && (new.bind_addr != old.bind_addr || new.remote_bind_addr != old.remote_bind_addr) {
        return Err(AppError::BadRequest(
            "The listening addresses are set by the server's environment (LISTENERS_FROM_ENV, used by Docker). \
             Change INTRANET_PORT / INTERNET_PORT in the .env file next to docker-compose.yml and restart \
             the portal (docker compose up -d)."
                .into(),
        ));
    }
    let changed = old.changed_keys(&new);
    // Users follow their role: work out who gets new permissions (and refuse a lock-out) now.
    let role_changes = if changed.iter().any(|k| k == "roles") {
        role_updates(&old, &new, &state.db.list_users()?, admin.id)?
    } else {
        Vec::new()
    };
    if changed.is_empty() {
        let mut document = settings_document(&state).await;
        document["messages"] = json!(["Nothing was changed."]);
        return Ok(Json(document));
    }

    // 2. Re-open the listeners if an address changed. This is the step that can
    //    fail (port in use...), so it runs BEFORE anything is saved.
    let mut messages: Vec<String> = Vec::new();
    let listeners_changed = old.bind_addr != new.bind_addr
        || old.remote_enabled != new.remote_enabled
        || (new.remote_enabled && old.remote_bind_addr != new.remote_bind_addr);
    if listeners_changed {
        messages.extend(state.listeners.apply(&state, &new).await?);
    }

    // 3. Save. If saving fails, put the listeners back.
    let stored = serde_json::to_string(&new).map_err(|e| AppError::Internal(e.to_string()))?;
    if let Err(e) = state.db.save_settings(&stored) {
        if listeners_changed {
            let _ = state.listeners.apply(&state, &old).await;
        }
        return Err(e);
    }
    // The users of the changed roles, in one transaction. If that fails, put everything back.
    if !role_changes.is_empty() {
        let rows: Vec<(i64, &str, &PermissionSet, &Option<Vec<String>>)> =
            role_changes.iter().map(|u| (u.user_id, u.role.as_str(), &u.permissions, &u.servers)).collect();
        if let Err(e) = state.db.set_users_role_permissions(&rows) {
            if let Ok(previous) = serde_json::to_string(&*old) {
                let _ = state.db.save_settings(&previous);
            }
            if listeners_changed {
                let _ = state.listeners.apply(&state, &old).await;
            }
            return Err(e);
        }
        // Signed-in people are not signed out: permissions are read again on every request.
        for role in new.roles.iter() {
            let names: Vec<&str> =
                role_changes.iter().filter(|u| u.role == role.key).map(|u| u.username.as_str()).collect();
            if names.is_empty() {
                continue;
            }
            state.db.audit(
                &admin.username,
                "role_applied_to_users",
                &format!(
                    "{} [{}] -> {}",
                    role.key,
                    PermissionSet::from_slice(&role.permissions).to_db_string(),
                    names.join(", ")
                ),
            );
            let (label, count, users) = (&role.label, names.len(), names.join(", "));
            messages
                .push(format!("The new permissions of the role \"{label}\" were given to {count} user(s): {users}."));
        }
    }

    // 4. Publish: from now on every request sees the new values.
    state.settings.set(new.clone());
    if changed.iter().any(|k| k.starts_with("orthanc_")) {
        state.orthancs.reconfigure(&new);
        messages.push("The connection to Orthanc was updated.".into());
        forget_removed_servers(&state, &old, &new);
    }
    // Thumbnails: resize the cache, and empty it when they are switched off or when
    // Orthanc changes (the cached images would belong to another server).
    let orthanc_changed = changed.iter().any(|k| k.starts_with("orthanc_") && k.as_str() != "orthanc_password");
    state.thumbs.trim(if new.thumbnails_enabled && !orthanc_changed { new.thumbnail_cache_bytes() } else { 0 });
    // Showing passwords again switched off: the copies kept so far are erased at once.
    if changed.iter().any(|k| k == "share_keep_passwords") && !new.share_keep_passwords {
        if let Ok(erased) = state.db.forget_share_passwords(now_unix(), true) {
            messages.push(format!("{erased} kept share link passwords were erased."));
        }
    }
    if changed.iter().any(|k| k == "session_hours") {
        messages.push("The new session duration applies to the next sign-ins.".into());
    }
    // A (shorter) retention period takes effect now, not at the next daily clean-up.
    if changed.iter().any(|k| k == "audit_retention_days") && new.audit_retention_days > 0 {
        let cutoff = now_unix() - (new.audit_retention_days as i64) * 86_400;
        if let Ok(removed) = state.db.purge_older_than(cutoff) {
            if removed > 0 {
                messages.push(format!(
                    "{removed} audit and transfer records older than {} days were deleted.",
                    new.audit_retention_days
                ));
            }
        }
    }

    state.db.audit(&admin.username, "settings_updated", &changed.join(", "));

    let mut document = settings_document(&state).await;
    document["messages"] = json!(messages);
    Ok(Json(document))
}

// ----------------------------------------------------------- test connection

/// Optional values to try. Anything omitted falls back to the SAVED setting, so
/// "Test connection" works with the password the browser never receives.
#[derive(Deserialize, Default)]
pub struct TestRequest {
    url: Option<String>,
    username: Option<String>,
    password: Option<String>,
}

pub async fn test_orthanc(
    State(state): State<AppState>,
    AuthUser(admin): AuthUser,
    Json(req): Json<TestRequest>,
) -> AppResult<Json<Diagnosis>> {
    admin.require(Permission::Admin)?;
    let saved = state.settings.get();

    let url =
        req.url.map(|u| u.trim().to_string()).filter(|u| !u.is_empty()).unwrap_or_else(|| saved.orthanc_url.clone());
    if !(url.starts_with("http://") || url.starts_with("https://"))
        || url.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(AppError::BadRequest("The Orthanc address must look like http://orthanc:8042".into()));
    }
    let username = req.username.unwrap_or_else(|| saved.orthanc_username.clone());
    let password = match req.password.filter(|p| !p.is_empty()) {
        Some(p) => p,
        None => saved.orthanc_password.0.clone(),
    };

    Ok(Json(OrthancClient::for_test(&url, &username, &password)?.diagnose().await))
}

// ---------------------------------------------------------------- test email

#[derive(Deserialize)]
pub struct TestEmail {
    to: String,
}

/// Sends a test email with the SAVED mail server settings: `{ ok, message }`.
pub async fn test_email(
    State(state): State<AppState>,
    AuthUser(admin): AuthUser,
    Json(req): Json<TestEmail>,
) -> AppResult<Json<Value>> {
    admin.require(Permission::Admin)?;
    let settings = state.settings.get();
    let to = crate::mailer::parse_recipients(&req.to).map_err(AppError::BadRequest)?;
    let to_text = crate::mailer::addresses(&to);
    let (subject, body) = crate::mailer::test_email(&settings.portal_name, &admin.username, settings.language);
    let result = crate::mailer::send(&settings, to, &subject, &body).await;
    state.db.audit(
        &admin.username,
        "email_test",
        &format!("to {to_text}: {}", if result.is_ok() { "sent" } else { "failed" }),
    );
    Ok(Json(match result {
        Ok(()) => json!({ "ok": true, "message": format!("Test email sent to {to_text}.") }),
        Err(e) => json!({ "ok": false, "message": e }),
    }))
}

// -------------------------------------------------------------- DICOM nodes

/// A DICOM node as shown in the "DICOM nodes" tab.
#[derive(Serialize)]
pub struct NodeView {
    id: String,
    aet: String,
    host: String,
    port: u64,
    allow_store: bool,
    allow_echo: bool,
    /// What the staff call it (kept by the portal, not by Orthanc).
    description: String,
}

impl NodeView {
    fn from_orthanc(id: &str, cfg: &Value) -> Self {
        let text = |key: &str| cfg[key].as_str().unwrap_or_default().to_string();
        Self {
            id: id.to_string(),
            aet: text("AET"),
            host: text("Host"),
            port: cfg["Port"].as_u64().unwrap_or(0),
            // Orthanc's defaults: both are allowed unless the configuration says otherwise.
            allow_store: cfg["AllowStore"].as_bool().unwrap_or(true),
            allow_echo: cfg["AllowEcho"].as_bool().unwrap_or(true),
            description: String::new(),
        }
    }
}

/// An alias is part of Orthanc URLs: keep it simple.
fn valid_alias(alias: &str) -> bool {
    (1..=64).contains(&alias.len()) && alias.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

pub async fn list_nodes(State(state): State<AppState>, AuthUser(admin): AuthUser) -> AppResult<Json<Vec<NodeView>>> {
    admin.require(Permission::Admin)?;
    let descriptions = state.db.node_descriptions()?;
    let mut nodes: Vec<NodeView> = state
        .orthanc
        .modalities_raw()
        .await?
        .iter()
        .map(|(id, cfg)| NodeView {
            description: descriptions.get(id.as_str()).cloned().unwrap_or_default(),
            ..NodeView::from_orthanc(id, cfg)
        })
        .collect();
    nodes.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(Json(nodes))
}

#[derive(Deserialize)]
pub struct NodeInput {
    /// DICOM Application Entity title of the remote system (max 16 characters).
    aet: String,
    /// Host name or IP address.
    host: String,
    /// DICOM port (usually 104 or 11112).
    port: u64,
    /// May Orthanc send images to it? (default yes)
    allow_store: Option<bool>,
    /// May Orthanc ping it with C-ECHO? (default yes)
    allow_echo: Option<bool>,
    /// What the staff call it, e.g. "Radiology information system" (optional, up to 80 characters).
    #[serde(default)]
    description: String,
}

/// Creates or updates a DICOM node in Orthanc.
///
/// NOTE: Orthanc keeps nodes created this way only until it restarts, unless it
/// runs with `"DicomModalitiesInDatabase": true` (set in `deploy/orthanc.json`).
pub async fn put_node(
    State(state): State<AppState>,
    AuthUser(admin): AuthUser,
    Path(alias): Path<String>,
    Json(input): Json<NodeInput>,
) -> AppResult<Json<Value>> {
    admin.require(Permission::Admin)?;
    let bad = |m: &str| Err(AppError::BadRequest(m.to_string()));

    if !valid_alias(&alias) {
        return bad("The name must have 1-64 letters, digits, '.', '-' or '_'");
    }
    let aet = input.aet.trim().to_string();
    if !(1..=16).contains(&aet.len()) || !aet.chars().all(|c| c.is_ascii_graphic() || c == ' ') || aet.contains('\\') {
        return bad("The AE title must have 1-16 printable ASCII characters (no backslash)");
    }
    let host = input.host.trim().to_string();
    if !(1..=253).contains(&host.len())
        || !host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'))
    {
        return bad("The host must be a name or an IP address");
    }
    if !(1..=65535).contains(&input.port) {
        return bad("The port must be between 1 and 65535");
    }

    // Start from the node's current configuration so settings this screen does not
    // show (AllowFind, AllowMove, TLS...) are preserved when editing.
    let existing = state.orthanc.modalities_raw().await?;
    let mut config: Map<String, Value> = match existing.get(&alias) {
        Some(Value::Object(current)) => current.clone(),
        _ => Map::new(),
    };
    config.insert("AET".into(), json!(aet));
    config.insert("Host".into(), json!(host));
    config.insert("Port".into(), json!(input.port));
    config.insert("AllowStore".into(), json!(input.allow_store.unwrap_or(true)));
    config.insert("AllowEcho".into(), json!(input.allow_echo.unwrap_or(true)));

    let description: String = input.description.trim().chars().filter(|c| !c.is_control()).collect();
    if description.chars().count() > 80 {
        return bad("The description can have at most 80 characters");
    }
    state.orthanc.put_modality(&alias, &Value::Object(config)).await?;
    state.db.set_node_description(&alias, &description)?;
    state.db.audit(&admin.username, "dicom_node_saved", &format!("{alias} = {aet}@{host}:{}", input.port));
    Ok(Json(json!({ "ok": true })))
}

pub async fn delete_node(
    State(state): State<AppState>,
    AuthUser(admin): AuthUser,
    Path(alias): Path<String>,
) -> AppResult<Json<Value>> {
    admin.require(Permission::Admin)?;
    if !valid_alias(&alias) {
        return Err(AppError::BadRequest("Invalid name".into()));
    }
    state.orthanc.delete_modality(&alias).await?;
    state.db.set_node_description(&alias, "")?;
    state.db.audit(&admin.username, "dicom_node_deleted", &alias);
    Ok(Json(json!({ "ok": true })))
}

/// DICOM "ping": does the remote system answer?
pub async fn echo_node(
    State(state): State<AppState>,
    AuthUser(admin): AuthUser,
    Path(alias): Path<String>,
) -> AppResult<Json<Value>> {
    admin.require(Permission::Admin)?;
    if !valid_alias(&alias) {
        return Err(AppError::BadRequest("Invalid name".into()));
    }
    state.orthanc.echo_modality(&alias).await?;
    Ok(Json(json!({ "ok": true })))
}

// ------------------------------------------------------------ roles -> users

/// An account that follows a role whose permissions were just changed.
#[derive(Debug, PartialEq)]
struct RoleUpdate {
    user_id: i64,
    username: String,
    role: String,
    permissions: PermissionSet,
    /// The role's Orthanc servers (`None` = all).
    servers: Option<Vec<String>>,
}

/// A removed Orthanc server leaves the lists of servers of the accounts that had it. An account
/// limited to some servers stays limited (to those left, possibly none): it never becomes "all".
fn forget_removed_servers(state: &AppState, old: &Settings, new: &Settings) {
    let kept: Vec<String> = new.servers().into_iter().map(|s| s.key).collect();
    let removed: Vec<String> = old.servers().into_iter().map(|s| s.key).filter(|k| !kept.contains(k)).collect();
    if removed.is_empty() {
        return;
    }
    let Ok(users) = state.db.list_users() else { return };
    for user in users {
        let Some(list) = &user.servers else { continue };
        if list.iter().any(|k| removed.contains(k)) {
            let left: Vec<String> = list.iter().filter(|k| !removed.contains(k)).cloned().collect();
            if let Err(e) = state.db.set_user_servers(user.id, &Some(left)) {
                tracing::error!("the servers of {} could not be updated: {e}", user.username);
            }
        }
    }
}

/// Users follow their role: when a role's permissions change, every account with that role gets
/// the new permissions. The role of an account is the one stored with it, or (older accounts,
/// a deleted role) the role with exactly its permissions, as the Users list shows it.
///
/// Refused (nothing is saved) when the new permissions would leave no active administrator, or
/// would take the administrator rights away from the person saving, as in the user form.
fn role_updates(old: &Settings, new: &Settings, users: &[User], acting_admin: i64) -> AppResult<Vec<RoleUpdate>> {
    // Roles that exist before and after with other permissions or servers (a renamed key is a new role).
    let changed: Vec<(&str, PermissionSet, &Option<Vec<String>>)> = new
        .roles
        .iter()
        .filter_map(|r| {
            let before = old.roles.iter().find(|o| o.key == r.key)?;
            let after = PermissionSet::from_slice(&r.permissions);
            (PermissionSet::from_slice(&before.permissions) != after || before.servers != r.servers).then_some((
                r.key.as_str(),
                after,
                &r.servers,
            ))
        })
        .collect();
    if changed.is_empty() {
        return Ok(Vec::new());
    }
    let mut updates = Vec::new();
    let (mut admins_after, mut admin_removed) = (0, false);
    for user in users {
        let role = old.role_of(&user.role, &user.permissions).map(|r| r.key.as_str());
        let follow = changed.iter().find(|(key, _, _)| Some(*key) == role);
        let after = follow.map_or(&user.permissions, |(_, p, _)| p);
        let loses_admin = user.permissions.contains(Permission::Admin) && !after.contains(Permission::Admin);
        if user.active && after.contains(Permission::Admin) {
            admins_after += 1;
        }
        if let Some((key, permissions, servers)) = follow {
            if loses_admin && user.active {
                if user.id == acting_admin {
                    return Err(AppError::Conflict(format!(
                        "You cannot remove the administrator rights of your own account: you have the role \
                         '{key}'. Give your account another role first (Users)."
                    )));
                }
                admin_removed = true;
            }
            if *permissions != user.permissions || user.role != *key || **servers != user.servers {
                updates.push(RoleUpdate {
                    user_id: user.id,
                    username: user.username.clone(),
                    role: key.to_string(),
                    permissions: permissions.clone(),
                    servers: (*servers).clone(),
                });
            }
        }
    }
    if admin_removed && admins_after == 0 {
        return Err(AppError::Conflict(
            "At least one active administrator is required: these role changes would remove the last one".into(),
        ));
    }
    Ok(updates)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Role;

    fn user(id: i64, role: &str, permissions: &[Permission]) -> User {
        User {
            id,
            username: format!("user{id}"),
            password_hash: String::new(),
            permissions: PermissionSet::from_slice(permissions),
            role: role.into(),
            active: true,
            created_at: 0,
            last_login: None,
            email: String::new(),
            two_factor: false,
            recovery_codes_left: 0,
            totp_secret: None,
            totp_pending: None,
            servers: None,
        }
    }

    fn with_role(settings: &Settings, key: &str, permissions: &[Permission]) -> Settings {
        let mut s = settings.clone();
        s.roles.iter_mut().find(|r| r.key == key).unwrap().permissions = permissions.to_vec();
        s
    }

    const VIEWER: [Permission; 2] = [Permission::Read, Permission::Visualize];
    const VIEWER_PLUS: [Permission; 3] = [Permission::Read, Permission::Visualize, Permission::Download];

    #[test]
    fn users_follow_their_role() {
        let old = Settings::defaults();
        let new = with_role(&old, "viewer", &VIEWER_PLUS);
        let users = [
            user(1, "administrator", &Permission::ALL),
            user(2, "viewer", &VIEWER),       // stored role
            user(3, "", &VIEWER),             // older account: found by its permissions
            user(4, "", &[Permission::Read]), // custom: untouched
            user(
                5,
                "exporter",
                &[Permission::Read, Permission::Visualize, Permission::Download, Permission::Send, Permission::Share],
            ),
        ];
        let updates = role_updates(&old, &new, &users, 1).unwrap();
        let ids: Vec<i64> = updates.iter().map(|u| u.user_id).collect();
        assert_eq!(ids, [2, 3]);
        assert!(updates.iter().all(|u| u.role == "viewer" && u.permissions == PermissionSet::from_slice(&VIEWER_PLUS)));
    }

    /// A role limited to some servers gives that limit to its users, like its permissions.
    #[test]
    fn users_follow_the_servers_of_their_role() {
        let old = Settings::defaults();
        let mut new = old.clone();
        new.roles[0].servers = Some(vec!["main".into()]);
        let users = [user(2, "viewer", &VIEWER), user(3, "", &VIEWER_PLUS)];
        let updates = role_updates(&old, &new, &users, 1).unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!((updates[0].user_id, updates[0].servers.clone()), (2, Some(vec!["main".to_string()])));
        // Back to every server.
        let back =
            role_updates(&new, &old, &[User { servers: Some(vec!["main".into()]), ..user(2, "viewer", &VIEWER) }], 1)
                .unwrap();
        assert_eq!(back[0].servers, None);
    }

    #[test]
    fn nothing_when_only_the_label_changes() {
        let old = Settings::defaults();
        let mut new = old.clone();
        new.roles[0].label = "Readers".into();
        new.roles.push(Role { key: "new".into(), label: "New".into(), permissions: VIEWER.to_vec(), servers: None });
        assert!(role_updates(&old, &new, &[user(2, "viewer", &VIEWER)], 1).unwrap().is_empty());
    }

    #[test]
    fn administrators_are_protected() {
        let old = Settings::defaults();
        let new = with_role(&old, "administrator", &VIEWER);
        // Your own role: refused.
        let users = [user(1, "administrator", &Permission::ALL), user(2, "", &[Permission::Admin, Permission::Read])];
        assert!(matches!(role_updates(&old, &new, &users, 1), Err(AppError::Conflict(_))));
        // Somebody else's, with another administrator left: allowed.
        let users = [user(1, "", &[Permission::Admin, Permission::Read]), user(2, "administrator", &Permission::ALL)];
        assert_eq!(role_updates(&old, &new, &users, 1).unwrap().len(), 1);
        // The role of the only active administrator: refused (the other one is disabled).
        let mut disabled = user(1, "", &[Permission::Admin, Permission::Read]);
        disabled.active = false;
        let users = [disabled, user(2, "administrator", &Permission::ALL)];
        assert!(matches!(role_updates(&old, &new, &users, 1), Err(AppError::Conflict(_))));
    }
}
