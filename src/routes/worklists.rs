//! # routes/worklists.rs
//!
//! **Modality worklists** through Orthanc's worklists plugin (its REST API, Orthanc 1.12.10 or
//! newer, with `"SaveInOrthancDatabase": true`). The scanners ask Orthanc for the list of
//! examinations to do; the staff prepare it here. Intranet listener only; switched on in
//! Settings > Features; needs the `worklists` permission (or administrator).
//!
//! * `GET    /api/worklists`        every worklist, simplified
//! * `POST   /api/worklists`        create one: `{ patient_name, patient_id, birth_date, sex, accession,
//!   modality, date, time, description, station_aet, physician, requested_procedure }`
//! * `PUT    /api/worklists/{id}`   change one (the plugin has no "edit": deleted and created again)
//! * `DELETE /api/worklists/{id}`

use crate::auth::AuthUser;
use crate::db::User;
use crate::error::{AppError, AppResult};
use crate::orthanc::OrthancClient;
use crate::permissions::Permission;
use crate::servers::Chosen;
use crate::state::AppState;
use crate::util::feature;
use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

/// The checks of every request, and the server whose worklists are shown (`?server=`).
fn allowed(state: &AppState, user: &User, chosen: &Chosen) -> AppResult<OrthancClient> {
    user.require_or_admin(Permission::Worklists)?;
    feature(state.settings.get().worklists_enabled, "Worklists are switched off")?;
    state.orthancs.chosen(user, chosen)
}

fn check_id(id: &str) -> AppResult<()> {
    if (1..=64).contains(&id.len()) && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        Ok(())
    } else {
        Err(AppError::BadRequest("Invalid worklist id".into()))
    }
}

/// One worklist as the page shows it, from the plugin's answer (simplified tags, at the top or under `Tags`).
fn simplify(id: &str, item: &Value) -> Value {
    let tags = if item["Tags"].is_object() { &item["Tags"] } else { item };
    let step =
        tags["ScheduledProcedureStepSequence"].as_array().and_then(|a| a.first()).cloned().unwrap_or(Value::Null);
    let t = |v: &Value, k: &str| v[k].as_str().unwrap_or("").to_string();
    json!({
        "id": id,
        "patient_name": t(tags, "PatientName"),
        "patient_id": t(tags, "PatientID"),
        "birth_date": t(tags, "PatientBirthDate"),
        "sex": t(tags, "PatientSex"),
        "accession": t(tags, "AccessionNumber"),
        "requested_procedure": t(tags, "RequestedProcedureDescription"),
        "study_uid": t(tags, "StudyInstanceUID"),
        "modality": t(&step, "Modality"),
        "date": t(&step, "ScheduledProcedureStepStartDate"),
        "time": t(&step, "ScheduledProcedureStepStartTime"),
        "description": t(&step, "ScheduledProcedureStepDescription"),
        "station_aet": t(&step, "ScheduledStationAETitle"),
        "physician": t(&step, "ScheduledPerformingPhysicianName"),
    })
}

pub async fn list(State(state): State<AppState>, AuthUser(user): AuthUser, chosen: Chosen) -> AppResult<Json<Value>> {
    let orthanc = allowed(&state, &user, &chosen)?;
    let answer = orthanc.get_json("/worklists/?format=Simplify").await?;
    let mut items: Vec<Value> = match &answer {
        Value::Array(list) => list.iter().map(|w| simplify(w["ID"].as_str().unwrap_or(""), w)).collect(),
        Value::Object(map) => map.iter().map(|(id, w)| simplify(id, w)).collect(),
        _ => Vec::new(),
    };
    items.sort_by(|a, b| (a["date"].as_str(), a["time"].as_str()).cmp(&(b["date"].as_str(), b["time"].as_str())));
    Ok(Json(Value::Array(items)))
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct WorklistForm {
    patient_name: String,
    patient_id: String,
    birth_date: String,
    sex: String,
    accession: String,
    modality: String,
    date: String,
    time: String,
    description: String,
    station_aet: String,
    physician: String,
    requested_procedure: String,
}

/// The `Tags` of the plugin's create call, from the checked form.
fn tags(f: &WorklistForm) -> AppResult<Value> {
    let bad = |m: &str| Err(AppError::BadRequest(m.into()));
    let all = [
        &f.patient_name,
        &f.patient_id,
        &f.birth_date,
        &f.sex,
        &f.accession,
        &f.modality,
        &f.date,
        &f.time,
        &f.description,
        &f.station_aet,
        &f.physician,
        &f.requested_procedure,
    ];
    if all.iter().any(|v| v.chars().count() > 64 || v.chars().any(|c| c.is_control() || c == '\\')) {
        return bad("Each value: at most 64 characters, on one line");
    }
    if f.patient_id.trim().is_empty() || f.patient_name.trim().is_empty() {
        return bad("The patient name and ID are needed");
    }
    let is_date = |d: &str| d.is_empty() || (d.len() == 8 && d.chars().all(|c| c.is_ascii_digit()));
    if !is_date(&f.date) || !is_date(&f.birth_date) {
        return bad("Dates are written YYYYMMDD");
    }
    if !f.time.chars().all(|c| c.is_ascii_digit()) || f.time.len() > 6 {
        return bad("The time is written HHMM or HHMMSS");
    }
    if !matches!(f.sex.as_str(), "" | "M" | "F" | "O") {
        return bad("Sex: M, F, O or empty");
    }
    if f.modality.trim().is_empty() || !f.modality.chars().all(|c| c.is_ascii_alphanumeric()) {
        return bad("The modality is a DICOM code such as CT, MR or US");
    }
    if !f.station_aet.chars().all(|c| c.is_ascii_graphic()) || f.station_aet.len() > 16 {
        return bad("The AE title of the station: at most 16 letters or digits");
    }
    let t = |v: &str| json!(v.trim());
    Ok(json!({
        "PatientName": t(&f.patient_name),
        "PatientID": t(&f.patient_id),
        "PatientBirthDate": t(&f.birth_date),
        "PatientSex": t(&f.sex),
        "AccessionNumber": t(&f.accession),
        "RequestedProcedureDescription": t(&f.requested_procedure),
        "ScheduledProcedureStepSequence": [{
            "Modality": t(&f.modality.to_uppercase()),
            "ScheduledProcedureStepStartDate": t(&f.date),
            "ScheduledProcedureStepStartTime": t(&f.time),
            "ScheduledProcedureStepDescription": t(&f.description),
            "ScheduledStationAETitle": t(&f.station_aet),
            "ScheduledPerformingPhysicianName": t(&f.physician),
        }],
    }))
}

async fn create_one(orthanc: &OrthancClient, form: &WorklistForm) -> AppResult<String> {
    let answer = orthanc.post_json("/worklists/create", &json!({ "Tags": tags(form)? })).await?;
    Ok(answer["ID"].as_str().unwrap_or("").to_string())
}

pub async fn create(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    chosen: Chosen,
    Json(form): Json<WorklistForm>,
) -> AppResult<Json<Value>> {
    let orthanc = allowed(&state, &user, &chosen)?;
    let id = create_one(&orthanc, &form).await?;
    state.db.audit(&user.username, "worklist_created", &format!("{id} ({} {})", form.patient_id, form.modality));
    Ok(Json(json!({ "id": id })))
}

pub async fn update(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    chosen: Chosen,
    Path(id): Path<String>,
    Json(form): Json<WorklistForm>,
) -> AppResult<Json<Value>> {
    let orthanc = allowed(&state, &user, &chosen)?;
    check_id(&id)?;
    tags(&form)?; // checked BEFORE the old one is deleted
    orthanc.get_json(&format!("/worklists/{id}")).await?;
    let new_id = create_one(&orthanc, &form).await?;
    orthanc.delete(&format!("/worklists/{id}")).await?;
    state.db.audit(
        &user.username,
        "worklist_changed",
        &format!("{id} -> {new_id} ({} {})", form.patient_id, form.modality),
    );
    Ok(Json(json!({ "id": new_id })))
}

pub async fn delete(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    chosen: Chosen,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let orthanc = allowed(&state, &user, &chosen)?;
    check_id(&id)?;
    orthanc.delete(&format!("/worklists/{id}")).await?;
    state.db.audit(&user.username, "worklist_deleted", &id);
    Ok(Json(json!({ "deleted": id })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worklist_values_are_checked() {
        let form = |f: fn(&mut WorklistForm)| {
            let mut w = WorklistForm {
                patient_name: "DOE^JANE".into(),
                patient_id: "P-9".into(),
                modality: "US".into(),
                date: "20261120".into(),
                ..Default::default()
            };
            f(&mut w);
            tags(&w)
        };
        assert!(form(|_| {}).is_ok());
        assert!(form(|w| w.patient_id.clear()).is_err());
        assert!(form(|w| w.date = "2026-11-20".into()).is_err());
        assert!(form(|w| w.modality = "U S".into()).is_err());
        assert!(form(|w| w.sex = "Q".into()).is_err());
        assert!(form(|w| w.description = "a\\b".into()).is_err());
        assert_eq!(form(|_| {}).unwrap()["ScheduledProcedureStepSequence"][0]["Modality"], "US");
    }

    #[test]
    fn both_answer_shapes_are_read() {
        let top = simplify("1", &json!({ "PatientID": "A", "ScheduledProcedureStepSequence": [{ "Modality": "CT" }] }));
        let nested = simplify("2", &json!({ "Tags": { "PatientID": "B" } }));
        assert_eq!((top["patient_id"].as_str(), top["modality"].as_str()), (Some("A"), Some("CT")));
        assert_eq!(nested["patient_id"], "B");
    }
}
