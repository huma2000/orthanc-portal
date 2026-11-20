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
//! Searching is delegated to Orthanc's `POST /tools/find` (recent "extended find"
//! API). The `Query` object uses DICOM matching rules: `*` / `?` wildcards, date
//! ranges `YYYYMMDD-YYYYMMDD`, and **lists of values separated by a backslash**,
//! which DICOM defines as "any of": `"CT\\MR"` finds CT *or* MR.

use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use crate::models::{SeriesSummary, StudySummary};
use crate::permissions::Permission;
use crate::state::AppState;
use crate::util::{contains_pattern, dicom_date, is_valid_dicom_uid, is_valid_orthanc_id, non_empty};
use axum::extract::{Path, Query, State};
use axum::Json;
use futures_util::{stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::HashMap;

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
}

/// One page of results. `has_more` lets the UI show a "Next page" button without
/// an expensive total count.
#[derive(Serialize)]
pub struct Page<T> {
    items: Vec<T>,
    has_more: bool,
    offset: usize,
    limit: usize,
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

/// The label part of a studies search: `(labels, "Any" | "All" | "None")`, or nothing.
async fn label_filter(state: &AppState, p: &SearchParams) -> AppResult<Option<(Vec<String>, &'static str)>> {
    if !state.settings.get().labels_enabled {
        return Ok(None); // switched off: the label filters of old links are ignored
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
        // "None of every existing label" = without any label.
        let all = crate::routes::labels::all_labels(state).await?;
        return Ok(if all.is_empty() { None } else { Some((all, "None")) });
    }
    if wanted.is_empty() {
        return Ok(None);
    }
    if wanted.len() > 20 || wanted.iter().any(|l| !crate::routes::labels::valid_label(l)) {
        return Err(AppError::BadRequest("Invalid labels".into()));
    }
    let mode = match p.labels_mode.as_deref() {
        None | Some("") | Some("any") => "Any",
        Some("all") => "All",
        Some(other) => return Err(AppError::BadRequest(format!("Invalid labels mode '{other}': use any or all"))),
    };
    Ok(Some((wanted, mode)))
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
    let mut body = json!({
        "Level": "Study",
        "Query": query,
        "Expand": true,
        "RequestedTags": STUDY_TAGS,
        "OrderBy": order,
        "Limit": limit + 1, // ask for one extra row to know whether another page exists
        "Since": offset,
    });
    if let Some((labels, constraint)) = label_filter(&state, &p).await? {
        body["Labels"] = json!(labels);
        body["LabelsConstraint"] = json!(constraint);
    }

    let mut found = state.orthanc.find(&body).await?;
    let has_more = found.len() > limit;
    found.truncate(limit);

    let items = found.iter().map(StudySummary::from_orthanc).collect();
    Ok(Json(Page { items, has_more, offset, limit }))
}

/// `GET /api/stats`: how many studies the PACS holds (header of the simplified view).
pub async fn statistics(State(state): State<AppState>, AuthUser(user): AuthUser) -> AppResult<Json<Value>> {
    user.require(Permission::Read)?;
    let stats = state.orthanc.get_json("/statistics").await?;
    Ok(Json(json!({ "studies": stats["CountStudies"].as_u64() })))
}

/// Series of one study, ordered by series number.
pub async fn study_series(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Vec<SeriesSummary>>> {
    user.require(Permission::Read)?;
    if !is_valid_orthanc_id(&id) {
        return Err(AppError::BadRequest("Invalid study id".into()));
    }

    let value = state.orthanc.get_json(&format!("/studies/{id}/series")).await?;
    let mut series: Vec<SeriesSummary> =
        value.as_array().map(|items| items.iter().map(SeriesSummary::from_orthanc).collect()).unwrap_or_default();
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

    let mut body = json!({
        "Level": "Series",
        "Query": query,
        "Expand": true,
        // The parent study's UID (a main tag Orthanc keeps in its database): lets the parent
        // studies of the whole page be read in one search below.
        "RequestedTags": ["StudyInstanceUID"],
        "Limit": limit + 1,
        "Since": offset,
    });
    if let Some(order) = series_order(&p)? {
        body["OrderBy"] = order;
    }
    let mut found = state.orthanc.find(&body).await?;
    let has_more = found.len() > limit;
    found.truncate(limit);

    let mut uids: Vec<String> =
        found.iter().filter_map(|v| v["RequestedTags"]["StudyInstanceUID"].as_str()).map(str::to_string).collect();
    uids.sort();
    uids.dedup();
    let mut items: Vec<SeriesSummary> = found.iter().map(SeriesSummary::from_orthanc).collect();

    // A lone series is meaningless without its patient/study: attach each one's parent.
    let mut parent_ids: Vec<String> = items.iter().map(|s| s.study_id.clone()).collect();
    parent_ids.sort();
    parent_ids.dedup();
    let studies = parent_studies(&state, parent_ids, &uids).await;

    for series in &mut items {
        series.study = studies.get(&series.study_id).cloned();
    }
    Ok(Json(Page { items, has_more, offset, limit }))
}

/// The parent studies of a page of series, by Orthanc id. ONE study search for all of them (by the
/// StudyInstanceUIDs the series search returned: a page of 50 series used to cost up to 50 requests),
/// then one request per study that search did not bring (up to 8 at a time): an Orthanc that did not
/// return the UIDs, a study modified meanwhile. The same studies either way.
async fn parent_studies(state: &AppState, ids: Vec<String>, uids: &[String]) -> HashMap<String, StudySummary> {
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
        if let Ok(found) = state.orthanc.find(&body).await {
            for study in found.iter().map(StudySummary::from_orthanc) {
                if ids.contains(&study.id) {
                    studies.insert(study.id.clone(), study);
                }
            }
        }
    }
    let missing: Vec<String> = ids.into_iter().filter(|id| !studies.contains_key(id)).collect();
    let orthanc = &state.orthanc;
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
