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
use crate::error::{AppError, AppResult};
use crate::orthanc::{Diagnosis, OrthancClient};
use crate::permissions::Permission;
use crate::settings::default_roles;
use crate::state::AppState;
use crate::util::now_unix;
use axum::extract::{Path, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

// ------------------------------------------------------------------ public

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
    }))
}

// ---------------------------------------------------------------- settings

/// Settings + the state of the listeners + factory roles (for "Restore defaults").
async fn settings_document(state: &AppState) -> Value {
    json!({
        "settings": state.settings.get().to_view(),
        "listeners": state.listeners.status().await,
        "defaults": { "roles": default_roles() },
        "branding": state.branding.describe(),
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
    let changed = old.changed_keys(&new);
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

    // 4. Publish: from now on every request sees the new values.
    state.settings.set(new.clone());
    if changed.iter().any(|k| k.starts_with("orthanc_")) {
        state.orthanc.reconfigure(&new);
        messages.push("The connection to Orthanc was updated.".into());
    }
    // Thumbnails: resize the cache, and empty it when they are switched off or when
    // Orthanc changes (the cached images would belong to another server).
    let orthanc_changed = changed.iter().any(|k| k.starts_with("orthanc_") && k.as_str() != "orthanc_password");
    state.thumbs.trim(if new.thumbnails_enabled && !orthanc_changed { new.thumbnail_cache_bytes() } else { 0 });
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
        }
    }
}

/// An alias is part of Orthanc URLs: keep it simple.
fn valid_alias(alias: &str) -> bool {
    (1..=64).contains(&alias.len()) && alias.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

pub async fn list_nodes(State(state): State<AppState>, AuthUser(admin): AuthUser) -> AppResult<Json<Vec<NodeView>>> {
    admin.require(Permission::Admin)?;
    let mut nodes: Vec<NodeView> =
        state.orthanc.modalities_raw().await?.iter().map(|(id, cfg)| NodeView::from_orthanc(id, cfg)).collect();
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

    state.orthanc.put_modality(&alias, &Value::Object(config)).await?;
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
