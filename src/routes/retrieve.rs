//! # routes/retrieve.rs
//!
//! **Query / retrieve**: search the studies of another system (a DICOM node with C-FIND, or a
//! DICOMweb server with QIDO-RS) and bring them into this archive (C-MOVE, or WADO-RS through
//! Orthanc's DICOMweb client). Intranet listener only; switched on in Settings > Features; needs
//! the `retrieve` permission (or administrator). Every retrieval shows in the Transfers list.
//!
//! * `GET  /api/retrieve/sources`  the systems that can be searched (`{ id, kind, aet, host, port, url, description }`)
//! * `POST /api/retrieve/query`    `{ source, patient_name, patient_id, accession, description, modality, date_from, date_to }`
//!   -> the studies found there, each with `local_id` when it is already in this archive
//! * `POST /api/retrieve/retrieve` `{ source, study_uid, label }` -> starts bringing that study in

use crate::auth::AuthUser;
use crate::db::User;
use crate::error::{AppError, AppResult};
use crate::models::ModalityNode;
use crate::permissions::Permission;
use crate::state::AppState;
use crate::util::feature;
use crate::util::{contains_pattern, dicom_date, is_valid_dicom_uid, non_empty};
use axum::extract::State;
use axum::Json;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde::Deserialize;
use serde_json::{json, Map, Value};

fn allowed(state: &AppState, user: &User) -> AppResult<()> {
    user.require_or_admin(Permission::Retrieve)?;
    feature(state.settings.get().query_retrieve, "Query / retrieve is switched off")
}

/// DICOM nodes that answer C-FIND, and the DICOMweb servers of Orthanc.
async fn sources(state: &AppState) -> AppResult<Vec<ModalityNode>> {
    let mut nodes: Vec<ModalityNode> = state
        .orthanc
        .modalities_raw()
        .await?
        .iter()
        .filter(|(_, cfg)| cfg["AllowFind"].as_bool() != Some(false))
        .map(|(id, cfg)| if cfg.is_null() { ModalityNode::bare(id) } else { ModalityNode::from_orthanc(id, cfg) })
        .collect();
    nodes.sort_by(|a, b| a.id.cmp(&b.id));
    let servers = state.orthanc.named_targets("/dicom-web/servers").await;
    nodes.extend(servers.iter().map(|(name, cfg)| crate::routes::send::named_target("dicomweb", name, cfg)));
    let descriptions = state.db.node_descriptions()?;
    for node in &mut nodes {
        node.description = descriptions.get(&node.id).cloned().unwrap_or_default();
    }
    Ok(nodes)
}

pub async fn list_sources(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
) -> AppResult<Json<Vec<ModalityNode>>> {
    allowed(&state, &user)?;
    Ok(Json(sources(&state).await?))
}

async fn find_source(state: &AppState, id: &str) -> AppResult<ModalityNode> {
    sources(state).await?.into_iter().find(|n| n.id == id).ok_or_else(|| AppError::BadRequest("Unknown system".into()))
}

/// The name of a source in Orthanc's REST paths (an alias, or `dicomweb:<name>` without its prefix).
fn path_name(node: &ModalityNode) -> String {
    let name = node.id.split_once(':').map(|(_, n)| n).unwrap_or(&node.id);
    utf8_percent_encode(name, NON_ALPHANUMERIC).to_string()
}

#[derive(Deserialize)]
pub struct QueryRequest {
    source: String,
    patient_name: Option<String>,
    patient_id: Option<String>,
    accession: Option<String>,
    description: Option<String>,
    modality: Option<String>,
    date_from: Option<String>,
    date_to: Option<String>,
}

/// The values asked for, checked: no control characters, short; dates as a DICOM range.
struct Criteria {
    name: Option<String>,
    patient_id: Option<String>,
    accession: Option<String>,
    description: Option<String>,
    modality: Option<String>,
    dates: Option<String>,
}

fn criteria(req: &QueryRequest) -> AppResult<Criteria> {
    let clean = |v: &Option<String>| -> AppResult<Option<String>> {
        match non_empty(v.clone()) {
            Some(v) if v.chars().count() > 64 || v.chars().any(|c| c.is_control() || c == '\\') => {
                Err(AppError::BadRequest("A search value is too long or has a forbidden character".into()))
            }
            other => Ok(other.map(|v| v.trim().to_string())),
        }
    };
    let modality = clean(&req.modality)?.map(|m| m.to_uppercase());
    if modality.as_deref().is_some_and(|m| !m.chars().all(|c| c.is_ascii_alphanumeric())) {
        return Err(AppError::BadRequest("Invalid modality".into()));
    }
    let from = non_empty(req.date_from.clone()).map(|d| dicom_date(&d)).transpose()?;
    let to = non_empty(req.date_to.clone()).map(|d| dicom_date(&d)).transpose()?;
    let dates = if from.is_none() && to.is_none() {
        None
    } else {
        Some(format!("{}-{}", from.unwrap_or_default(), to.unwrap_or_default()))
    };
    let c = Criteria {
        name: clean(&req.patient_name)?,
        patient_id: clean(&req.patient_id)?,
        accession: clean(&req.accession)?,
        description: clean(&req.description)?,
        modality,
        dates,
    };
    if c.name.is_none()
        && c.patient_id.is_none()
        && c.accession.is_none()
        && c.description.is_none()
        && c.dates.is_none()
    {
        return Err(AppError::BadRequest(
            "Fill in at least a name, an ID, an accession number, a description or a date".into(),
        ));
    }
    Ok(c)
}

/// One study found on the other system.
/// `[study_uid, patient_name, patient_id, study_date, description, accession, modalities, images]`
fn answer(values: [&str; 8]) -> Value {
    let [uid, name, pid, date, desc, acc, mods, images] = values;
    json!({
        "study_uid": uid, "patient_name": name, "patient_id": pid, "study_date": date, "description": desc,
        "accession": acc, "modalities": mods.split('\\').filter(|m| !m.is_empty()).collect::<Vec<_>>(),
        "images": images, "local_id": Value::Null,
    })
}

/// C-FIND through Orthanc: `POST /modalities/{alias}/query`, then the answers.
async fn dicom_query(state: &AppState, node: &ModalityNode, c: &Criteria) -> AppResult<Vec<Value>> {
    let mut q = Map::new();
    let put = |q: &mut Map<String, Value>, key: &str, v: &Option<String>| {
        q.insert(key.into(), json!(v.clone().unwrap_or_default()));
    };
    q.insert("PatientName".into(), json!(c.name.as_deref().map(contains_pattern).unwrap_or_default()));
    put(&mut q, "PatientID", &c.patient_id);
    put(&mut q, "AccessionNumber", &c.accession);
    q.insert("StudyDescription".into(), json!(c.description.as_deref().map(contains_pattern).unwrap_or_default()));
    put(&mut q, "ModalitiesInStudy", &c.modality);
    put(&mut q, "StudyDate", &c.dates);
    for key in ["StudyInstanceUID", "PatientBirthDate", "NumberOfStudyRelatedInstances"] {
        q.insert(key.into(), json!(""));
    }
    let started = state
        .orthanc
        .post_json(&format!("/modalities/{}/query", path_name(node)), &json!({ "Level": "Study", "Query": q }))
        .await?;
    let query_id = started["ID"].as_str().unwrap_or("");
    if query_id.is_empty() || !query_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(AppError::Upstream("Orthanc did not return the query".into()));
    }
    let answers = state.orthanc.get_json(&format!("/queries/{query_id}/answers?expand&simplify")).await?;
    let text = |a: &Value, k: &str| a[k].as_str().unwrap_or("").to_string();
    Ok(answers
        .as_array()
        .map(|list| {
            list.iter()
                .map(|a| {
                    answer([
                        &text(a, "StudyInstanceUID"),
                        &text(a, "PatientName"),
                        &text(a, "PatientID"),
                        &text(a, "StudyDate"),
                        &text(a, "StudyDescription"),
                        &text(a, "AccessionNumber"),
                        &text(a, "ModalitiesInStudy"),
                        &text(a, "NumberOfStudyRelatedInstances"),
                    ])
                })
                .collect()
        })
        .unwrap_or_default())
}

/// The first value of a DICOM JSON attribute (`"00100010": { "Value": [ { "Alphabetic": ... } ] }`).
fn dicom_json(item: &Value, tag: &str) -> String {
    let values = item[tag]["Value"].as_array();
    let Some(values) = values else { return String::new() };
    values
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_string)
                .or_else(|| v["Alphabetic"].as_str().map(str::to_string))
                .unwrap_or_else(|| v.to_string())
        })
        .collect::<Vec<_>>()
        .join("\\")
}

/// QIDO-RS through Orthanc's DICOMweb client: `POST /dicom-web/servers/{name}/get`.
async fn dicomweb_query(state: &AppState, node: &ModalityNode, c: &Criteria) -> AppResult<Vec<Value>> {
    let enc = |v: &str| utf8_percent_encode(v, NON_ALPHANUMERIC).to_string();
    let mut uri = String::from("/studies?limit=200&includefield=00081030&includefield=00080061&includefield=00201208");
    for (key, value) in [
        ("PatientName", c.name.as_deref().map(contains_pattern)),
        ("PatientID", c.patient_id.clone()),
        ("AccessionNumber", c.accession.clone()),
        ("StudyDescription", c.description.as_deref().map(contains_pattern)),
        ("ModalitiesInStudy", c.modality.clone()),
        ("StudyDate", c.dates.clone()),
    ] {
        if let Some(v) = value {
            uri.push_str(&format!("&{key}={}", enc(&v)));
        }
    }
    let found =
        state.orthanc.post_json(&format!("/dicom-web/servers/{}/get", path_name(node)), &json!({ "Uri": uri })).await?;
    Ok(found
        .as_array()
        .map(|list| {
            list.iter()
                .map(|i| {
                    answer([
                        &dicom_json(i, "0020000D"),
                        &dicom_json(i, "00100010"),
                        &dicom_json(i, "00100020"),
                        &dicom_json(i, "00080020"),
                        &dicom_json(i, "00081030"),
                        &dicom_json(i, "00080050"),
                        &dicom_json(i, "00080061"),
                        &dicom_json(i, "00201208"),
                    ])
                })
                .collect()
        })
        .unwrap_or_default())
}

pub async fn query(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(req): Json<QueryRequest>,
) -> AppResult<Json<Value>> {
    allowed(&state, &user)?;
    let c = criteria(&req)?;
    let node = find_source(&state, &req.source).await?;
    let mut answers = if node.kind == "dicomweb" {
        dicomweb_query(&state, &node, &c).await?
    } else {
        dicom_query(&state, &node, &c).await?
    };
    answers.retain(|a| a["study_uid"].as_str().is_some_and(is_valid_dicom_uid));
    answers.truncate(200);

    // Which of them are already here? One search with the list of UIDs ("any of").
    let uids: Vec<&str> = answers.iter().filter_map(|a| a["study_uid"].as_str()).collect();
    if !uids.is_empty() {
        let here = state
            .orthanc
            .find(&json!({ "Level": "Study", "Query": { "StudyInstanceUID": uids.join("\\") }, "Expand": true, "Limit": 1000 }))
            .await
            .unwrap_or_default();
        for a in &mut answers {
            let uid = a["study_uid"].as_str().unwrap_or("").to_string();
            if let Some(local) =
                here.iter().find(|s| s["MainDicomTags"]["StudyInstanceUID"].as_str() == Some(uid.as_str()))
            {
                a["local_id"] = local["ID"].clone();
            }
        }
    }
    state.db.audit(&user.username, "query_remote", &format!("{}: {} found", node.id, answers.len()));
    Ok(Json(json!({ "answers": answers })))
}

#[derive(Deserialize)]
pub struct RetrieveRequest {
    source: String,
    study_uid: String,
    /// What the Transfers list shows (patient / date / description, from the answer).
    #[serde(default)]
    label: String,
}

pub async fn retrieve(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(req): Json<RetrieveRequest>,
) -> AppResult<Json<Value>> {
    allowed(&state, &user)?;
    if !is_valid_dicom_uid(&req.study_uid) {
        return Err(AppError::BadRequest("Invalid StudyInstanceUID".into()));
    }
    let node = find_source(&state, &req.source).await?;
    let (path, body) = if node.kind == "dicomweb" {
        (
            format!("/dicom-web/servers/{}/retrieve", path_name(&node)),
            json!({ "Resources": [{ "Study": req.study_uid }], "Asynchronous": true }),
        )
    } else {
        (
            format!("/modalities/{}/move", path_name(&node)),
            json!({ "Level": "Study", "Resources": [{ "StudyInstanceUID": req.study_uid }], "Synchronous": false }),
        )
    };
    let answer = state.orthanc.post_json(&path, &body).await?;
    let label: String = req.label.chars().filter(|c| !c.is_control()).take(160).collect();
    let label = if label.trim().is_empty() { req.study_uid.clone() } else { label };
    // A job id when Orthanc runs it in the background; some versions answer only when it is done.
    let job = answer["ID"].as_str().unwrap_or("").to_string();
    let transfer = state.db.add_transfer(&user.username, &req.study_uid, &label, &node.id, &job, "retrieve")?;
    if job.is_empty() {
        let _ = state.db.set_transfer_final(transfer, "Success", None, "{}");
    }
    state.db.audit(&user.username, "retrieve_study", &format!("{label} <- {} (job {job})", node.id));
    Ok(Json(json!({ "transfer_id": transfer, "job_id": job })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dicom_json_values() {
        let item = json!({ "00100010": { "vr": "PN", "Value": [{ "Alphabetic": "DOE^JOHN" }] },
                           "00080061": { "vr": "CS", "Value": ["CT", "SR"] }, "00201208": { "vr": "IS", "Value": [12] } });
        assert_eq!(dicom_json(&item, "00100010"), "DOE^JOHN");
        assert_eq!(dicom_json(&item, "00080061"), "CT\\SR");
        assert_eq!(dicom_json(&item, "00201208"), "12");
        assert_eq!(dicom_json(&item, "00081030"), "");
    }

    #[test]
    fn a_query_needs_something_and_safe_values() {
        let req = |name: &str| QueryRequest {
            source: "x".into(),
            patient_name: Some(name.into()),
            patient_id: None,
            accession: None,
            description: None,
            modality: None,
            date_from: None,
            date_to: None,
        };
        assert!(criteria(&req("DOE")).is_ok());
        assert!(criteria(&req("")).is_err(), "nothing to search");
        assert!(criteria(&req("A\\B")).is_err(), "no DICOM list separator");
        assert!(criteria(&req(&"x".repeat(65))).is_err());
    }
}
