//! # routes/send.rs
//!
//! Sending a study to a **remote DICOM node** (what Orthanc calls a *DICOM
//! modality*, declared in Orthanc's `DicomModalities` configuration). All routes
//! require the `send` permission and exist on the intranet listener only.
//!
//! * `GET  /api/modalities`          destinations Orthanc can send to
//! * `POST /api/studies/{id}/send`   start sending a study: body `{ "target": "alias" }`
//! * `GET  /api/transfers`           recent transfers with their live state
//!
//! How it works: the portal asks Orthanc to run a C-STORE job
//! (`POST /modalities/{alias}/store` with `"Synchronous": false`). Orthanc answers
//! at once with a **job id**; the portal stores it and, when the Transfers tab is
//! open, reads `GET /jobs/{id}` to show Pending / Running / Success / Failure.

use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use crate::models::{ModalityNode, StudySummary};
use crate::permissions::Permission;
use crate::state::AppState;
use crate::util::is_valid_orthanc_id;
use axum::extract::{Path, State};
use axum::Json;
use futures_util::{stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Lists the remote DICOM nodes the study can be sent to.
pub async fn list_modalities(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
) -> AppResult<Json<Vec<ModalityNode>>> {
    user.require(Permission::Send)?;
    Ok(Json(state.orthanc.list_modalities().await?))
}

#[derive(Deserialize)]
pub struct SendRequest {
    /// Alias of the destination, as returned by `GET /api/modalities`.
    target: String,
}

/// Starts sending the whole study to the chosen node and returns immediately.
pub async fn send_study(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
    Json(req): Json<SendRequest>,
) -> AppResult<Json<Value>> {
    user.require(Permission::Send)?;
    if !is_valid_orthanc_id(&id) {
        return Err(AppError::BadRequest("Invalid study id".into()));
    }

    // The destination must be one Orthanc really offers: the user's text is only
    // used to PICK an entry, never to build a URL.
    let nodes = state.orthanc.list_modalities().await?;
    let node = nodes
        .iter()
        .find(|n| n.id == req.target)
        .ok_or_else(|| AppError::BadRequest("Unknown DICOM destination".into()))?;

    // Reading the study checks it exists (404 otherwise) and gives a readable label.
    let study = StudySummary::from_orthanc(&state.orthanc.get_json(&format!("/studies/{id}")).await?);
    let label: String =
        format!("{} | {} | {}", study.patient_name.replace('^', " "), study.study_date, study.study_description)
            .chars()
            .take(160)
            .collect();

    let job_id = state.orthanc.store_to_modality(&node.id, &id).await?;
    let transfer_id = state.db.add_transfer(&user.username, &id, &label, &node.id, &job_id)?;
    state.db.audit(&user.username, "send_study", &format!("{label} -> {} (job {job_id})", node.id));
    Ok(Json(json!({ "transfer_id": transfer_id, "job_id": job_id })))
}

/// One row of the Transfers tab: what was sent, where, and the live job state.
#[derive(Serialize)]
pub struct TransferView {
    id: i64,
    ts: i64,
    username: String,
    label: String,
    target: String,
    /// `Pending`, `Running`, `Success`, `Failure`, `Paused`, `Retry`, or
    /// `Unknown` when Orthanc no longer remembers the job.
    state: String,
    /// 0-100.
    progress: u64,
    /// Orthanc's explanation when the state is `Failure`.
    error: Option<String>,
}

/// Recent transfers. Users see their own; administrators see everybody's.
pub async fn list_transfers(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
) -> AppResult<Json<Vec<TransferView>>> {
    user.require(Permission::Send)?;
    let only_user = if user.permissions.contains(Permission::Admin) { None } else { Some(user.username.as_str()) };
    let transfers = state.db.recent_transfers(only_user, 40)?;

    // Ask Orthanc only about transfers that have not ended yet (up to 8 requests at a
    // time, keeping the order). Once a job has ended its outcome is stored in the
    // database, so the Transfers tab (which refreshes every 3 seconds while a job runs)
    // does not repeat 40 requests for jobs that finished long ago.
    let orthanc = &state.orthanc;
    let db = &state.db;
    let views = stream::iter(transfers)
        .map(|t| async move {
            if let Some(done) = &t.final_state {
                let progress = if done == "Success" { 100 } else { 0 };
                return TransferView {
                    id: t.id,
                    ts: t.ts,
                    username: t.username,
                    label: t.label,
                    target: t.target,
                    state: done.clone(),
                    progress,
                    error: t.final_error,
                };
            }
            let (job_state, progress, error) = match orthanc.get_job(&t.job_id).await {
                Ok(job) => {
                    let state = job["State"].as_str().unwrap_or("Unknown").to_string();
                    let error =
                        if state == "Failure" { job["ErrorDescription"].as_str().map(str::to_string) } else { None };
                    (state, job["Progress"].as_u64().unwrap_or(0), error)
                }
                // Orthanc no longer remembers the job: its outcome can never be known.
                Err(AppError::NotFound) => ("Unknown".to_string(), 0, None),
                // Orthanc did not answer: say nothing definitive, try again next time.
                Err(_) => ("Unavailable".to_string(), 0, None),
            };
            if matches!(job_state.as_str(), "Success" | "Failure" | "Unknown") {
                let _ = db.set_transfer_final(t.id, &job_state, error.as_deref());
            }
            TransferView {
                id: t.id,
                ts: t.ts,
                username: t.username,
                label: t.label,
                target: t.target,
                state: job_state,
                progress,
                error,
            }
        })
        .buffered(8)
        .collect::<Vec<_>>()
        .await;

    Ok(Json(views))
}
