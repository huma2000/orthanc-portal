//! # config.rs
//!
//! The few settings that must be known **before** the database can be opened, and
//! therefore cannot be edited from the web interface. Everything else lives in
//! `settings.rs` and is managed from the Settings tab.
//!
//! | Variable | Meaning |
//! |---|---|
//! | `DATABASE_PATH` | SQLite file with users, audit log, transfers **and the saved settings** |
//! | `ADMIN_USERNAME` / `ADMIN_PASSWORD` | first administrator, created only if there are no users |
//! | `RESET_SETTINGS` | `true` = forget the settings saved from the web interface and go back to the defaults / `.env` values (recovery, see README) |
//! | `LISTENERS_FROM_ENV` | `true` = the two listening addresses (`BIND_ADDR`, `REMOTE_BIND_ADDR`) ALWAYS come from the environment and cannot be changed in Settings (set by the Docker image, where the published ports must match) |
//!
//! It also provides the small helpers used to read environment variables.

use anyhow::{anyhow, Result};
use std::str::FromStr;

/// Start-up settings. Built once in `main`.
#[derive(Debug, Clone)]
pub struct Bootstrap {
    /// Path of the SQLite file.
    pub database_path: String,
    /// Username / password of the first administrator, used only if no users exist.
    pub admin_username: String,
    pub admin_password: Option<String>,
    /// Discard the settings saved in the database at start-up (emergency recovery).
    pub reset_settings: bool,
    /// The listening addresses come from the environment on every start and are
    /// read-only in the Settings screen (Docker: they must match the published ports).
    pub listeners_from_env: bool,
}

impl Bootstrap {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            database_path: env_or("DATABASE_PATH", "portal.db"),
            admin_username: env_or("ADMIN_USERNAME", "admin"),
            admin_password: env_opt("ADMIN_PASSWORD"),
            reset_settings: env_flag("RESET_SETTINGS"),
            listeners_from_env: env_flag("LISTENERS_FROM_ENV"),
        })
    }
}

/// Returns the variable's value, or `None` when it is unset or blank.
pub fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// `true` when the variable is `1`, `true` or `yes` (any case); `false` otherwise.
pub fn env_flag(key: &str) -> bool {
    env_opt(key).map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes")).unwrap_or(false)
}

/// Returns the variable's value, or `default` when it is unset or blank.
pub fn env_or(key: &str, default: &str) -> String {
    env_opt(key).unwrap_or_else(|| default.to_string())
}

/// Parses the variable into any type implementing `FromStr` (numbers, ...).
pub fn env_parse<T: FromStr>(key: &str, default: T) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    match env_opt(key) {
        None => Ok(default),
        Some(raw) => raw.parse::<T>().map_err(|e| anyhow!("invalid value for {key}: {e}")),
    }
}
