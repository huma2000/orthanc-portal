//! # permissions.rs
//!
//! The permission model of the portal. Every user owns a **set of permissions**; each API
//! route checks the one it needs (`user.require(Permission::X)`), so access control is
//! enforced on the server - hiding a button in the web UI is only cosmetic.
//!
//! | Permission  | What it allows                                                         |
//! |-------------|------------------------------------------------------------------------|
//! | `read`      | Search / list studies and series, see their metadata                   |
//! | `visualize` | Open a study in Weasis or OHIF, see preview thumbnails                 |
//! | `upload`    | Upload DICOM files (or ZIP archives) into the PACS                     |
//! | `download`  | Download studies / series as a ZIP file                                |
//! | `send`      | Send a study to a remote DICOM node (an Orthanc "modality")            |
//! | `share`     | Invite somebody OUTSIDE: password-protected link to ONE study          |
//! | `admin`     | Manage users and settings, read the audit log                          |
//!
//! All accounts live on the **intranet**. People outside never have an account: they get a
//! one-study invitation link (see `routes/shares.rs` and `routes/guest.rs`), which has its
//! own rules and is not part of this permission model.
//!
//! To add a new permission (e.g. `delete`): add a variant to [`Permission`], extend `ALL`,
//! `as_str` and `description`, then call `user.require(...)` in the new route. Nothing else
//! changes (the database stores permissions as text).

use crate::error::AppError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// A single capability a user may or may not have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Permission {
    Read,
    Visualize,
    Upload,
    Download,
    Send,
    Share,
    Admin,
}

impl Permission {
    /// Every permission, in the order shown in the admin screen.
    pub const ALL: [Permission; 7] = [
        Permission::Read,
        Permission::Visualize,
        Permission::Upload,
        Permission::Download,
        Permission::Send,
        Permission::Share,
        Permission::Admin,
    ];

    /// Stable machine name, used in the database, in the JSON API and in error messages.
    pub fn as_str(self) -> &'static str {
        match self {
            Permission::Read => "read",
            Permission::Visualize => "visualize",
            Permission::Upload => "upload",
            Permission::Download => "download",
            Permission::Send => "send",
            Permission::Share => "share",
            Permission::Admin => "admin",
        }
    }

    /// Short explanation displayed next to the checkbox in the admin screen.
    pub fn description(self) -> &'static str {
        match self {
            Permission::Read => "Search and browse studies and series",
            Permission::Visualize => "Open studies in Weasis or OHIF and see preview thumbnails",
            Permission::Upload => "Upload DICOM files into the PACS",
            Permission::Download => "Download studies and series as ZIP files",
            Permission::Send => "Send studies to remote DICOM nodes",
            Permission::Share => {
                "Invite people outside: password-protected link to one study (view + limited downloads)"
            }
            Permission::Admin => "Manage users and settings, view the audit log",
        }
    }

    /// Parses the machine name back into a permission.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == s)
    }
}

/// The set of permissions held by one user.
/// Serialises as a plain JSON array of names: `["read","visualize"]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PermissionSet(BTreeSet<Permission>);

impl PermissionSet {
    /// Does the set contain this permission?
    pub fn contains(&self, p: Permission) -> bool {
        self.0.contains(&p)
    }

    /// Builds a set from a slice (used for the role presets).
    pub fn from_slice(perms: &[Permission]) -> Self {
        Self(perms.iter().copied().collect())
    }

    /// Parses names coming from the admin API; rejects unknown names and
    /// enforces the consistency rules in [`PermissionSet::validate`].
    pub fn parse_list(names: &[String]) -> Result<Self, AppError> {
        let mut set = BTreeSet::new();
        for name in names {
            let perm =
                Permission::parse(name).ok_or_else(|| AppError::BadRequest(format!("Unknown permission '{name}'")))?;
            set.insert(perm);
        }
        let set = Self(set);
        set.validate()?;
        Ok(set)
    }

    /// Consistency rule: viewing, downloading, sending or sharing is pointless without
    /// being able to find studies, so those permissions require `read`.
    pub fn validate(&self) -> Result<(), AppError> {
        let needs_read = self.contains(Permission::Visualize)
            || self.contains(Permission::Download)
            || self.contains(Permission::Send)
            || self.contains(Permission::Share);
        if needs_read && !self.contains(Permission::Read) {
            return Err(AppError::BadRequest(
                "The 'visualize', 'download', 'send' and 'share' permissions require the 'read' permission".into(),
            ));
        }
        Ok(())
    }

    /// Storage format in the SQLite column: `"read,visualize,upload"`.
    pub fn to_db_string(&self) -> String {
        self.0.iter().map(|p| p.as_str()).collect::<Vec<_>>().join(",")
    }

    /// Reads the storage format back. Unknown tokens are ignored (safe default: no access).
    pub fn from_db_string(s: &str) -> Self {
        Self(s.split(',').filter_map(|t| Permission::parse(t.trim())).collect())
    }
}
