//! # permissions.rs
//!
//! The permission model of the portal, in two layers:
//!
//! 1. **Permissions** - what each *user* may do. Every user owns a set of them.
//!    Each API route checks the one it needs (`user.require(Permission::X)`), so
//!    access control is enforced on the server; hiding a button in the web UI is
//!    only cosmetic.
//! 2. **Access scope** - where the request comes *from*. Requests arriving
//!    through the Cloudflare Tunnel (internet) get the `Remote` scope, which only
//!    ever allows viewing, whatever the user's own permissions are. Requests
//!    from the intranet get the `Full` scope.
//!
//! The permissions a request really has are `user permissions  ∩  scope allows`
//! (see [`PermissionSet::restricted_to`]).
//!
//! | Permission  | What it allows                                                      |
//! |-------------|---------------------------------------------------------------------|
//! | `read`      | Search / list studies and series, see their metadata  (remote: yes) |
//! | `visualize` | Open a study in Weasis or OHIF; see preview thumbnails  (remote: yes) |
//! | `upload`    | Upload DICOM files (or ZIP archives) into the PACS     (intranet only)|
//! | `download`  | Download studies / series as a ZIP file                (intranet only)|
//! | `send`      | Send a study to a remote DICOM node (Orthanc "modality") (intranet only)|
//! | `admin`     | Manage users and read the audit log                    (intranet only)|
//!
//! To add a new permission (e.g. `delete`): add a variant to [`Permission`],
//! extend `ALL`, `as_str` and `description`, decide in [`AccessScope::allows`]
//! whether it may be used remotely, then call `user.require(...)` in the new
//! route. Nothing else changes (the database stores permissions as text).

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
    Admin,
}

impl Permission {
    /// Every permission, in the order shown in the admin screen.
    pub const ALL: [Permission; 6] = [
        Permission::Read,
        Permission::Visualize,
        Permission::Upload,
        Permission::Download,
        Permission::Send,
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
            Permission::Admin => "admin",
        }
    }

    /// Short explanation displayed next to the checkbox in the admin screen.
    pub fn description(self) -> &'static str {
        match self {
            Permission::Read => "Search and browse studies and series (also available remotely)",
            Permission::Visualize => {
                "Open studies in Weasis or OHIF and see preview thumbnails (also available remotely)"
            }
            Permission::Upload => "Upload DICOM files into the PACS (intranet only)",
            Permission::Download => "Download studies and series as ZIP files (intranet only)",
            Permission::Send => "Send studies to remote DICOM nodes (intranet only)",
            Permission::Admin => "Manage users and view the audit log (intranet only)",
        }
    }

    /// Parses the machine name back into a permission.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == s)
    }
}

/// Where a request comes from. Decided by the server (never by the browser):
/// see `auth::effective_scope`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AccessScope {
    /// Intranet: every permission of the user is honoured.
    Full,
    /// Internet (Cloudflare Tunnel): VIEWING ONLY, whatever the user's permissions.
    Remote,
}

impl AccessScope {
    /// May a request of this scope use the given permission at all?
    pub fn allows(self, permission: Permission) -> bool {
        match self {
            AccessScope::Full => true,
            // The one place that defines what the internet-facing interface can do.
            AccessScope::Remote => matches!(permission, Permission::Read | Permission::Visualize),
        }
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

    /// Keeps only what the access scope allows (intersection).
    pub fn restricted_to(&self, scope: AccessScope) -> Self {
        Self(self.0.iter().copied().filter(|p| scope.allows(*p)).collect())
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

    /// Consistency rule: viewing, downloading or sending is pointless without
    /// being able to find studies, so those permissions require `read`.
    pub fn validate(&self) -> Result<(), AppError> {
        let needs_read = self.contains(Permission::Visualize)
            || self.contains(Permission::Download)
            || self.contains(Permission::Send);
        if needs_read && !self.contains(Permission::Read) {
            return Err(AppError::BadRequest(
                "The 'visualize', 'download' and 'send' permissions require the 'read' permission".into(),
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
