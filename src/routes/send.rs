//! # routes/send.rs
//!
//! Sending studies to **other systems**: the remote DICOM nodes (what Orthanc calls *DICOM
//! modalities*, `DicomModalities`), and, when switched on in Settings > Features, the other
//! Orthanc servers (`OrthancPeers`) and the DICOMweb servers (`DicomWeb.Servers`) configured in
//! Orthanc. All routes require the `send` permission and exist on the intranet listener only.
//!
//! * `GET  /api/modalities`          destinations: `{ id, kind, aet, host, port, url, description }`.
//!   A DICOM node's id is its alias; a peer is `peer:<name>`, a DICOMweb server `dicomweb:<name>`.
//! * `POST /api/studies/{id}/send`   start sending a study: body `{ "target": "<id>" }`
//! * `POST /api/send`                send several studies: `{ "target", "studies": [...] }` (one transfer each)
//! * `GET  /api/transfers`           recent transfers (and retrievals) with their live state
//!
//! How it works: the portal asks Orthanc to run a job (`POST /modalities/{alias}/store`,
//! `/peers/{name}/store` or `/dicom-web/servers/{name}/stow`, all with `"Synchronous": false`).
//! Orthanc answers at once with a **job id**; the portal stores it and, when the Transfers tab is
//! open, reads `GET /jobs/{id}` to show Pending / Running / Success / Failure, the progress,
//! the number of images, the time it has been running and the images that failed.

use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use crate::models::{ModalityNode, StudySummary};
use crate::permissions::Permission;
use crate::servers::ResourceRef;
use crate::state::AppState;
use crate::util::feature;
use axum::extract::{Path, State};
use axum::Json;
use futures_util::{stream, StreamExt};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Studies started at the same time when several are sent (Orthanc queues the jobs anyway).
const PARALLEL_STARTS: usize = 4;

/// A peer or DICOMweb server of Orthanc, as a destination (also a source of query / retrieve).
pub fn named_target(kind: &'static str, name: &str, cfg: &Value) -> ModalityNode {
    let mut node = ModalityNode::bare(&format!("{kind}:{name}"));
    node.kind = kind;
    // Only the address: never the user name or password Orthanc keeps for it.
    node.url = cfg["Url"].as_str().or_else(|| cfg.as_array().and_then(|a| a.first()?.as_str())).unwrap_or("").into();
    node
}

/// Every destination the user may send to (DICOM nodes, and peers / DICOMweb servers when switched on).
pub async fn destinations(state: &AppState) -> AppResult<Vec<ModalityNode>> {
    let settings = state.settings.get();
    let orthanc = &state.orthanc;
    // The three lists are independent: asked at the same time.
    let peers = async {
        if settings.send_to_peers {
            orthanc.named_targets("/peers").await
        } else {
            Default::default()
        }
    };
    let servers = async {
        if settings.send_to_dicomweb {
            orthanc.named_targets("/dicom-web/servers").await
        } else {
            Default::default()
        }
    };
    let (nodes, peers, servers) = tokio::join!(orthanc.list_modalities(), peers, servers);
    let mut nodes = nodes?;
    nodes.extend(peers.iter().map(|(name, cfg)| named_target("peer", name, cfg)));
    nodes.extend(servers.iter().map(|(name, cfg)| named_target("dicomweb", name, cfg)));
    let descriptions = state.db.node_descriptions()?;
    for node in &mut nodes {
        node.description = descriptions.get(&node.id).cloned().unwrap_or_default();
    }
    Ok(nodes)
}

/// Lists the destinations a study can be sent to.
pub async fn list_modalities(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
) -> AppResult<Json<Vec<ModalityNode>>> {
    user.require(Permission::Send)?;
    Ok(Json(destinations(&state).await?))
}

#[derive(Deserialize)]
pub struct SendRequest {
    target: String,
}

/// Starts the job that sends one study to `node`; records it in the Transfers list. `person` is
/// who asks (checked: they must be allowed to use the study's server); `None` for a background
/// task (an auto-forward rule), whose study comes from the server it watches.
pub(crate) async fn start_send(
    state: &AppState,
    username: &str,
    person: Option<&crate::db::User>,
    node: &ModalityNode,
    id: &str,
) -> AppResult<(i64, String)> {
    let study_ref = ResourceRef::parse(id)?;
    // The DICOM nodes, peers and DICOMweb servers listed are those of the default Orthanc.
    // Sending from another server needs that server's own (multi-server phase 3).
    if !study_ref.is_main() {
        return Err(AppError::BadRequest(
            "Sending is only available for the studies of the default Orthanc server for now".into(),
        ));
    }
    let orthanc = match person {
        Some(user) => state.orthancs.of(user, &study_ref)?,
        None => state.orthancs.stored(&study_ref.server)?,
    };
    let id = study_ref.id.as_str();
    let study = StudySummary::from_orthanc(&orthanc.get_json(&format!("/studies/{id}")).await?);
    let label: String =
        format!("{} | {} | {}", study.patient_name.replace('^', " "), study.study_date, study.study_description)
            .chars()
            .take(160)
            .collect();

    let job_id = match node.kind {
        "dicom" => orthanc.store_to_modality(&node.id, id).await?,
        kind => {
            let name = node.id.split_once(':').map(|(_, n)| n).unwrap_or_default();
            let name = utf8_percent_encode(name, NON_ALPHANUMERIC);
            let path =
                if kind == "peer" { format!("/peers/{name}/store") } else { format!("/dicom-web/servers/{name}/stow") };
            orthanc.start_job(&path, &json!({ "Resources": [id], "Synchronous": false })).await?
        }
    };
    let transfer_id = state.db.add_transfer(username, id, &label, &node.id, &job_id, "send")?;
    state.db.audit(username, "send_study", &format!("{label} -> {} (job {job_id})", node.id));
    Ok((transfer_id, job_id))
}

/// The destination called `target`, if the user may send there.
pub(crate) async fn find_destination(state: &AppState, target: &str) -> AppResult<ModalityNode> {
    destinations(state)
        .await?
        .into_iter()
        .find(|n| n.id == target)
        .ok_or_else(|| AppError::BadRequest("Unknown destination".into()))
}

pub async fn send_study(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
    Json(req): Json<SendRequest>,
) -> AppResult<Json<Value>> {
    user.require(Permission::Send)?;
    let (_, study) = state.orthancs.resolve(&user, &id)?;
    let node = find_destination(&state, &req.target).await?;
    let (transfer_id, job_id) = start_send(&state, &user.username, Some(&user), &node, &study.compound()).await?;
    Ok(Json(json!({ "transfer_id": transfer_id, "job_id": job_id })))
}

#[derive(Deserialize)]
pub struct BulkSendRequest {
    target: String,
    studies: Vec<String>,
}

/// Sends several studies (the tick boxes of the lists): one transfer per study.
pub async fn send_many(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(req): Json<BulkSendRequest>,
) -> AppResult<Json<Value>> {
    user.require(Permission::Send)?;
    feature(state.settings.get().bulk_actions, "Actions on several studies are switched off")?;
    if req.studies.is_empty() || req.studies.len() > 100 {
        return Err(AppError::BadRequest("Choose 1 to 100 studies".into()));
    }
    if req.studies.iter().any(|id| ResourceRef::try_parse(id).is_none()) {
        return Err(AppError::BadRequest("Invalid study id".into()));
    }
    let node = find_destination(&state, &req.target).await?;
    let (state, user, node) = (&state, &user, &node);
    let outcomes: Vec<(String, AppResult<(i64, String)>)> = stream::iter(req.studies.clone())
        .map(|id| async move {
            let outcome = start_send(state, &user.username, Some(user), node, &id).await;
            (id, outcome)
        })
        .buffered(PARALLEL_STARTS)
        .collect()
        .await;
    let errors: Vec<String> =
        outcomes.iter().filter_map(|(id, r)| r.as_ref().err().map(|e| format!("{id}: {e}"))).collect();
    Ok(Json(json!({ "started": outcomes.len() - errors.len(), "errors": errors })))
}

#[derive(Serialize)]
pub struct TransferView {
    id: i64,
    ts: i64,
    username: String,
    label: String,
    target: String,
    /// `send` or `retrieve`.
    direction: String,
    /// A retrieval: the StudyInstanceUID brought in (to open it once it is here).
    study_uid: Option<String>,
    state: String,
    progress: u64,
    error: Option<String>,
    instances: Option<u64>,
    failed: Option<u64>,
    runtime: Option<f64>,
}

/// The numbers Orthanc gives about a C-STORE job: `(images, failed images, run time in seconds)`.
fn job_numbers(job: &Value) -> (Option<u64>, Option<u64>, Option<f64>) {
    let content = &job["Content"];
    (content["InstancesCount"].as_u64(), content["FailedInstancesCount"].as_u64(), job["EffectiveRuntime"].as_f64())
}

/// Recent transfers. Users see their own; administrators see everybody's.
pub async fn list_transfers(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
) -> AppResult<Json<Vec<TransferView>>> {
    if !(user.may(Permission::Send) || user.may(Permission::Retrieve)) {
        return Err(AppError::Forbidden("send"));
    }
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
                let info: Value =
                    t.final_info.as_deref().and_then(|i| serde_json::from_str(i).ok()).unwrap_or_default();
                return TransferView {
                    id: t.id,
                    study_uid: (t.direction == "retrieve").then(|| t.study_id.clone()),
                    direction: t.direction.clone(),
                    ts: t.ts,
                    username: t.username,
                    label: t.label,
                    target: t.target,
                    state: done.clone(),
                    progress,
                    error: t.final_error,
                    instances: info["instances"].as_u64(),
                    failed: info["failed"].as_u64(),
                    runtime: info["runtime"].as_f64(),
                };
            }
            let (job_state, progress, error, (instances, failed, runtime)) = match orthanc.get_job(&t.job_id).await {
                Ok(job) => {
                    let state = job["State"].as_str().unwrap_or("Unknown").to_string();
                    let error =
                        if state == "Failure" { job["ErrorDescription"].as_str().map(str::to_string) } else { None };
                    (state, job["Progress"].as_u64().unwrap_or(0), error, job_numbers(&job))
                }
                // Orthanc no longer remembers the job: its outcome can never be known.
                Err(AppError::NotFound) => ("Unknown".to_string(), 0, None, (None, None, None)),
                // Orthanc did not answer: say nothing definitive, try again next time.
                Err(_) => ("Unavailable".to_string(), 0, None, (None, None, None)),
            };
            if matches!(job_state.as_str(), "Success" | "Failure" | "Unknown") {
                let info = json!({ "instances": instances, "failed": failed, "runtime": runtime }).to_string();
                let _ = db.set_transfer_final(t.id, &job_state, error.as_deref(), &info);
            }
            TransferView {
                id: t.id,
                study_uid: (t.direction == "retrieve").then(|| t.study_id.clone()),
                direction: t.direction.clone(),
                ts: t.ts,
                username: t.username,
                label: t.label,
                target: t.target,
                state: job_state,
                progress,
                error,
                instances,
                failed,
                runtime,
            }
        })
        .buffered(8)
        .collect::<Vec<_>>()
        .await;

    Ok(Json(views))
}
