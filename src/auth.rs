//! # auth.rs
//!
//! Everything related to *who is calling*:
//!
//! * password hashing with **Argon2** (CPU heavy, so it runs on a blocking thread);
//! * [`TokenStore`]: in-memory map of random tokens -> user id. It is used twice:
//!   - **sessions**: the `portal_session` cookie set after login;
//!   - **Weasis tokens**: short-lived bearer tokens placed in `weasis://` links;
//! * [`LoginThrottle`]: temporary lockout after repeated failed logins;
//! * [`effective_scope`]: decides whether a request comes from the intranet
//!   (`Full`) or from the internet through Cloudflare (`Remote`, view-only);
//! * the [`AuthUser`] extractor: add `AuthUser(user): AuthUser` to a handler's
//!   arguments and the request is rejected with 401 unless a valid session exists.
//!
//! The user's *permissions* are re-read from the database on every request, so
//! disabling an account or changing its permissions takes effect immediately.

use crate::config::Bootstrap;
use crate::db::{Db, User};
use crate::error::{AppError, AppResult};
use crate::permissions::{AccessScope, Permission, PermissionSet};
use crate::settings::{CookieSecure, Settings};
use crate::state::AppState;
use crate::util::{is_https, now_unix, random_token};
use argon2::password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::extract::FromRequestParts;
use axum::http::{header, request::Parts, HeaderMap};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Name of the session cookie.
pub const SESSION_COOKIE: &str = "portal_session";
/// Minimum length of a password.
pub const MIN_PASSWORD_LEN: usize = 10;
/// Longest password accepted (refuses absurd inputs before any hashing).
pub const MAX_PASSWORD_LEN: usize = 1024;
/// Most browser sessions one user may have at the same time; the oldest are dropped.
pub const MAX_SESSIONS_PER_USER: usize = 20;
/// Safety cap on Weasis tokens per user (normally a user holds one or two, see `issue_or_reuse`).
pub const MAX_WEASIS_TOKENS_PER_USER: usize = 50;
/// Password hashes / checks that may run at the same time. Argon2 deliberately uses
/// a lot of memory (~19 MB each): without a limit a flood of logins could exhaust it.
const MAX_PARALLEL_HASHING: usize = 4;
static HASH_GATE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(MAX_PARALLEL_HASHING);

// =========================================================== password hashing

pub fn validate_password(password: &str) -> AppResult<()> {
    let length = password.chars().count();
    if length < MIN_PASSWORD_LEN {
        return Err(AppError::BadRequest(format!("The password must have at least {MIN_PASSWORD_LEN} characters")));
    }
    if length > MAX_PASSWORD_LEN {
        return Err(AppError::BadRequest(format!("The password can have at most {MAX_PASSWORD_LEN} characters")));
    }
    Ok(())
}

/// Cheap shape check of a user name, done BEFORE any hashing, throttling or logging:
/// 1-64 letters, digits and `. _ @ -`. It stops absurdly long or odd names from being
/// stored in memory or in the audit log, and keeps `:` out of names so they can never
/// collide with the `ip:...` keys of the login throttle.
pub fn plausible_username(name: &str) -> bool {
    (1..=64).contains(&name.chars().count())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '@' | '-'))
}

/// Synchronous Argon2 hashing with a fresh random salt. Output is a PHC string
/// (`$argon2id$v=19$...`) that contains the salt and the parameters.
fn hash_password_blocking(password: &str) -> Result<String, AppError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| AppError::Internal(format!("password hashing failed: {e}")))
}

/// Hashes a password without blocking the async runtime.
pub async fn hash_password(password: String) -> AppResult<String> {
    let _slot = HASH_GATE.acquire().await.map_err(|_| AppError::Internal("the hashing gate was closed".into()))?;
    tokio::task::spawn_blocking(move || hash_password_blocking(&password))
        .await
        .map_err(|e| AppError::Internal(format!("hashing task failed: {e}")))?
}

/// A throw-away hash, verified when the username does not exist so that "unknown
/// user" and "wrong password" take the same time (no username enumeration).
static DUMMY_HASH: OnceLock<String> = OnceLock::new();

/// Checks a password. `stored_hash = None` means "no such user": a dummy
/// verification is still performed and the result is always `false`.
pub async fn verify_password(stored_hash: Option<String>, password: String) -> bool {
    let Ok(_slot) = HASH_GATE.acquire().await else { return false };
    tokio::task::spawn_blocking(move || {
        let is_real = stored_hash.is_some();
        let hash = stored_hash.unwrap_or_else(|| {
            DUMMY_HASH.get_or_init(|| hash_password_blocking("dummy-password-for-timing").unwrap_or_default()).clone()
        });
        let verified = match PasswordHash::new(&hash) {
            Ok(parsed) => Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok(),
            Err(_) => false,
        };
        verified && is_real
    })
    .await
    .unwrap_or(false)
}

// ================================================================ token store

/// One issued token.
struct TokenEntry {
    user_id: i64,
    /// Creation order (an ever-increasing counter). Used to find the OLDEST tokens of a
    /// user; the expiry time cannot do that, because it only has one-second resolution.
    seq: u64,
    /// Unix time after which the token is worthless.
    expires_at: i64,
}

/// Thread-safe `token -> user` map with expiry. Kept in memory on purpose:
/// restarting the portal logs everybody out, which is acceptable and simple.
#[derive(Clone, Default)]
pub struct TokenStore {
    inner: Arc<Mutex<HashMap<String, TokenEntry>>>,
    /// Source of the `seq` numbers.
    counter: Arc<AtomicU64>,
}

impl TokenStore {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, TokenEntry>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Creates a new random token for `user_id`, valid for `ttl_secs` seconds.
    /// A user can hold at most `max_per_user` tokens: beyond that the OLDEST ones are
    /// dropped, so repeated logins (or requests) can never grow the memory without limit.
    pub fn issue(&self, user_id: i64, ttl_secs: i64, max_per_user: usize) -> String {
        let token = random_token();
        let seq = self.counter.fetch_add(1, Ordering::Relaxed);
        let mut map = self.map();
        map.insert(token.clone(), TokenEntry { user_id, seq, expires_at: now_unix() + ttl_secs });

        let mut mine: Vec<(u64, String)> =
            map.iter().filter(|(_, e)| e.user_id == user_id).map(|(t, e)| (e.seq, t.clone())).collect();
        if mine.len() > max_per_user {
            mine.sort();
            for (_, oldest) in mine.iter().take(mine.len() - max_per_user) {
                map.remove(oldest);
            }
        }
        token
    }

    /// Like [`TokenStore::issue`], but hands out a token the user already has when
    /// it still has more than half of its lifetime left. Used for Weasis links: every
    /// "Open in Weasis" click would otherwise create one more token, and all of them
    /// grant exactly the same (read-only viewing) access anyway.
    pub fn issue_or_reuse(&self, user_id: i64, ttl_secs: i64, max_per_user: usize) -> String {
        {
            let map = self.map();
            let now = now_unix();
            let reusable = map
                .iter()
                .filter(|(_, e)| e.user_id == user_id && e.expires_at - now > ttl_secs / 2)
                .max_by_key(|(_, e)| e.expires_at);
            if let Some((token, _)) = reusable {
                return token.clone();
            }
        }
        self.issue(user_id, ttl_secs, max_per_user)
    }

    /// Returns the user the token belongs to, or `None` if unknown/expired.
    pub fn lookup(&self, token: &str) -> Option<i64> {
        let mut map = self.map();
        match map.get(token) {
            Some(entry) if entry.expires_at > now_unix() => Some(entry.user_id),
            Some(_) => {
                map.remove(token); // expired: forget it
                None
            }
            None => None,
        }
    }

    /// Invalidates a single token (logout).
    pub fn revoke(&self, token: &str) {
        self.map().remove(token);
    }

    /// Invalidates every token of a user (password change, account disabled/deleted).
    pub fn revoke_user(&self, user_id: i64) {
        self.map().retain(|_, e| e.user_id != user_id);
    }

    /// Drops expired tokens; called periodically from `main.rs`.
    pub fn purge_expired(&self) {
        let now = now_unix();
        self.map().retain(|_, e| e.expires_at > now);
    }
}

// ============================================================ login throttling

// The number of failures and the lockout duration are settings: see
// `Settings::login_max_failures` / `login_lockout_minutes` (Settings screen).

struct Attempts {
    failures: u32,
    locked_until: i64,
    last_failure: i64,
}

/// Slows down password guessing. Keyed by lower-cased user name AND by client address
/// (`ip:<address>`, see [`ip_key`]), so one address cannot try thousands of different
/// names either.
#[derive(Clone, Default)]
pub struct LoginThrottle {
    inner: Arc<Mutex<HashMap<String, Attempts>>>,
}

impl LoginThrottle {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, Attempts>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Fails with 429 while the username is locked out.
    pub fn check(&self, username: &str) -> AppResult<()> {
        let now = now_unix();
        if let Some(a) = self.map().get(&username.to_lowercase()) {
            if a.locked_until > now {
                let minutes = (a.locked_until - now + 59) / 60;
                return Err(AppError::TooManyRequests(format!(
                    "Too many failed attempts. Try again in {minutes} minute(s)."
                )));
            }
        }
        Ok(())
    }

    /// Counts a failed login; after `max_failures` in a row the account is locked
    /// for `lockout_secs` seconds (both values come from the settings).
    pub fn record_failure(&self, username: &str, max_failures: u32, lockout_secs: i64) {
        let now = now_unix();
        let mut map = self.map();
        let entry =
            map.entry(username.to_lowercase()).or_insert(Attempts { failures: 0, locked_until: 0, last_failure: now });
        // Failures only count while they are recent: old typos are forgiven.
        if now - entry.last_failure > lockout_secs.max(900) {
            entry.failures = 0;
        }
        entry.failures += 1;
        entry.last_failure = now;
        if entry.failures >= max_failures {
            entry.locked_until = now + lockout_secs;
            entry.failures = 0; // a fresh series of attempts starts after the lockout
        }
    }

    pub fn record_success(&self, username: &str) {
        self.map().remove(&username.to_lowercase());
    }

    /// Forgets old records so random usernames cannot grow the map forever.
    pub fn purge_old(&self) {
        let cutoff = now_unix() - 3600;
        self.map().retain(|_, a| a.last_failure > cutoff || a.locked_until > now_unix());
    }
}

/// Throttle key of a client address. User names cannot contain `:` (see
/// [`plausible_username`]), so these keys never collide with user names.
pub fn ip_key(ip: &str) -> String {
    format!("ip:{ip}")
}

// ===================================================================== cookies

/// Finds one cookie by name in the request's `Cookie` header(s).
pub fn read_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|raw| raw.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

/// Should the cookie carry the `Secure` flag for THIS request? (see [`CookieSecure`])
pub fn cookie_secure(settings: &Settings, headers: &HeaderMap) -> bool {
    match settings.cookie_secure {
        CookieSecure::Always => true,
        CookieSecure::Never => false,
        CookieSecure::Auto => is_https(headers),
    }
}

/// Builds the `Set-Cookie` value for a new session.
///  * `HttpOnly`  -> JavaScript cannot read the cookie (limits XSS damage)
///  * `SameSite=Lax` -> not sent on cross-site POSTs (CSRF protection)
pub fn session_cookie(token: &str, max_age_secs: u64, secure: bool) -> String {
    format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age_secs}{}",
        if secure { "; Secure" } else { "" }
    )
}

/// `Set-Cookie` value that deletes the session cookie in the browser.
pub fn clear_session_cookie(secure: bool) -> String {
    session_cookie("", 0, secure)
}

// ============================================================ request guards

// ================================================================ access scope

/// Does the request carry the headers that Cloudflare adds to everything it forwards?
/// A browser or Weasis on the intranet never sends them, and a remote visitor
/// cannot remove them: Cloudflare sets them on its way in.
pub fn is_cloudflare_request(headers: &HeaderMap) -> bool {
    headers.contains_key("cf-connecting-ip")
        || headers.contains_key("cf-ray")
        || headers
            .get("cdn-loop")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_ascii_lowercase().contains("cloudflare"))
            .unwrap_or(false)
}

/// Decides whether this request is **Full** (intranet) or **Remote** (view-only).
///
/// Two independent safeguards, either one is enough to make it `Remote`:
///  1. it arrived on the remote listener (`state.listener_scope`) - the port the
///     Cloudflare Tunnel is configured to use;
///  2. it carries Cloudflare headers - this protects you if the tunnel is
///     mistakenly pointed at the intranet port.
pub fn effective_scope(state: &AppState, headers: &HeaderMap) -> AccessScope {
    if state.listener_scope == AccessScope::Remote || is_cloudflare_request(headers) {
        AccessScope::Remote
    } else {
        AccessScope::Full
    }
}

/// Best-effort client address for the audit log: the real visitor address given
/// by Cloudflare when present, otherwise the TCP peer.
pub fn client_ip(headers: &HeaderMap, peer: std::net::SocketAddr) -> String {
    headers
        .get("cf-connecting-ip")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.chars().filter(|c| !c.is_control()).take(45).collect())
        .unwrap_or_else(|| peer.ip().to_string())
}

// ============================================================ request guards

/// Loads the user behind a token and checks the account is still enabled.
/// The permissions are already narrowed to the request's scope.
fn active_user(state: &AppState, user_id: i64, headers: &HeaderMap) -> AppResult<User> {
    let user = state.db.get_user(user_id)?.ok_or(AppError::Unauthorized)?;
    if !user.active {
        return Err(AppError::Unauthorized);
    }
    Ok(user.with_scope(effective_scope(state, headers)))
}

/// Extractor for "a logged-in user". Usage: `AuthUser(user): AuthUser`.
/// It only proves WHO the caller is; handlers still call `user.require(...)`
/// to check WHAT the caller may do (already limited by the intranet/internet scope).
pub struct AuthUser(pub User);

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let token = read_cookie(&parts.headers, SESSION_COOKIE).ok_or(AppError::Unauthorized)?;
        let user_id = state.sessions.lookup(&token).ok_or(AppError::Unauthorized)?;
        Ok(AuthUser(active_user(state, user_id, &parts.headers)?))
    }
}

/// Authentication for everything the viewers read through the portal:
/// `/dicom-web/*`, `/wado` and `/ohif/*`.
///
/// * **Weasis** is a separate program without our cookie, so it sends
///   `Authorization: Bearer <token>` with the token from its `weasis://` link.
/// * **OHIF** runs inside a browser tab of the portal, so the normal session
///   cookie is sent automatically.
///
/// The user must hold the `visualize` permission (allowed remotely too). Over the internet only the
/// cookie is accepted: Weasis (bearer token) is an intranet-only viewer.
pub fn authenticate_viewer(state: &AppState, headers: &HeaderMap) -> AppResult<User> {
    // Over the internet only the browser's session cookie is accepted. A Weasis token (even a
    // genuine one, issued on the intranet) is ignored there: Weasis saves the images on the
    // computer, and images must not leave the building that way.
    let remote = effective_scope(state, headers) == AccessScope::Remote;
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| t.trim().to_string())
        .filter(|_| !remote);

    let user_id = if let Some(token) = bearer {
        state.weasis_tokens.lookup(&token).ok_or(AppError::Unauthorized)?
    } else if let Some(cookie) = read_cookie(headers, SESSION_COOKIE) {
        state.sessions.lookup(&cookie).ok_or(AppError::Unauthorized)?
    } else {
        return Err(AppError::Unauthorized);
    };

    let user = active_user(state, user_id, headers)?;
    user.require(Permission::Visualize)?;
    Ok(user)
}

// ============================================================ first start-up

/// Creates the first administrator when the user table is empty.
/// With no `ADMIN_PASSWORD` configured, a random one is generated and printed
/// ONCE in the log - change it after the first login.
pub async fn ensure_initial_admin(db: &Db, boot: &Bootstrap) -> AppResult<()> {
    if db.count_users()? > 0 {
        return Ok(());
    }
    let (password, generated) = match &boot.admin_password {
        Some(p) => (p.clone(), false),
        None => (random_token()[..16].to_string(), true),
    };
    if !plausible_username(&boot.admin_username) {
        return Err(AppError::BadRequest(
            "ADMIN_USERNAME may only contain letters, digits and . _ @ - (max 64 characters)".into(),
        ));
    }
    validate_password(&password)?;
    let hash = hash_password(password.clone()).await?;
    db.create_user(&boot.admin_username, &hash, &PermissionSet::from_slice(&Permission::ALL))?;
    if generated {
        tracing::warn!("==========================================================");
        tracing::warn!(" First start: administrator account created");
        tracing::warn!("   username: {}", boot.admin_username);
        tracing::warn!("   password: {password}   (shown only once)");
        tracing::warn!("==========================================================");
    } else {
        tracing::info!("Administrator '{}' created from ADMIN_PASSWORD", boot.admin_username);
    }
    Ok(())
}
