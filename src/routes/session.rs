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
use crate::settings::{CardSize, ModalityButton, RecentLayout, RecentOrder, TwoFactorMode};
use crate::state::AppState;
use crate::totp;
use crate::util::now_unix;
use axum::{
    extract::{ConnectInfo, State},
    http::{header, HeaderMap, HeaderValue},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
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
    /// "Send the invitation by email" starts ticked when a new link is created.
    email_default: bool,
    /// An emailed password goes in a second email, without the link (Settings > Share links).
    email_split: bool,
    /// "Share an anonymized copy" is offered.
    anonymize: bool,
    /// "Email me when the link is opened or downloaded" is offered (mail server + an address on the account).
    notify: bool,
    /// "Every study of this patient" is offered (and links for several studies are switched on).
    patient: bool,
    /// QR codes of links are offered.
    qr: bool,
    /// "Every study with a label" is offered (Shares page).
    label: bool,
}

/// Upload links (people outside send studies in), as the Shares page needs them.
#[derive(Serialize)]
pub struct UploadLinkFeature {
    /// Switched on AND this user may create them (share + upload).
    enabled: bool,
    default_hours: u64,
    max_hours: u64,
    max_mb: u64,
    label: String,
    /// "Email me when files arrive" is offered.
    notify: bool,
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
    /// Colours of labels chosen in Settings > Search & labels (label -> #rrggbb).
    label_colors: std::collections::BTreeMap<String, String>,
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
    /// Upload links for people outside.
    upload_links: UploadLinkFeature,
    /// Two-step sign-in: off, optional (each person chooses, My account) or required.
    two_factor: TwoFactorMode,
    /// The "Recently viewed" list of the home page: how many (0 = not shown).
    recently_viewed: u64,
    /// Administrators only: a newer version published on GitHub (`{ version, url, name }`).
    #[serde(skip_serializing_if = "Option::is_none")]
    update: Option<serde_json::Value>,
}

/// Answer of `/api/login` and `/api/me`: the user fields (id, username,
/// permissions...) plus the features.
#[derive(Serialize)]
pub struct Me {
    #[serde(flatten)]
    user: User,
    features: Features,
    /// The Orthanc servers this person may use (switched-on ones, in order): what the search
    /// page offers. The server controls appear only with two or more.
    orthanc_servers: Vec<crate::servers::ServerInfo>,
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
    let user_may_read = user.permissions.contains(Permission::Read);
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
        "stats_page": settings.stats_page && is_admin,
        "cd_export": settings.cd_export && may(Permission::Download),
        "backups": settings.backups_enabled && is_admin,
        "account_self_service": settings.account_self_service,
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
        email_default: settings.email_configured() && settings.share_email_default,
        email_split: settings.share_email_split_password,
        anonymize: settings.share_anonymize,
        notify: settings.share_notify && settings.email_configured(),
        patient: settings.multi_study_shares && settings.share_patient,
        qr: settings.share_qr,
        label: settings.multi_study_shares && settings.share_label && settings.labels_enabled,
    };
    let upload_links = UploadLinkFeature {
        enabled: settings.upload_links
            && user.permissions.contains(Permission::Share)
            && user.permissions.contains(Permission::Upload),
        default_hours: settings.upload_link_default_hours.min(settings.share_max_hours),
        max_hours: settings.share_max_hours,
        max_mb: settings.upload_link_max_mb,
        label: settings.upload_link_label.clone(),
        notify: settings.share_notify && settings.email_configured(),
    };
    let search = SearchFeature {
        columns: settings.study_columns.clone(),
        fields: settings.study_search_fields.clone(),
        live: settings.search_as_you_type,
        min_chars: settings.search_min_chars,
        infinite: settings.infinite_scroll,
        allowed_labels: settings.available_labels.clone(),
        label_colors: settings.label_colors.clone(),
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
    let orthanc_servers = state.orthancs.usable(&user).into_iter().map(|(info, _)| info).collect();
    Me {
        user,
        orthanc_servers,
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
            upload_links,
            two_factor: settings.two_factor,
            recently_viewed: if settings.recently_viewed && user_may_read { settings.recently_viewed_count } else { 0 },
            update: if is_admin { crate::updates::newer_release(state) } else { None },
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
    state.throttle.record_success(&username);

    // 3. Two-step sign-in (Settings > Security & roles): the password is not enough. The answer
    //    carries a short-lived ticket; POST /api/login/two-factor with the code finishes the sign-in.
    if settings.two_factor_on() {
        if user.two_factor {
            let ticket = state.login_tickets.issue(user.id, LOGIN_TICKET_SECS, 3);
            return Ok(Json(json!({ "two_factor": "code", "ticket": ticket })).into_response());
        }
        if settings.two_factor == TwoFactorMode::Required {
            // Not set up yet and required: set it up now, before any session exists.
            let secret = totp::new_secret();
            state.db.set_totp_pending(user.id, Some(&secret))?;
            let ticket = state.login_tickets.issue(user.id, LOGIN_TICKET_SECS, 3);
            let mut answer = setup_material(&settings.portal_name, &user.username, &secret);
            answer["two_factor"] = json!("setup");
            answer["ticket"] = json!(ticket);
            return Ok(Json(answer).into_response());
        }
    }

    // 4. Success: create the session and send the cookie.
    start_session(&state, user, &headers, &ip, None).await
}

/// How long the code may take to arrive after the password (seconds).
const LOGIN_TICKET_SECS: i64 = 300;

/// What the app needs to set up two-step sign-in: the secret (to type by hand), the `otpauth://`
/// address and its QR code (SVG).
pub fn setup_material(portal: &str, username: &str, secret: &str) -> Value {
    let uri = totp::otpauth_uri(portal, username, secret);
    // In groups of four, easier to type: "ABCD EFGH ...".
    let grouped: Vec<String> = secret.as_bytes().chunks(4).map(|c| String::from_utf8_lossy(c).into_owned()).collect();
    json!({ "secret": grouped.join(" "), "uri": uri, "qr": totp::qr_svg(&uri) })
}

/// Signs `user` in: the session cookie and the answer of /api/me (plus `extra` fields).
async fn start_session(
    state: &AppState,
    user: User,
    headers: &HeaderMap,
    ip: &str,
    extra: Option<(&str, Value)>,
) -> AppResult<Response> {
    let settings = state.settings.get();
    state.db.touch_login(user.id)?;
    state.db.audit(&user.username, "login", &format!("ip={ip}{}", if user.two_factor { " (two-step)" } else { "" }));

    let ttl = settings.session_ttl_secs();
    let token = state.sessions.issue(user.id, ttl as i64, auth::MAX_SESSIONS_PER_USER);
    let secure = auth::cookie_secure(&settings, headers);
    let cookie = auth::session_cookie(&token, ttl, secure);

    let mut body =
        serde_json::to_value(me_response(state, user).await).map_err(|e| AppError::Internal(e.to_string()))?;
    if let Some((key, value)) = extra {
        body[key] = value;
    }
    let mut response = Json(body).into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, HeaderValue::from_str(&cookie).map_err(|e| AppError::Internal(e.to_string()))?);
    Ok(response)
}

#[derive(Deserialize)]
pub struct TwoFactorLogin {
    ticket: String,
    /// The 6 digits of the app, or one of the recovery codes.
    code: String,
}

/// Second step of the sign-in: the code of the authenticator app (or a recovery code). For an
/// account setting it up at this sign-in (required by the settings), the first code confirms the
/// set-up and the answer carries the recovery codes, shown once.
pub async fn login_two_factor(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<TwoFactorLogin>,
) -> AppResult<Response> {
    let settings = state.settings.get();
    let ip = auth::client_ip(&headers, peer);
    let expired = || AppError::BadRequest("The sign-in took too long. Enter your user name and password again.".into());
    if req.ticket.len() != 64 || req.code.chars().count() > 40 {
        return Err(expired());
    }
    let user_id = state.login_tickets.lookup(&req.ticket).ok_or_else(expired)?;
    let user = state.db.get_user(user_id)?.filter(|u| u.active).ok_or_else(expired)?;
    let key = format!("2fa:{}", user.id);
    if let Err(locked) = state.throttle.check(&key) {
        state.login_tickets.revoke(&req.ticket);
        return Err(locked);
    }

    let now = now_unix();
    let mut recovery_codes = None;
    let accepted = if user.two_factor {
        let secret = user.totp_secret.clone().unwrap_or_default();
        if totp::looks_like_recovery_code(&req.code) {
            let used = state.db.use_recovery_code(user.id, &totp::recovery_hash(&req.code))?;
            if used {
                state.db.audit(&user.username, "recovery_code_used", &format!("ip={ip}"));
            }
            used
        } else {
            match totp::verify(&secret, &req.code, now) {
                Some(step) => state.db.use_totp_step(user.id, step)?,
                None => false,
            }
        }
    } else if let Some(pending) = user.totp_pending.clone() {
        // Setting it up at this sign-in.
        match totp::verify(&pending, &req.code, now) {
            Some(step) => {
                let codes = totp::new_recovery_codes();
                let hashes: Vec<String> = codes.iter().map(|c| totp::recovery_hash(c)).collect();
                state.db.enable_totp(user.id, &pending, step, &hashes)?;
                // Sessions opened with the password alone, before it was required, end now.
                state.sessions.revoke_user(user.id);
                state.weasis_tokens.revoke_user(user.id);
                state.db.audit(&user.username, "two_factor_enabled", "at sign-in");
                recovery_codes = Some(codes);
                true
            }
            None => false,
        }
    } else {
        return Err(expired());
    };

    if !accepted {
        state.throttle.record_failure(&key, settings.login_max_failures, settings.lockout_secs());
        state.db.audit(&user.username, "login_code_failed", &format!("ip={ip}"));
        return Err(AppError::BadRequest("The code is not correct (or was already used). Try the next one.".into()));
    }
    state.throttle.record_success(&key);
    state.login_tickets.revoke(&req.ticket);
    let user = state.db.get_user(user.id)?.ok_or(AppError::Unauthorized)?;
    start_session(&state, user, &headers, &ip, recovery_codes.map(|codes| ("recovery_codes", json!(codes)))).await
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
