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
//!
//! ## Which server the staff viewers read
//! OHIF and Weasis only know study UIDs (`/dicom-web/studies/{uid}/...`), and every request they
//! make names one, so [`OrthancPool::for_study`] picks the server from it: the one the person
//! opened the study from (the portal notes it when they click a viewer), otherwise the first of
//! their servers that holds that study, remembered for a while. Prefixed paths (`/o/archive/...`)
//! cannot work: the DICOMweb plugin writes absolute links to `/dicom-web/` in its answers.

use crate::db::User;
use crate::error::AppError;
use crate::orthanc::OrthancClient;
use crate::settings::{Settings, MAIN_SERVER};
use crate::util::is_valid_orthanc_id;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

/// How long the server a person opened a study from is remembered for its viewer requests.
const ROUTE_TTL_SECS: i64 = 30 * 60;
/// How long "no server has it" is remembered (the study may arrive soon).
const MISSING_TTL_SECS: i64 = 30;
/// At most this many study routes are remembered at once.
const MAX_ROUTES: usize = 20_000;
/// `(user id, StudyInstanceUID)` -> `(server key, expiry)`.
type Routes = HashMap<(i64, String), (String, i64)>;

/// How long the number of distinct studies of a set of servers is reused.
const DISTINCT_TTL_SECS: i64 = 5 * 60;
/// How long a server may take to list its studies for that count.
const LIST_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a server may take to say whether it holds a study.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);

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
    label: String,
    color: String,
    enabled: bool,
    searched_by_default: bool,
    client: OrthancClient,
}

/// What the web page and the search answers say about a server: its key, name and badge colour.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ServerInfo {
    pub key: String,
    pub label: String,
    pub color: String,
    pub searched_by_default: bool,
}

/// The clients of every Orthanc server, rebuilt when the settings change. Cheap to clone.
///
/// ## Who may use which server
/// Staff routes always name the person asking ([`get`](Self::get), [`resolve`](Self::resolve),
/// [`usable`](Self::usable)...): a server the account may not use (`User::may_use_server`)
/// answers exactly like a server that does not exist (404), so its studies cannot even be
/// confirmed to exist. Only the records that already carry their server and were checked when
/// they were made (a guest's link, a background task) use [`stored`](Self::stored).
#[derive(Clone)]
pub struct OrthancPool {
    members: Arc<RwLock<Arc<Vec<Member>>>>,
    /// `(user id, StudyInstanceUID)` -> `(server key, expiry)`: which server a person's viewer
    /// reads a study from (see [`for_study`](Self::for_study)).
    routes: Arc<Mutex<Routes>>,
    /// Servers' keys (joined) -> (number of distinct studies, when counted).
    distinct: Arc<Mutex<HashMap<String, (u64, i64)>>>,
}

impl OrthancPool {
    /// `main` is the client of the default server (the one in `AppState::orthanc`): the pool
    /// shares it, so both always point at the same place.
    pub fn new(main: OrthancClient, settings: &Settings) -> Result<Self, AppError> {
        let pool = Self {
            members: Arc::new(RwLock::new(Arc::new(Vec::new()))),
            routes: Arc::default(),
            distinct: Arc::default(),
        };
        pool.rebuild(Some(main), settings)?;
        Ok(pool)
    }

    /// Follows new settings: servers added, removed, moved or with another account. Clients of
    /// servers that stay are reconfigured in place (their caches survive when nothing changed).
    pub fn reconfigure(&self, settings: &Settings) {
        if let Err(e) = self.rebuild(None, settings) {
            tracing::error!("Orthanc servers: {e}");
        }
        // Servers may have moved: the viewers look their studies up again, and studies are counted again.
        self.routes.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.distinct.lock().unwrap_or_else(|e| e.into_inner()).clear();
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
            members.push(Member {
                key: server.key.clone(),
                label: server.name().to_string(),
                color: server.color.clone(),
                enabled: server.enabled,
                searched_by_default: server.searched_by_default,
                client,
            });
        }
        *self.members.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(members);
        Ok(())
    }

    fn snapshot(&self) -> Arc<Vec<Member>> {
        self.members.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The client of a server that exists, is switched on and that this person may use;
    /// otherwise 404 (an ID of a removed, switched-off or forbidden server is treated like an
    /// ID that does not exist).
    pub fn get(&self, user: &User, key: &str) -> Result<OrthancClient, AppError> {
        if !user.may_use_server(key) {
            return Err(AppError::NotFound);
        }
        self.stored(key)
    }

    /// The same without a person: for records that carry their server and were checked when
    /// they were made (a guest's link, a background task). Never for a value from a request.
    pub fn stored(&self, key: &str) -> Result<OrthancClient, AppError> {
        self.snapshot().iter().find(|m| m.key == key && m.enabled).map(|m| m.client.clone()).ok_or(AppError::NotFound)
    }

    /// The client of the server a resource is on.
    pub fn of(&self, user: &User, resource: &ResourceRef) -> Result<OrthancClient, AppError> {
        self.get(user, &resource.server)
    }

    /// Reads an ID from a request (`id` or `key:id`) and gives the client of its server.
    pub fn resolve(&self, user: &User, raw: &str) -> Result<(OrthancClient, ResourceRef), AppError> {
        let resource = ResourceRef::parse(raw)?;
        Ok((self.of(user, &resource)?, resource))
    }

    /// An ID stored with a record (see [`stored`](Self::stored)).
    pub fn resolve_stored(&self, raw: &str) -> Result<(OrthancClient, ResourceRef), AppError> {
        let resource = ResourceRef::parse(raw)?;
        Ok((self.stored(&resource.server)?, resource))
    }

    /// Several IDs that one Orthanc must handle together (one ZIP, one CD): they must all be on
    /// the same server. Gives its client, its key and the plain Orthanc IDs.
    pub fn resolve_together(
        &self,
        user: &User,
        raws: &[&str],
    ) -> Result<(OrthancClient, String, Vec<String>), AppError> {
        let refs = raws.iter().map(|raw| ResourceRef::parse(raw)).collect::<Result<Vec<_>, _>>()?;
        let Some(first) = refs.first() else { return Err(AppError::BadRequest("Choose at least one study".into())) };
        if refs.iter().any(|r| r.server != first.server) {
            return Err(AppError::BadRequest(
                "These studies are on several Orthanc servers: choose the studies of one server at a time".into(),
            ));
        }
        Ok((self.of(user, first)?, first.server.clone(), refs.into_iter().map(|r| r.id).collect()))
    }

    /// The default server, for the pages that do not offer a choice of server yet (upload,
    /// query / retrieve, worklists, the DICOM nodes of Send). Refused with an explanation to an
    /// account that may not use it.
    pub fn default_for(&self, user: &User) -> Result<OrthancClient, AppError> {
        if !user.may_use_server(MAIN_SERVER) {
            let name = self.info(MAIN_SERVER).map(|s| s.label).unwrap_or_else(|| "Main".into());
            return Err(AppError::Denied(format!(
                "Your account cannot use the Orthanc server \"{name}\", and this page only works with that one for now."
            )));
        }
        self.stored(MAIN_SERVER)
    }

    /// The servers this person may use that are switched on, in the configured order (the
    /// default one first when it is one of them), with their clients.
    pub fn usable(&self, user: &User) -> Vec<(ServerInfo, OrthancClient)> {
        self.snapshot()
            .iter()
            .filter(|m| m.enabled && user.may_use_server(&m.key))
            .map(|m| (Self::describe(m), m.client.clone()))
            .collect()
    }

    /// Notes that this person opens the study `uid` from `server` (a viewer button): their viewer's
    /// requests for it go there. Ignored for a server they may not use.
    pub fn note_route(&self, user: &User, uid: &str, server: &str) {
        if self.get(user, server).is_ok() {
            self.remember_route(user.id, uid, server, ROUTE_TTL_SECS);
        }
    }

    fn remember_route(&self, user_id: i64, uid: &str, server: &str, ttl: i64) {
        let now = crate::util::now_unix();
        let mut routes = self.routes.lock().unwrap_or_else(|e| e.into_inner());
        if routes.len() >= MAX_ROUTES {
            routes.retain(|_, (_, expiry)| *expiry > now);
            if routes.len() >= MAX_ROUTES {
                routes.clear();
            }
        }
        routes.insert((user_id, uid.to_string()), (server.to_string(), now + ttl));
    }

    /// The server a person's viewer reads the study `uid` from, among the servers they may use:
    /// the one they opened it from, else the first that holds it (asked at the same time, and
    /// remembered), else their first server. Without a UID (a viewer's own study list), their
    /// first server. 404 when they may use no server.
    pub async fn for_study(&self, user: &User, uid: Option<&str>) -> Result<OrthancClient, AppError> {
        let usable = self.usable(user);
        let Some((first, _)) = usable.first() else { return Err(AppError::NotFound) };
        let client_of = |key: &str| usable.iter().find(|(s, _)| s.key == key).map(|(_, c)| c.clone());
        let Some(uid) = uid.filter(|_| usable.len() > 1) else { return Ok(usable[0].1.clone()) };
        let now = crate::util::now_unix();
        let noted = self
            .routes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(user.id, uid.to_string()))
            .filter(|(_, expiry)| *expiry > now)
            .map(|(key, _)| key.clone());
        if let Some(client) = noted.as_deref().and_then(client_of) {
            return Ok(client);
        }
        // Only plain UIDs reach Orthanc's query (no wildcard can widen it).
        if !crate::util::is_valid_dicom_uid(uid) {
            return Ok(usable[0].1.clone());
        }
        let query = serde_json::json!({ "Level": "Study", "Query": { "StudyInstanceUID": uid }, "Limit": 1 });
        let answers = futures_util::future::join_all(usable.iter().map(|(_, client)| {
            let query = &query;
            async move {
                matches!(tokio::time::timeout(LOOKUP_TIMEOUT, client.find(query)).await, Ok(Ok(found)) if !found.is_empty())
            }
        }))
        .await;
        match answers.iter().position(|has| *has) {
            Some(i) => {
                self.remember_route(user.id, uid, &usable[i].0.key, ROUTE_TTL_SECS);
                Ok(usable[i].1.clone())
            }
            None => {
                self.remember_route(user.id, uid, &first.key, MISSING_TTL_SECS);
                Ok(usable[0].1.clone())
            }
        }
    }

    /// How many different studies these servers hold together: a study on several servers counts
    /// once. Orthanc gives the same study the same ID on every server (it is made from the patient
    /// ID and the StudyInstanceUID), so this is the number of distinct IDs of their study lists.
    /// Listing every study is not free on a big archive: the answer is reused for a few minutes.
    /// `None` when a server could not list its studies.
    pub async fn distinct_studies(&self, servers: &[(ServerInfo, OrthancClient)]) -> Option<u64> {
        let key = servers.iter().map(|(s, _)| s.key.as_str()).collect::<Vec<_>>().join(",");
        let now = crate::util::now_unix();
        if let Some((count, at)) = self.distinct.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
            if now - at < DISTINCT_TTL_SECS {
                return Some(*count);
            }
        }
        let lists = futures_util::future::join_all(servers.iter().map(|(_, client)| async move {
            match tokio::time::timeout(LIST_TIMEOUT, client.get_json("/studies")).await {
                Ok(Ok(Value::Array(ids))) => Some(ids),
                _ => None,
            }
        }))
        .await;
        let mut ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        for list in lists {
            ids.extend(list?.into_iter().filter_map(|id| id.as_str().map(str::to_string)));
        }
        let count = ids.len() as u64;
        self.distinct.lock().unwrap_or_else(|e| e.into_inner()).insert(key, (count, now));
        Some(count)
    }

    /// Every server that is switched on, in order, with its client: for background tasks that
    /// belong to no person (auto-forward).
    pub fn all_enabled(&self) -> Vec<(ServerInfo, OrthancClient)> {
        self.snapshot().iter().filter(|m| m.enabled).map(|m| (Self::describe(m), m.client.clone())).collect()
    }

    /// Key, name and colour of a server (switched-off ones included).
    pub fn info(&self, key: &str) -> Option<ServerInfo> {
        self.snapshot().iter().find(|m| m.key == key).map(Self::describe)
    }

    fn describe(m: &Member) -> ServerInfo {
        ServerInfo {
            key: m.key.clone(),
            label: m.label.clone(),
            color: m.color.clone(),
            searched_by_default: m.searched_by_default,
        }
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

    fn account(permissions: &[crate::permissions::Permission], servers: Option<&[&str]>) -> User {
        User {
            id: 7,
            username: "someone".into(),
            password_hash: String::new(),
            permissions: crate::permissions::PermissionSet::from_slice(permissions),
            role: String::new(),
            active: true,
            created_at: 0,
            last_login: None,
            email: String::new(),
            two_factor: false,
            recovery_codes_left: 0,
            totp_secret: None,
            totp_pending: None,
            servers: servers.map(|s| s.iter().map(|k| k.to_string()).collect()),
        }
    }

    /// An account limited to some servers: the others answer like servers that do not exist.
    #[test]
    fn each_account_reaches_its_own_servers() {
        use crate::permissions::Permission::{Admin, Read};
        let settings = settings_with(json!([
            { "key": "archive", "url": "http://archive:8042" },
            { "key": "research", "url": "http://research:8042", "searched_by_default": false }
        ]));
        let pool = OrthancPool::new(OrthancClient::new(&settings).unwrap(), &settings).unwrap();
        let keys = |user: &User| pool.usable(user).into_iter().map(|(s, _)| s.key).collect::<Vec<_>>();

        let everyone = account(&[Read], None);
        assert_eq!(keys(&everyone), ["main", "archive", "research"]);

        let archive_only = account(&[Read], Some(&["archive"]));
        assert_eq!(keys(&archive_only), ["archive"]);
        assert!(pool.get(&archive_only, "archive").is_ok());
        assert!(matches!(pool.get(&archive_only, "research"), Err(AppError::NotFound)));
        assert!(
            matches!(pool.resolve(&archive_only, ID), Err(AppError::NotFound)),
            "a plain ID is the default server's"
        );
        assert!(pool.resolve(&archive_only, &format!("archive:{ID}")).is_ok());
        assert!(matches!(pool.resolve_together(&archive_only, &[ID]), Err(AppError::NotFound)));
        assert!(matches!(pool.default_for(&archive_only), Err(AppError::Denied(_))));
        // Records that carry their server (a guest's link) are not tied to a person.
        assert!(pool.resolve_stored(ID).is_ok());

        let nobody = account(&[Read], Some(&[]));
        assert!(keys(&nobody).is_empty());

        // Administrators reach every server whatever their list says.
        let admin = account(&[Admin, Read], Some(&["archive"]));
        assert_eq!(keys(&admin), ["main", "archive", "research"]);
        assert!(pool.default_for(&admin).is_ok());
    }

    /// The server a viewer reads a study from: noted when it is opened, only among the person's.
    #[tokio::test]
    async fn viewers_follow_the_server_a_study_was_opened_from() {
        use crate::permissions::Permission::Read;
        let settings = settings_with(json!([{ "key": "archive", "url": "http://127.0.0.1:9" }]));
        let pool = OrthancPool::new(OrthancClient::new(&settings).unwrap(), &settings).unwrap();
        let user = account(&[Read], None);
        let archive = pool.get(&user, "archive").unwrap();
        let main = pool.get(&user, "main").unwrap();
        pool.note_route(&user, "1.2.3", "archive");
        assert!(pool.for_study(&user, Some("1.2.3")).await.unwrap().same_as(&archive));
        assert!(pool.for_study(&user, None).await.unwrap().same_as(&main), "no UID: the first server");
        // A server the person may not use is never noted.
        let main_only = account(&[Read], Some(&["main"]));
        pool.note_route(&main_only, "1.2.4", "archive");
        assert!(pool.for_study(&main_only, Some("1.2.4")).await.unwrap().same_as(&main));
        // New settings: looked up again.
        pool.reconfigure(&settings);
        assert!(pool.routes.lock().unwrap().is_empty());
    }

    #[test]
    fn the_pool_follows_the_settings() {
        let settings = settings_with(json!([
            { "key": "archive", "label": "Archive", "url": "http://archive:8042" },
            { "key": "research", "url": "http://research:8042", "enabled": false }
        ]));
        let main = OrthancClient::new(&settings).unwrap();
        let pool = OrthancPool::new(main, &settings).unwrap();
        assert!(pool.stored("main").is_ok() && pool.stored("archive").is_ok());
        assert!(matches!(pool.stored("research"), Err(AppError::NotFound)), "switched off");
        assert!(matches!(pool.stored("nope"), Err(AppError::NotFound)));
        assert_eq!(pool.enabled_keys(), ["main", "archive"]);
        pool.reconfigure(&settings_with(json!([])));
        assert_eq!(pool.enabled_keys(), ["main"]);
        assert!(matches!(pool.stored("archive"), Err(AppError::NotFound)), "removed");
    }
}
