//! # routes/studies.rs
//!
//! Read-only "consult" endpoints. All of them require the `read` permission and
//! exist only on the intranet listener (guests with an invitation never search).
//!
//! * `GET /api/studies?patient_name=&patient_id=&accession_number=&description=&modality=&date_from=&date_to=`
//!   `&birth_from=&birth_to=&patient_sex=&referring_physician=&requesting_physician=&institution=`
//!   `&other_patient_ids=&study_uid=&labels=&labels_mode=any|all&no_labels=true&sort=&dir=asc|desc&limit=&offset=`
//!
//! A text filter is a "contains" search; `*` and `?` are wildcards, and a value between double
//! quotes (`"P-001"`) must match exactly.
//! * `GET /api/studies/{id}/series`   series of one study
//! * `GET /api/series?...`            search directly at series level
//! * `GET /api/stats`                 number of studies in the PACS
//!
//! ## Several Orthanc servers
//! Both searches take `servers=main,archive` (keys); without it they search the servers marked
//! "searched by default", always among those the person may use. Each server is asked at the
//! same time (at most [`SERVER_TIMEOUT`]); the answers are merged in the order asked, the same
//! study (or series) on several servers is ONE row whose `servers` lists them, and a server
//! that did not answer is named in `unavailable` (the others' results are still given). Pages
//! of a search over several servers are cut from the merged list, so they cannot go beyond
//! [`MAX_MERGED_OFFSET`] results.
//!
//! Searching is delegated to Orthanc's `POST /tools/find` (recent "extended find"
//! API). The `Query` object uses DICOM matching rules: `*` / `?` wildcards, date
//! ranges `YYYYMMDD-YYYYMMDD`, and **lists of values separated by a backslash**,
//! which DICOM defines as "any of": `"CT\\MR"` finds CT *or* MR.

use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use crate::models::{SeriesSummary, StudySummary};
use crate::orthanc::OrthancClient;
use crate::permissions::Permission;
use crate::servers::{ResourceRef, ServerInfo};
use crate::state::AppState;
use crate::util::{contains_pattern, dicom_date, is_valid_dicom_uid, non_empty};
use axum::extract::{Path, Query, State};
use axum::Json;
use futures_util::{stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::time::Duration;

/// Query-string parameters accepted by both search endpoints. Every field is optional.
#[derive(Debug, Deserialize)]
pub struct SearchParams {
    /// Patient name. A plain word means "contains"; use `*`/`?` for explicit wildcards.
    pub patient_name: Option<String>,
    pub patient_id: Option<String>,
    pub accession_number: Option<String>,
    /// Study description when searching studies, series description when searching series.
    pub description: Option<String>,
    /// One or several modality codes separated by commas, e.g. `CT,MR` or `RX,US`.
    /// `RX` is accepted as a shortcut for the two X-ray codes `CR` and `DX`.
    pub modality: Option<String>,
    /// Series search only: body part examined (CHEST, HEAD...).
    pub body_part: Option<String>,
    /// Study date range, `YYYY-MM-DD` (either side may be omitted).
    pub date_from: Option<String>,
    pub date_to: Option<String>,
    /// Studies only: `received` = most recently received by Orthanc first (the home page);
    /// anything else = newest study date first. `sort` (below) wins when given.
    pub order: Option<String>,
    /// Studies only: patient birth date range, `YYYY-MM-DD` (either side may be omitted).
    pub birth_from: Option<String>,
    pub birth_to: Option<String>,
    /// Studies only: `M`, `F` or `O`.
    pub patient_sex: Option<String>,
    pub referring_physician: Option<String>,
    pub requesting_physician: Option<String>,
    pub institution: Option<String>,
    pub other_patient_ids: Option<String>,
    /// Studies only: one exact StudyInstanceUID (links from other programs).
    pub study_uid: Option<String>,
    /// Studies only: labels separated by commas; `labels_mode` = `any` (default) or `all`.
    pub labels: Option<String>,
    pub labels_mode: Option<String>,
    /// Studies only: `true` = only the studies that have no label at all.
    pub no_labels: Option<bool>,
    /// Column to sort by (see `study_order` / `series_order`) and `asc` / `desc`.
    pub sort: Option<String>,
    pub dir: Option<String>,
    /// Page size (1-100, default 25) and number of results to skip.
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    /// The Orthanc servers to search, by key, separated by commas (empty = the default ones).
    pub servers: Option<String>,
}

/// One page of results. `has_more` lets the UI show a "Next page" button without
/// an expensive total count.
#[derive(Serialize)]
pub struct Page<T> {
    items: Vec<T>,
    has_more: bool,
    offset: usize,
    limit: usize,
    /// The servers searched, by key.
    servers: Vec<String>,
    /// The servers that did not answer: the results come from the others.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    unavailable: Vec<Unavailable>,
}

/// A server that did not answer a search.
#[derive(Serialize)]
pub struct Unavailable {
    server: String,
    label: String,
    error: String,
}

/// How long one server may take to answer a search over several servers.
pub const SERVER_TIMEOUT: Duration = Duration::from_secs(10);

/// A search over several servers merges their lists from the first result: pages that start
/// further than this are refused (narrow the search instead).
pub const MAX_MERGED_OFFSET: usize = 5000;

/// The servers a search asks: those named in `servers=` (each must be one this person may use,
/// otherwise it is "unknown"), or those searched by default (all of them if none is).
pub fn chosen_servers(
    state: &AppState,
    user: &crate::db::User,
    raw: &Option<String>,
) -> AppResult<Vec<(ServerInfo, OrthancClient)>> {
    let usable = state.orthancs.usable(user);
    if usable.is_empty() {
        return Err(AppError::Denied("Your account cannot use any Orthanc server".into()));
    }
    let wanted: Vec<&str> =
        raw.as_deref().unwrap_or_default().split(',').map(str::trim).filter(|k| !k.is_empty()).collect();
    if wanted.is_empty() {
        let defaults: Vec<_> = usable.iter().filter(|(s, _)| s.searched_by_default).cloned().collect();
        return Ok(if defaults.is_empty() { usable } else { defaults });
    }
    if let Some(unknown) = wanted.iter().find(|key| !usable.iter().any(|(s, _)| s.key == **key)) {
        return Err(AppError::BadRequest(format!("Unknown Orthanc server '{unknown}'")));
    }
    // In the configured order, whatever the order of the request.
    Ok(usable.into_iter().filter(|(s, _)| wanted.contains(&s.key.as_str())).collect())
}

/// One server's answer, or why it gave none.
type Answer = (ServerInfo, AppResult<Vec<Value>>);

/// The rows of each server that answered.
type Lists = Vec<(ServerInfo, Vec<Value>)>;

/// Asks every server at the same time. `body` builds each server's request (the labels of
/// "without labels" differ from one server to the next). With several servers, each has at
/// most [`SERVER_TIMEOUT`].
async fn ask_all<'a, F, Fut>(servers: &'a [(ServerInfo, OrthancClient)], body: F) -> Vec<Answer>
where
    F: Fn(&'a OrthancClient) -> Fut,
    Fut: std::future::Future<Output = AppResult<Value>> + 'a,
{
    let several = servers.len() > 1;
    let body = &body;
    futures_util::future::join_all(servers.iter().map(|(info, client)| async move {
        let ask = async {
            let request = body(client).await?;
            client.find(&request).await
        };
        let result = if several {
            match tokio::time::timeout(SERVER_TIMEOUT, ask).await {
                Ok(result) => result,
                Err(_) => {
                    Err(AppError::Upstream(format!("did not answer within {} seconds", SERVER_TIMEOUT.as_secs())))
                }
            }
        } else {
            ask.await
        };
        (info.clone(), result)
    }))
    .await
}

/// The answers that came, and the servers that failed. When none answered, the first error.
fn split_answers(answers: Vec<Answer>) -> AppResult<(Lists, Vec<Unavailable>)> {
    let mut lists = Vec::new();
    let mut unavailable = Vec::new();
    let mut first_error = None;
    for (info, result) in answers {
        match result {
            Ok(rows) => lists.push((info, rows)),
            Err(e) => {
                tracing::warn!("search: Orthanc server '{}' did not answer: {e}", info.key);
                unavailable.push(Unavailable {
                    server: info.key.clone(),
                    label: info.label.clone(),
                    error: e.to_string(),
                });
                first_error = first_error.or(Some(e));
            }
        }
    }
    match (lists.is_empty(), first_error) {
        (true, Some(e)) => Err(e),
        _ => Ok((lists, unavailable)),
    }
}

/// The `Limit` / `Since` each server is asked for: the page itself with one server; with
/// several, everything from the start up to the end of the page (plus one, for "has more").
fn window(servers: usize, offset: usize, limit: usize) -> AppResult<(usize, usize)> {
    if servers <= 1 {
        return Ok((limit + 1, offset));
    }
    if offset > MAX_MERGED_OFFSET {
        return Err(AppError::BadRequest(format!(
            "Searching several Orthanc servers at once shows the first {MAX_MERGED_OFFSET} results: narrow the search \
             (dates, modality...) or search one server"
        )));
    }
    Ok((offset + limit + 1, 0))
}

/// A row of a merged search: the first copy found (by server order) and every server holding it.
#[derive(Debug)]
pub struct Merged {
    pub value: Value,
    /// Index (in the servers asked) of the server `value` comes from.
    pub server: usize,
    /// Every server holding it, in order.
    pub servers: Vec<usize>,
    /// Orthanc's ID of the copy on each of `servers` (the same order). Usually the same ID
    /// everywhere, but a copy whose patient ID was changed has another one.
    pub ids: Vec<String>,
}

/// Merges sorted lists (one per server, in server order) into one sorted list, the same item
/// (same `identity`) on several servers becoming one row. `requested` is how many rows each
/// server was asked for: a list shorter than that is complete. Merging stops when a list that
/// is NOT complete runs out (what follows it is unknown), so every row given is in its place.
pub fn merge(
    lists: Vec<Vec<Value>>,
    requested: usize,
    order: &dyn Fn(&Value, &Value) -> Ordering,
    identity: &dyn Fn(&Value) -> String,
) -> Vec<Merged> {
    let complete: Vec<bool> = lists.iter().map(|l| l.len() < requested).collect();
    let mut queues: Vec<std::collections::VecDeque<Value>> = lists.into_iter().map(Into::into).collect();
    let mut rows: Vec<Merged> = Vec::new();
    let mut by_identity: HashMap<String, usize> = HashMap::new();
    loop {
        if queues.iter().zip(&complete).any(|(q, done)| q.is_empty() && !done) {
            break;
        }
        // The smallest head; on equal values, the first server.
        let mut best: Option<usize> = None;
        for (i, q) in queues.iter().enumerate() {
            let Some(head) = q.front() else { continue };
            let better = match best {
                None => true,
                Some(b) => order(head, queues[b].front().expect("non-empty")) == Ordering::Less,
            };
            if better {
                best = Some(i);
            }
        }
        let Some(i) = best else { break };
        let value = queues[i].pop_front().expect("non-empty");
        let key = identity(&value);
        match by_identity.get(&key).copied().filter(|_| !key.is_empty()) {
            Some(row) => {
                let row = &mut rows[row];
                if !row.servers.contains(&i) {
                    let at = row.servers.partition_point(|&s| s < i);
                    row.servers.insert(at, i);
                    row.ids.insert(at, orthanc_id(&value));
                }
                // The copy of the first server is the one the row's actions use.
                if i < row.server {
                    row.value = value;
                    row.server = i;
                }
            }
            None => {
                by_identity.insert(key, rows.len());
                let ids = vec![orthanc_id(&value)];
                rows.push(Merged { value, server: i, servers: vec![i], ids });
            }
        }
    }
    rows
}

/// The comparison of two rows following an Orthanc `OrderBy` (the one the servers sorted by):
/// DICOM tags as text, `LastUpdate` as the metadata Orthanc returns with expanded resources.
pub fn comparator(order_by: Option<&Value>) -> impl Fn(&Value, &Value) -> Ordering {
    let keys: Vec<(String, bool, bool)> = order_by
        .and_then(Value::as_array)
        .map(|keys| {
            keys.iter()
                .map(|k| {
                    let metadata = k["Type"] == "Metadata";
                    (k["Key"].as_str().unwrap_or_default().to_string(), metadata, k["Direction"] == "DESC")
                })
                .collect()
        })
        .unwrap_or_default();
    move |a: &Value, b: &Value| {
        for (key, metadata, descending) in &keys {
            let read = |v: &Value| -> String {
                let found = if *metadata {
                    &v[key.as_str()]
                } else {
                    [&v["MainDicomTags"], &v["PatientMainDicomTags"], &v["RequestedTags"]]
                        .into_iter()
                        .map(|tags| &tags[key.as_str()])
                        .find(|t| !t.is_null())
                        .unwrap_or(&Value::Null)
                };
                found.as_str().unwrap_or_default().to_string()
            };
            let ordering = read(a).cmp(&read(b));
            let ordering = if *descending { ordering.reverse() } else { ordering };
            if ordering != Ordering::Equal {
                return ordering;
            }
        }
        Ordering::Equal
    }
}

/// The page of a search: `(rows, has_more)`. One server: its rows as they are. Several: merged,
/// then the page cut out.
fn page_of(
    lists: Vec<Vec<Value>>,
    requested: usize,
    offset: usize,
    limit: usize,
    order: &dyn Fn(&Value, &Value) -> Ordering,
    identity: &dyn Fn(&Value) -> String,
) -> (Vec<Merged>, bool) {
    if lists.len() == 1 {
        let mut rows: Vec<Merged> = lists
            .into_iter()
            .next()
            .unwrap_or_default()
            .into_iter()
            .map(|value| Merged { ids: vec![orthanc_id(&value)], value, server: 0, servers: vec![0] })
            .collect();
        let has_more = rows.len() > limit;
        rows.truncate(limit);
        return (rows, has_more);
    }
    let merged = merge(lists, requested, order, identity);
    let has_more = merged.len() > offset + limit;
    (merged.into_iter().skip(offset).take(limit).collect(), has_more)
}

/// The keys of the servers of a merged row.
fn server_keys(row: &Merged, servers: &[ServerInfo]) -> Vec<String> {
    row.servers.iter().map(|&i| servers[i].key.clone()).collect()
}

/// Orthanc's own ID of a found resource.
fn orthanc_id(value: &Value) -> String {
    value["ID"].as_str().unwrap_or_default().to_string()
}

/// The copies of a study found on several servers: `{ server, id }` each, the ID written for
/// its server. Empty for a study on one server.
fn copies_of(row: &Merged, servers: &[ServerInfo]) -> Vec<crate::models::Copy> {
    if row.servers.len() < 2 {
        return Vec::new();
    }
    row.servers
        .iter()
        .zip(&row.ids)
        .map(|(&i, id)| crate::models::Copy {
            server: servers[i].key.clone(),
            id: crate::servers::compound(&servers[i].key, id),
        })
        .collect()
}

/// Clamps the requested page size and defaults the offset.
fn paging(p: &SearchParams) -> (usize, usize) {
    (p.limit.unwrap_or(25).clamp(1, 100), p.offset.unwrap_or(0))
}

/// Turns `"ct, rx"` into the DICOM "any of" list `"CT\\CR\\DX"`.
///
/// Every code is validated (letters/digits only), so the value can never carry
/// wildcards or anything unexpected into the query.
fn modality_list(raw: &Option<String>) -> AppResult<Option<String>> {
    let Some(raw) = non_empty(raw.clone()) else {
        return Ok(None);
    };
    let mut codes: Vec<String> = Vec::new();
    for code in raw.split(',').map(|c| c.trim().to_uppercase()).filter(|c| !c.is_empty()) {
        if code.len() > 16 || !code.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(AppError::BadRequest(format!("Invalid modality '{code}'")));
        }
        // "RX" is how X-ray is called in many countries; in DICOM it is CR or DX.
        let expanded: Vec<String> = if code == "RX" { vec!["CR".into(), "DX".into()] } else { vec![code] };
        for c in expanded {
            if !codes.contains(&c) {
                codes.push(c);
            }
        }
    }
    if codes.len() > 20 {
        return Err(AppError::BadRequest("Too many modalities selected".into()));
    }
    // A single backslash is the DICOM value separator (JSON-escaped on the wire).
    Ok(if codes.is_empty() { None } else { Some(codes.join("\\")) })
}

/// Builds the DICOM date-range string. `20240101-20240131`, `20240101-` or `-20240131`.
fn date_range(from: &Option<String>, to: &Option<String>) -> AppResult<Option<String>> {
    let from = non_empty(from.clone()).map(|d| dicom_date(&d)).transpose()?;
    let to = non_empty(to.clone()).map(|d| dicom_date(&d)).transpose()?;
    if from.is_none() && to.is_none() {
        return Ok(None);
    }
    Ok(Some(format!("{}-{}", from.unwrap_or_default(), to.unwrap_or_default())))
}

/// Inserts `key: *value*` into the Orthanc query when the user filled the field.
fn add_pattern(query: &mut Map<String, Value>, key: &str, value: &Option<String>) {
    if let Some(v) = non_empty(value.clone()) {
        query.insert(key.into(), json!(contains_pattern(&v)));
    }
}

/// Inserts an exact-match criterion (body part).
fn add_exact(query: &mut Map<String, Value>, key: &str, value: &Option<String>) {
    if let Some(v) = non_empty(value.clone()) {
        query.insert(key.into(), json!(v));
    }
}

/// `asc` / `desc` (or the default when not given).
fn direction(dir: &Option<String>, default: &'static str) -> AppResult<&'static str> {
    match dir.as_deref() {
        None | Some("") => Ok(default),
        Some("asc") => Ok("ASC"),
        Some("desc") => Ok("DESC"),
        Some(other) => Err(AppError::BadRequest(format!("Invalid sort direction '{other}': use asc or desc"))),
    }
}

/// How to sort studies. Every column with a DICOM value can be sorted on (the database sorts, so
/// paging stays consistent); the study date breaks ties.
fn study_order(p: &SearchParams) -> AppResult<Value> {
    let newest = |dir: &str| {
        json!([
            { "Type": "DicomTag", "Key": "StudyDate", "Direction": dir },
            { "Type": "DicomTag", "Key": "StudyTime", "Direction": dir }
        ])
    };
    let key =
        non_empty(p.sort.clone()).or_else(|| (p.order.as_deref() == Some("received")).then(|| "received".to_string()));
    let tag = match key.as_deref() {
        None | Some("study_date") => return Ok(newest(direction(&p.dir, "DESC")?)),
        // "LastUpdate" is the metadata Orthanc updates whenever an instance of the study arrives.
        Some("received") => {
            return Ok(json!([{ "Type": "Metadata", "Key": "LastUpdate", "Direction": direction(&p.dir, "DESC")? }]))
        }
        Some("patient") => "PatientName",
        Some("patient_id") => "PatientID",
        Some("birth_date") => "PatientBirthDate",
        Some("sex") => "PatientSex",
        Some("other_ids") => "OtherPatientIDs",
        Some("description") => "StudyDescription",
        Some("accession") => "AccessionNumber",
        Some("referring") => "ReferringPhysicianName",
        Some("requesting") => "RequestingPhysician",
        Some("institution") => "InstitutionName",
        Some(other) => return Err(AppError::BadRequest(format!("The list cannot be sorted by '{other}'"))),
    };
    let mut order = vec![json!({ "Type": "DicomTag", "Key": tag, "Direction": direction(&p.dir, "ASC")? })];
    order.extend(newest("DESC").as_array().cloned().unwrap_or_default());
    Ok(Value::Array(order))
}

/// How to sort series: `None` = Orthanc's own order (as before sorting existed).
fn series_order(p: &SearchParams) -> AppResult<Option<Value>> {
    let (tag, default) = match non_empty(p.sort.clone()).as_deref() {
        None => return Ok(None),
        Some("study_date") => ("StudyDate", "DESC"),
        Some("patient") => ("PatientName", "ASC"),
        Some("number") => ("SeriesNumber", "ASC"),
        Some("modality") => ("Modality", "ASC"),
        Some("description") => ("SeriesDescription", "ASC"),
        Some("body_part") => ("BodyPartExamined", "ASC"),
        Some(other) => return Err(AppError::BadRequest(format!("The list cannot be sorted by '{other}'"))),
    };
    Ok(Some(json!([{ "Type": "DicomTag", "Key": tag, "Direction": direction(&p.dir, default)? }])))
}

/// The label part of a studies search.
enum LabelFilter {
    None,
    /// `(labels, "Any" | "All")`.
    Some(Vec<String>, &'static str),
    /// Only the studies without any label: "none of the labels" of each server.
    Without,
}

/// What the label fields of a search ask for.
fn label_filter(state: &AppState, p: &SearchParams) -> AppResult<LabelFilter> {
    if !state.settings.get().labels_enabled {
        return Ok(LabelFilter::None); // switched off: the label filters of old links are ignored
    }
    let wanted: Vec<String> = p
        .labels
        .as_deref()
        .unwrap_or_default()
        .split(',')
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    if p.no_labels == Some(true) {
        if !wanted.is_empty() {
            return Err(AppError::BadRequest("Choose some labels or \"without labels\", not both".into()));
        }
        return Ok(LabelFilter::Without);
    }
    if wanted.is_empty() {
        return Ok(LabelFilter::None);
    }
    if wanted.len() > 20 || wanted.iter().any(|l| !crate::routes::labels::valid_label(l)) {
        return Err(AppError::BadRequest("Invalid labels".into()));
    }
    let mode = match p.labels_mode.as_deref() {
        None | Some("") | Some("any") => "Any",
        Some("all") => "All",
        Some(other) => return Err(AppError::BadRequest(format!("Invalid labels mode '{other}': use any or all"))),
    };
    Ok(LabelFilter::Some(wanted, mode))
}

/// Adds the label filter to one server's search.
async fn with_labels(mut body: Value, filter: &LabelFilter, orthanc: &OrthancClient) -> AppResult<Value> {
    match filter {
        LabelFilter::None => {}
        LabelFilter::Some(labels, constraint) => {
            body["Labels"] = json!(labels);
            body["LabelsConstraint"] = json!(constraint);
        }
        LabelFilter::Without => {
            // "None of every label this server knows" = without any label.
            let all = crate::routes::labels::server_labels(orthanc).await?;
            if !all.is_empty() {
                body["Labels"] = json!(all);
                body["LabelsConstraint"] = json!("None");
            }
        }
    }
    Ok(body)
}

// ------------------------------------------------------------------ studies

/// Computed tags that are not part of an expanded study itself (modalities, counts).
const STUDY_TAGS: [&str; 3] = ["ModalitiesInStudy", "NumberOfStudyRelatedInstances", "NumberOfStudyRelatedSeries"];

pub async fn search_studies(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Query(p): Query<SearchParams>,
) -> AppResult<Json<Page<StudySummary>>> {
    user.require(Permission::Read)?;
    let (limit, offset) = paging(&p);

    // Translate the form fields into DICOM tags.
    let mut query = Map::new();
    add_pattern(&mut query, "PatientName", &p.patient_name);
    add_pattern(&mut query, "PatientID", &p.patient_id);
    add_pattern(&mut query, "AccessionNumber", &p.accession_number);
    add_pattern(&mut query, "StudyDescription", &p.description);
    if let Some(modalities) = modality_list(&p.modality)? {
        query.insert("ModalitiesInStudy".into(), json!(modalities));
    }
    if let Some(range) = date_range(&p.date_from, &p.date_to)? {
        query.insert("StudyDate".into(), json!(range));
    }
    add_pattern(&mut query, "ReferringPhysicianName", &p.referring_physician);
    add_pattern(&mut query, "RequestingPhysician", &p.requesting_physician);
    add_pattern(&mut query, "InstitutionName", &p.institution);
    add_pattern(&mut query, "OtherPatientIDs", &p.other_patient_ids);
    if let Some(range) = date_range(&p.birth_from, &p.birth_to)? {
        query.insert("PatientBirthDate".into(), json!(range));
    }
    if let Some(sex) = non_empty(p.patient_sex.clone()) {
        let sex = sex.to_uppercase();
        if !["M", "F", "O"].contains(&sex.as_str()) {
            return Err(AppError::BadRequest("The sex must be M, F or O".into()));
        }
        query.insert("PatientSex".into(), json!(sex));
    }
    if let Some(uid) = non_empty(p.study_uid.clone()) {
        if !is_valid_dicom_uid(&uid) {
            return Err(AppError::BadRequest("Invalid StudyInstanceUID".into()));
        }
        query.insert("StudyInstanceUID".into(), json!(uid));
    }

    let order = study_order(&p)?;
    let labels = label_filter(&state, &p)?;
    let servers = chosen_servers(&state, &user, &p.servers)?;
    // One extra row tells whether another page exists.
    let (requested, since) = window(servers.len(), offset, limit)?;
    let body = json!({
        "Level": "Study",
        "Query": query,
        "Expand": true,
        "RequestedTags": STUDY_TAGS,
        "OrderBy": order,
        "Limit": requested,
        "Since": since,
    });
    let answers = ask_all(&servers, |client| with_labels(body.clone(), &labels, client)).await;
    let (lists, unavailable) = split_answers(answers)?;
    let infos: Vec<ServerInfo> = lists.iter().map(|(info, _)| info.clone()).collect();
    let lists: Vec<Vec<Value>> = lists.into_iter().map(|(_, rows)| rows).collect();
    let identity = |v: &Value| v["MainDicomTags"]["StudyInstanceUID"].as_str().unwrap_or_default().to_string();
    let (rows, has_more) = page_of(lists, requested, offset, limit, &comparator(Some(&order)), &identity);

    let items = rows
        .iter()
        .map(|row| {
            let mut study = StudySummary::from_orthanc(&row.value).on_server(&infos[row.server].key);
            study.servers = server_keys(row, &infos);
            study.copies = copies_of(row, &infos);
            study
        })
        .collect();
    let searched = servers.iter().map(|(s, _)| s.key.clone()).collect();
    Ok(Json(Page { items, has_more, offset, limit, servers: searched, unavailable }))
}

/// `GET /api/stats`: how many studies the PACS holds (header of the simplified view): in all and
/// on each server this person may use. A server that does not answer is left out of the total
/// (and named in `unavailable`), unless none answers.
pub async fn statistics(State(state): State<AppState>, AuthUser(user): AuthUser) -> AppResult<Json<Value>> {
    user.require(Permission::Read)?;
    let servers = state.orthancs.usable(&user);
    let answers = futures_util::future::join_all(servers.iter().map(|(info, client)| async move {
        let answer = tokio::time::timeout(SERVER_TIMEOUT, client.get_json("/statistics")).await;
        let answer = answer.unwrap_or_else(|_| Err(AppError::Upstream("did not answer in time".into())));
        (info, answer)
    }))
    .await;
    let mut total: Option<u64> = None;
    let mut per_server = Vec::new();
    let mut unavailable = Vec::new();
    let mut first_error = None;
    for (info, answer) in answers {
        match answer {
            Ok(stats) => {
                let count = stats["CountStudies"].as_u64();
                total = match (total, count) {
                    (Some(t), Some(c)) => Some(t + c),
                    (t, c) => t.or(c),
                };
                per_server.push(json!({ "key": info.key, "label": info.label, "studies": count }));
            }
            Err(e) => {
                unavailable.push(json!({ "server": info.key, "label": info.label, "error": e.to_string() }));
                first_error = first_error.or(Some(e));
            }
        }
    }
    if let (true, Some(e)) = (per_server.is_empty(), first_error) {
        return Err(e);
    }
    // With several servers, a study on more than one of them counts once in the total (the sum
    // when a server could not list its studies).
    if per_server.len() > 1 && unavailable.is_empty() {
        if let Some(distinct) = state.orthancs.distinct_studies(&servers).await {
            total = Some(distinct);
        }
    }
    let mut answer = json!({ "studies": total, "servers": per_server });
    if !unavailable.is_empty() {
        answer["unavailable"] = json!(unavailable);
    }
    Ok(Json(answer))
}

/// Series of one study, ordered by series number.
pub async fn study_series(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Vec<SeriesSummary>>> {
    user.require(Permission::Read)?;
    let study = ResourceRef::parse(&id)?;
    let value = state.orthancs.of(&user, &study)?.get_json(&format!("/studies/{}/series", study.id)).await?;
    let mut series: Vec<SeriesSummary> = value
        .as_array()
        .map(|items| items.iter().map(|s| SeriesSummary::from_orthanc(s).on_server(&study.server)).collect())
        .unwrap_or_default();
    // SeriesNumber is a string in DICOM: sort it as a number ("2" before "10").
    series.sort_by_key(|s| s.series_number.parse::<i64>().unwrap_or(i64::MAX));
    Ok(Json(series))
}

// ------------------------------------------------------------------- series

pub async fn search_series(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Query(p): Query<SearchParams>,
) -> AppResult<Json<Page<SeriesSummary>>> {
    user.require(Permission::Read)?;
    let (limit, offset) = paging(&p);

    // At series level Orthanc also accepts patient- and study-level tags.
    let mut query = Map::new();
    add_pattern(&mut query, "PatientName", &p.patient_name);
    add_pattern(&mut query, "PatientID", &p.patient_id);
    add_pattern(&mut query, "AccessionNumber", &p.accession_number);
    add_pattern(&mut query, "SeriesDescription", &p.description);
    add_exact(&mut query, "BodyPartExamined", &p.body_part);
    if let Some(modalities) = modality_list(&p.modality)? {
        query.insert("Modality".into(), json!(modalities));
    }
    if let Some(range) = date_range(&p.date_from, &p.date_to)? {
        query.insert("StudyDate".into(), json!(range));
    }

    let servers = chosen_servers(&state, &user, &p.servers)?;
    let (requested, since) = window(servers.len(), offset, limit)?;
    let mut body = json!({
        "Level": "Series",
        "Query": query,
        "Expand": true,
        // The parent study's UID (a main tag Orthanc keeps in its database): lets the parent
        // studies of the whole page be read in one search below.
        "RequestedTags": ["StudyInstanceUID"],
        "Limit": requested,
        "Since": since,
    });
    let order = series_order(&p)?;
    if let Some(order) = &order {
        body["OrderBy"] = order.clone();
    }
    let answers = ask_all(&servers, |_| {
        let body = body.clone();
        async move { Ok(body) }
    })
    .await;
    let (lists, unavailable) = split_answers(answers)?;
    let infos: Vec<ServerInfo> = lists.iter().map(|(info, _)| info.clone()).collect();
    let lists: Vec<Vec<Value>> = lists.into_iter().map(|(_, rows)| rows).collect();
    let identity = |v: &Value| v["MainDicomTags"]["SeriesInstanceUID"].as_str().unwrap_or_default().to_string();
    let (rows, has_more) = page_of(lists, requested, offset, limit, &comparator(order.as_ref()), &identity);

    // A lone series is meaningless without its patient/study: attach each one's parent, read
    // from the server each series comes from.
    let mut items: Vec<SeriesSummary> = Vec::with_capacity(rows.len());
    let mut parents: Vec<HashMap<String, StudySummary>> = Vec::with_capacity(infos.len());
    for (index, info) in infos.iter().enumerate() {
        let here: Vec<&Merged> = rows.iter().filter(|r| r.server == index).collect();
        if here.is_empty() {
            parents.push(HashMap::new());
            continue;
        }
        let mut uids: Vec<String> = here
            .iter()
            .filter_map(|r| r.value["RequestedTags"]["StudyInstanceUID"].as_str())
            .map(str::to_string)
            .collect();
        uids.sort();
        uids.dedup();
        let mut ids: Vec<String> =
            here.iter().map(|r| r.value["ParentStudy"].as_str().unwrap_or_default().to_string()).collect();
        ids.sort();
        ids.dedup();
        let client = servers.iter().find(|(s, _)| s.key == info.key).map(|(_, c)| c.clone());
        parents.push(match client {
            Some(client) => parent_studies(&client, ids, &uids).await,
            None => HashMap::new(),
        });
    }
    for row in &rows {
        let mut series = SeriesSummary::from_orthanc(&row.value);
        series.study = parents[row.server].get(&series.study_id).cloned();
        let mut series = series.on_server(&infos[row.server].key);
        series.servers = server_keys(row, &infos);
        items.push(series);
    }
    let searched = servers.iter().map(|(s, _)| s.key.clone()).collect();
    Ok(Json(Page { items, has_more, offset, limit, servers: searched, unavailable }))
}

/// The parent studies of a page of series, by Orthanc id. ONE study search for all of them (by the
/// StudyInstanceUIDs the series search returned: a page of 50 series used to cost up to 50 requests),
/// then one request per study that search did not bring (up to 8 at a time): an Orthanc that did not
/// return the UIDs, a study modified meanwhile. The same studies either way.
async fn parent_studies(orthanc: &OrthancClient, ids: Vec<String>, uids: &[String]) -> HashMap<String, StudySummary> {
    let mut studies = HashMap::new();
    if !uids.is_empty() && uids.iter().all(|uid| is_valid_dicom_uid(uid)) {
        let body = json!({
            "Level": "Study",
            // A backslash-separated list of UIDs is DICOM's "any of".
            "Query": { "StudyInstanceUID": uids.join("\\") },
            "Expand": true,
            "RequestedTags": STUDY_TAGS,
            "Limit": uids.len() + 5,
        });
        if let Ok(found) = orthanc.find(&body).await {
            for study in found.iter().map(StudySummary::from_orthanc) {
                if ids.contains(&study.id) {
                    studies.insert(study.id.clone(), study);
                }
            }
        }
    }
    let missing: Vec<String> = ids.into_iter().filter(|id| !studies.contains_key(id)).collect();
    let fetched: Vec<(String, StudySummary)> = stream::iter(missing)
        .map(|study_id| async move {
            let result = orthanc.get_json(&format!("/studies/{study_id}")).await;
            (study_id, result)
        })
        .buffer_unordered(8)
        .filter_map(|(study_id, result)| async move { result.ok().map(|v| (study_id, StudySummary::from_orthanc(&v))) })
        .collect()
        .await;
    studies.extend(fetched);
    studies
}

#[cfg(test)]
mod tests {
    use super::*;

    fn study(uid: &str, date: &str) -> Value {
        json!({ "ID": uid, "MainDicomTags": { "StudyInstanceUID": uid, "StudyDate": date, "StudyTime": "" } })
    }

    fn newest_first() -> Value {
        json!([
            { "Type": "DicomTag", "Key": "StudyDate", "Direction": "DESC" },
            { "Type": "DicomTag", "Key": "StudyTime", "Direction": "DESC" }
        ])
    }

    fn uid(v: &Value) -> String {
        v["MainDicomTags"]["StudyInstanceUID"].as_str().unwrap_or_default().to_string()
    }

    fn uids(rows: &[Merged]) -> Vec<String> {
        rows.iter().map(|r| uid(&r.value)).collect()
    }

    #[test]
    fn lists_are_merged_in_order_and_copies_become_one_row() {
        let order = newest_first();
        let main = vec![study("a", "20260105"), study("both", "20260103"), study("c", "20260101")];
        let archive = vec![study("d", "20260104"), study("both", "20260103"), study("e", "20250101")];
        // Every list is complete (fewer rows than asked for).
        let rows = merge(vec![main, archive], 10, &comparator(Some(&order)), &uid);
        assert_eq!(uids(&rows), ["a", "d", "both", "c", "e"]);
        let both = &rows[2];
        assert_eq!((both.server, both.servers.clone()), (0, vec![0, 1]), "the first server's copy");
    }

    #[test]
    fn the_copy_of_the_first_server_wins_even_when_found_later() {
        // Sorted by reception: the archive received its copy more recently.
        let order = json!([{ "Type": "Metadata", "Key": "LastUpdate", "Direction": "DESC" }]);
        let at = |uid: &str, when: &str| json!({ "ID": uid, "LastUpdate": when, "MainDicomTags": { "StudyInstanceUID": uid } });
        let main = vec![at("x", "20260101T000000")];
        let archive = vec![at("x", "20260301T000000"), at("y", "20260201T000000")];
        let rows = merge(vec![main, archive], 10, &comparator(Some(&order)), &uid);
        assert_eq!(uids(&rows), ["x", "y"]);
        assert_eq!((rows[0].server, rows[0].servers.clone()), (0, vec![0, 1]));
    }

    #[test]
    fn merging_stops_where_an_unfinished_list_runs_out() {
        let order = newest_first();
        // Both servers were asked for 2 rows and gave 2: there may be more after them.
        let main = vec![study("a", "20260110"), study("b", "20260109")];
        let archive = vec![study("c", "20260101"), study("d", "20250101")];
        let rows = merge(vec![main, archive], 2, &comparator(Some(&order)), &uid);
        // After "b" the main server's next row is unknown: it could come before "c".
        assert_eq!(uids(&rows), ["a", "b"]);
    }

    #[test]
    fn pages_of_several_servers() {
        let order = newest_first();
        let cmp = comparator(Some(&order));
        let main: Vec<Value> = (0..5).map(|i| study(&format!("m{i}"), &format!("2026010{}", 9 - i * 2))).collect();
        let archive: Vec<Value> = (0..5).map(|i| study(&format!("a{i}"), &format!("2026010{}", 8 - i * 2))).collect();
        // Page 2 (offset 2, limit 2): each server was asked for offset + limit + 1 = 5 rows.
        let (requested, since) = window(2, 2, 2).unwrap();
        assert_eq!((requested, since), (5, 0));
        let (rows, has_more) = page_of(vec![main, archive], requested, 2, 2, &cmp, &uid);
        assert_eq!(uids(&rows), ["m1", "a1"]);
        assert!(has_more);
        // One server: the page Orthanc gave, as it is.
        assert_eq!(window(1, 40, 20).unwrap(), (21, 40));
        assert!(window(2, MAX_MERGED_OFFSET + 1, 20).is_err(), "too deep");
    }

    /// Each copy keeps its own Orthanc ID (a copy whose patient ID was changed has another one).
    #[test]
    fn every_copy_keeps_its_own_id() {
        let order = newest_first();
        let mut moved = study("both", "20260103");
        moved["ID"] = json!("other-id");
        let rows = merge(vec![vec![study("both", "20260103")], vec![moved]], 10, &comparator(Some(&order)), &uid);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].servers.clone(), rows[0].ids.clone()),
            (vec![0, 1], vec!["both".to_string(), "other-id".to_string()])
        );
        let servers = [("main", "Main"), ("archive", "Archive")].map(|(key, label)| ServerInfo {
            key: key.into(),
            label: label.into(),
            color: String::new(),
            searched_by_default: true,
        });
        let copies = copies_of(&rows[0], &servers);
        assert_eq!(copies.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["both", "archive:other-id"]);
    }

    #[test]
    fn without_an_order_the_first_server_comes_first() {
        let rows = merge(vec![vec![study("a", "")], vec![study("b", "")]], 10, &comparator(None), &uid);
        assert_eq!(uids(&rows), ["a", "b"]);
    }
}
