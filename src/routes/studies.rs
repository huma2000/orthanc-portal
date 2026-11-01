//! # routes/studies.rs
//!
//! Read-only "consult" endpoints. All of them require the `read` permission and
//! are available both on the intranet and remotely.
//!
//! * `GET /api/studies?patient_name=&patient_id=&accession_number=&description=&modality=&date_from=&date_to=&limit=&offset=`
//! * `GET /api/studies/{id}/series`   series of one study
//! * `GET /api/series?...`            search directly at series level
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
use crate::util::{contains_pattern, dicom_date, is_valid_orthanc_id, non_empty};
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

// ------------------------------------------------------------------ studies

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

    let body = json!({
        "Level": "Study",
        "Query": query,
        "Expand": true,
        // Computed tags that are not part of the expanded study itself.
        "RequestedTags": ["ModalitiesInStudy", "NumberOfStudyRelatedInstances", "NumberOfStudyRelatedSeries"],
        // Newest studies first. The database sorts, so paging stays consistent.
        "OrderBy": [
            { "Type": "DicomTag", "Key": "StudyDate", "Direction": "DESC" },
            { "Type": "DicomTag", "Key": "StudyTime", "Direction": "DESC" }
        ],
        "Limit": limit + 1, // ask for one extra row to know whether another page exists
        "Since": offset,
    });

    let mut found = state.orthanc.find(&body).await?;
    let has_more = found.len() > limit;
    found.truncate(limit);

    let items = found.iter().map(StudySummary::from_orthanc).collect();
    Ok(Json(Page { items, has_more, offset, limit }))
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

    let body = json!({
        "Level": "Series",
        "Query": query,
        "Expand": true,
        "Limit": limit + 1,
        "Since": offset,
    });
    let mut found = state.orthanc.find(&body).await?;
    let has_more = found.len() > limit;
    found.truncate(limit);

    let mut items: Vec<SeriesSummary> = found.iter().map(SeriesSummary::from_orthanc).collect();

    // A lone series is meaningless without its patient/study, so fetch each
    // distinct parent study once (up to 8 requests in parallel) and attach it.
    let mut parent_ids: Vec<String> = items.iter().map(|s| s.study_id.clone()).collect();
    parent_ids.sort();
    parent_ids.dedup();

    let orthanc = &state.orthanc;
    let studies: HashMap<String, StudySummary> = stream::iter(parent_ids)
        .map(|study_id| async move {
            let result = orthanc.get_json(&format!("/studies/{study_id}")).await;
            (study_id, result)
        })
        .buffer_unordered(8)
        .filter_map(|(study_id, result)| async move { result.ok().map(|v| (study_id, StudySummary::from_orthanc(&v))) })
        .collect()
        .await;

    for series in &mut items {
        series.study = studies.get(&series.study_id).cloned();
    }
    Ok(Json(Page { items, has_more, offset, limit }))
}
