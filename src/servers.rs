//! # servers.rs
//!
//! The portal can work with several Orthanc servers. The default one is described by the
//! `orthanc_*` settings and has the key `main`; the others are in `orthanc_servers`.
//!
//! ## Which server a study, series or image is on
//! Orthanc makes its IDs from the DICOM UIDs, so **the same study has the same ID on every
//! server that holds it**: an ID alone cannot say which server is meant. The portal therefore
//! writes the resources of the other servers `key:id` (`archive:8a5f3e0b-...`) everywhere it
//! hands out or stores an ID: API answers, links, and the database (shares, transfers,
//! recently viewed...). Those of the default server keep their plain ID, so nothing changes
//! for an installation with one server, and IDs saved by older versions keep working.
//! [`ResourceRef`] reads and checks both forms; [`OrthancPool`] gives the client of a server.

use crate::error::AppError;
use crate::orthanc::OrthancClient;
use crate::settings::{Settings, MAIN_SERVER};
use crate::util::is_valid_orthanc_id;
use std::sync::{Arc, RwLock};

/// A study, series or image on one Orthanc server: `id` or `key:id`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResourceRef {
    /// The server's key (`main` for the default one).
    pub server: String,
    /// Orthanc's own ID on that server.
    pub id: String,
}

impl ResourceRef {
    /// Reads `id` (default server) or `key:id`. A malformed value is the usual "400 Invalid id".
    pub fn parse(raw: &str) -> Result<Self, AppError> {
        Self::try_parse(raw).ok_or_else(|| AppError::BadRequest("Invalid id".into()))
    }

    /// The same, `None` when malformed.
    pub fn try_parse(raw: &str) -> Option<Self> {
        let (server, id) = match raw.split_once(':') {
            Some((server, id)) => (server, id),
            None => (MAIN_SERVER, raw),
        };
        let key_ok = !server.is_empty()
            && server.len() <= 24
            && server.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        (key_ok && is_valid_orthanc_id(id)).then(|| Self { server: server.to_string(), id: id.to_string() })
    }

    /// Another resource (a series of this study, a copy...) on the same server.
    pub fn sibling(&self, id: &str) -> Self {
        Self { server: self.server.clone(), id: id.to_string() }
    }

    /// The form handed out and stored: the plain ID on the default server, `key:id` elsewhere.
    pub fn compound(&self) -> String {
        compound(&self.server, &self.id)
    }

    pub fn is_main(&self) -> bool {
        self.server == MAIN_SERVER
    }
}

impl std::fmt::Display for ResourceRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.compound())
    }
}

/// `id` on the default server, `key:id` on the others.
pub fn compound(server: &str, id: &str) -> String {
    if server == MAIN_SERVER {
        id.to_string()
    } else {
        format!("{server}:{id}")
    }
}

/// One server the pool can reach.
#[derive(Clone)]
struct Member {
    key: String,
    enabled: bool,
    client: OrthancClient,
}

/// The clients of every Orthanc server, rebuilt when the settings change. Cheap to clone.
#[derive(Clone)]
pub struct OrthancPool {
    members: Arc<RwLock<Arc<Vec<Member>>>>,
}

impl OrthancPool {
    /// `main` is the client of the default server (the one in `AppState::orthanc`): the pool
    /// shares it, so both always point at the same place.
    pub fn new(main: OrthancClient, settings: &Settings) -> Result<Self, AppError> {
        let pool = Self { members: Arc::new(RwLock::new(Arc::new(Vec::new()))) };
        pool.rebuild(Some(main), settings)?;
        Ok(pool)
    }

    /// Follows new settings: servers added, removed, moved or with another account. Clients of
    /// servers that stay are reconfigured in place (their caches survive when nothing changed).
    pub fn reconfigure(&self, settings: &Settings) {
        if let Err(e) = self.rebuild(None, settings) {
            tracing::error!("Orthanc servers: {e}");
        }
    }

    fn rebuild(&self, main: Option<OrthancClient>, settings: &Settings) -> Result<(), AppError> {
        let current = self.snapshot();
        let mut members = Vec::new();
        for server in settings.servers() {
            let existing = current.iter().find(|m| m.key == server.key).map(|m| m.client.clone());
            let client = match (server.key == MAIN_SERVER, &main, existing) {
                (true, Some(main), _) => main.clone(),
                (true, None, Some(client)) => {
                    client.reconfigure(settings);
                    client
                }
                (false, _, Some(client)) => {
                    client.reconfigure_server(&server);
                    client
                }
                (true, None, None) => OrthancClient::new(settings)?,
                (false, _, None) => OrthancClient::for_server(&server)?,
            };
            members.push(Member { key: server.key.clone(), enabled: server.enabled, client });
        }
        *self.members.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(members);
        Ok(())
    }

    fn snapshot(&self) -> Arc<Vec<Member>> {
        self.members.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The client of a server that exists and is switched on; otherwise 404 (an ID of a server
    /// that was removed or switched off is treated like an ID that does not exist).
    pub fn get(&self, key: &str) -> Result<OrthancClient, AppError> {
        self.snapshot().iter().find(|m| m.key == key && m.enabled).map(|m| m.client.clone()).ok_or(AppError::NotFound)
    }

    /// The client of the server a resource is on.
    pub fn of(&self, resource: &ResourceRef) -> Result<OrthancClient, AppError> {
        self.get(&resource.server)
    }

    /// Reads an ID from a request (`id` or `key:id`) and gives the client of its server.
    pub fn resolve(&self, raw: &str) -> Result<(OrthancClient, ResourceRef), AppError> {
        let resource = ResourceRef::parse(raw)?;
        Ok((self.of(&resource)?, resource))
    }

    /// Several IDs that one Orthanc must handle together (one ZIP, one CD): they must all be on
    /// the same server. Gives its client, its key and the plain Orthanc IDs.
    pub fn resolve_together(&self, raws: &[&str]) -> Result<(OrthancClient, String, Vec<String>), AppError> {
        let refs = raws.iter().map(|raw| ResourceRef::parse(raw)).collect::<Result<Vec<_>, _>>()?;
        let Some(first) = refs.first() else { return Err(AppError::BadRequest("Choose at least one study".into())) };
        if refs.iter().any(|r| r.server != first.server) {
            return Err(AppError::BadRequest(
                "These studies are on several Orthanc servers: choose the studies of one server at a time".into(),
            ));
        }
        Ok((self.of(first)?, first.server.clone(), refs.into_iter().map(|r| r.id).collect()))
    }

    /// The keys of the servers that are switched on, the default one first.
    #[cfg(test)]
    fn enabled_keys(&self) -> Vec<String> {
        self.snapshot().iter().filter(|m| m.enabled).map(|m| m.key.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ID: &str = "8a5f3e0b-1c2d3e4f-5a6b7c8d-9e0f1a2b-3c4d5e6f";

    #[test]
    fn plain_and_compound_ids() {
        let main = ResourceRef::parse(ID).unwrap();
        assert_eq!((main.server.as_str(), main.id.as_str()), ("main", ID));
        assert_eq!(main.compound(), ID, "the default server keeps plain IDs");
        let other = ResourceRef::parse(&format!("archive-2:{ID}")).unwrap();
        assert_eq!((other.server.as_str(), other.id.as_str()), ("archive-2", ID));
        assert_eq!(other.compound(), format!("archive-2:{ID}"));
        assert_eq!(other.sibling("abc").compound(), "archive-2:abc");
        assert_eq!(ResourceRef::parse(&format!("main:{ID}")).unwrap().compound(), ID);
        for bad in [
            "",
            ":",
            &format!(":{ID}"),
            &format!("Archive:{ID}"),
            &format!("a b:{ID}"),
            "archive:",
            "x:../system",
            &format!("{}:{ID}", "k".repeat(25)),
            &format!("a:b:{ID}"),
        ] {
            assert!(ResourceRef::try_parse(bad).is_none(), "{bad}");
        }
    }

    fn settings_with(servers: serde_json::Value) -> Settings {
        Settings::defaults().with_patch(&json!({ "orthanc_servers": servers })).unwrap()
    }

    #[test]
    fn the_pool_follows_the_settings() {
        let settings = settings_with(json!([
            { "key": "archive", "label": "Archive", "url": "http://archive:8042" },
            { "key": "research", "url": "http://research:8042", "enabled": false }
        ]));
        let main = OrthancClient::new(&settings).unwrap();
        let pool = OrthancPool::new(main, &settings).unwrap();
        assert!(pool.get("main").is_ok() && pool.get("archive").is_ok());
        assert!(matches!(pool.get("research"), Err(AppError::NotFound)), "switched off");
        assert!(matches!(pool.get("nope"), Err(AppError::NotFound)));
        assert_eq!(pool.enabled_keys(), ["main", "archive"]);
        pool.reconfigure(&settings_with(json!([])));
        assert_eq!(pool.enabled_keys(), ["main"]);
        assert!(matches!(pool.get("archive"), Err(AppError::NotFound)), "removed");
    }
}
