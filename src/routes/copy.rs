//! # routes/copy.rs
//!
//! **Copy a study to another Orthanc server** (several servers, see `servers.rs`).
//!
//! * `POST /api/studies/{id}/copy`  `{ "server": "archive", "delete_original": false }`
//!
//! The portal copies the study itself, image by image: each file is read from the study's server
//! and stored on the other one as it arrives (never kept whole in memory), a few at a time. This
//! works between any two Orthanc servers, without configuring them as peers of each other. The
//! copy is a transfer of the Transfers list (direction `copy`, destination = the server's key),
//! with its progress while it runs.
//!
//! Needs the `send` permission and both servers among those the person may use.
//! "Delete the original afterwards" needs the `delete` permission and deleting switched on, and
//! only happens when every image was copied.

use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use crate::models::StudySummary;
use crate::orthanc::OrthancClient;
use crate::permissions::Permission;
use crate::servers::ResourceRef;
use crate::state::AppState;
use crate::util::feature;
use axum::extract::{Path, State};
use axum::Json;
use futures_util::{stream, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

/// Images copied at the same time.
const PARALLEL_IMAGES: usize = 4;
/// Copies running at the same time (each reads and writes whole studies).
const MAX_RUNNING: usize = 4;

/// Where a running copy is: read by the Transfers list.
#[derive(Clone, Copy)]
pub struct Progress {
    pub total: u64,
    pub done: u64,
    pub failed: u64,
    pub started: Instant,
}

/// The copies running now, by transfer ID. A copy interrupted by a restart of the portal is not
/// here any more: the Transfers list then says its outcome is unknown.
static RUNNING: LazyLock<Mutex<HashMap<i64, Progress>>> = LazyLock::new(Default::default);

fn running() -> std::sync::MutexGuard<'static, HashMap<i64, Progress>> {
    RUNNING.lock().unwrap_or_else(|e| e.into_inner())
}

/// The progress of a running copy (`None` = not running in this portal).
pub fn progress(transfer_id: i64) -> Option<Progress> {
    running().get(&transfer_id).copied()
}

#[derive(Deserialize)]
pub struct CopyRequest {
    /// The key of the server to copy to.
    server: String,
    /// Delete the study from its server once every image was copied.
    #[serde(default)]
    delete_original: bool,
}

pub async fn copy_study(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
    Json(req): Json<CopyRequest>,
) -> AppResult<Json<Value>> {
    user.require(Permission::Send)?;
    let (source, study) = state.orthancs.resolve(&user, &id)?;
    let target = state.orthancs.get(&user, &req.server)?;
    if req.server == study.server {
        return Err(AppError::BadRequest("The study is already on that server".into()));
    }
    if req.delete_original {
        user.require_or_admin(Permission::Delete)?;
        feature(state.settings.get().allow_delete, "Deleting is switched off")?;
    }
    let (transfer_id, total) =
        start_copy(&state, &user.username, source, study, &req.server, target, req.delete_original).await?;
    Ok(Json(json!({ "transfer_id": transfer_id, "images": total })))
}

/// Starts copying a study to the server `target_key` (checked by the caller: a person's request,
/// or an auto-forward rule) and records it in the Transfers list. Gives the transfer's ID and the
/// number of images.
pub async fn start_copy(
    state: &AppState,
    username: &str,
    source: OrthancClient,
    study: ResourceRef,
    target_key: &str,
    target: OrthancClient,
    delete: bool,
) -> AppResult<(i64, u64)> {
    let summary = StudySummary::from_orthanc(&source.get_json(&format!("/studies/{}", study.id)).await?);
    let images: Vec<String> = source
        .get_json(&format!("/studies/{}/instances", study.id))
        .await?
        .as_array()
        .map(|list| list.iter().filter_map(|i| i["ID"].as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    if images.is_empty() {
        return Err(AppError::BadRequest("This study has no images to copy".into()));
    }
    let label: String =
        format!("{} | {} | {}", summary.patient_name.replace('^', " "), summary.study_date, summary.study_description)
            .chars()
            .take(160)
            .collect();
    if running().len() >= MAX_RUNNING {
        return Err(AppError::TooManyRequests(format!(
            "{MAX_RUNNING} copies are running already: try again when one has finished (Transfers)"
        )));
    }
    let job = format!("copy-{}", &crate::sharing::new_token()[..16]);
    let transfer_id = state.db.add_transfer(username, &study.compound(), &label, target_key, &job, "copy")?;
    let total = images.len() as u64;
    running().insert(transfer_id, Progress { total, done: 0, failed: 0, started: Instant::now() });
    let target_name = state.orthancs.info(target_key).map(|s| s.label).unwrap_or_else(|| target_key.to_string());
    state.db.audit(
        username,
        "copy_study",
        &format!(
            "{label} ({}) -> {target_name}{}",
            study.compound(),
            if delete { ", then deleted from its server" } else { "" }
        ),
    );
    let task = Copy { state: state.clone(), transfer_id, source, target, study, images, delete };
    tokio::spawn(task.run(username.to_string(), label));
    Ok((transfer_id, total))
}

/// One copy, run in the background.
struct Copy {
    state: AppState,
    transfer_id: i64,
    source: OrthancClient,
    target: OrthancClient,
    study: ResourceRef,
    images: Vec<String>,
    delete: bool,
}

impl Copy {
    async fn run(self, username: String, label: String) {
        let total = self.images.len() as u64;
        let (source, target, id) = (&self.source, &self.target, self.transfer_id);
        let mut first_error: Option<String> = None;
        let mut outcomes = stream::iter(self.images.clone())
            .map(|image| {
                let (source, target) = (source.clone(), target.clone());
                async move { copy_image(&source, &target, &image).await }
            })
            .buffer_unordered(PARALLEL_IMAGES);
        while let Some(outcome) = outcomes.next().await {
            let mut map = running();
            if let Some(p) = map.get_mut(&id) {
                p.done += 1;
                if outcome.is_err() {
                    p.failed += 1;
                }
            }
            drop(map);
            if let Err(e) = outcome {
                tracing::warn!("copy {id}: an image could not be copied: {e}");
                first_error.get_or_insert_with(|| e.to_string());
            }
        }
        let progress = running().remove(&id);
        let failed = progress.map_or(0, |p| p.failed);
        let runtime = progress.map_or(0.0, |p| p.started.elapsed().as_secs_f64());
        let (state, error) = match first_error {
            Some(e) => ("Failure", Some(format!("{failed} of {total} images could not be copied: {e}"))),
            None if self.delete => match source.delete(&format!("/studies/{}", self.study.id)).await {
                Ok(()) | Err(AppError::NotFound) => {
                    self.state.db.audit(&username, "delete_studies", &format!("{label} (moved to another server)"));
                    ("Success", None)
                }
                Err(e) => ("Success", Some(format!("Copied, but the original could not be deleted: {e}"))),
            },
            None => ("Success", None),
        };
        let info = json!({ "instances": total, "failed": failed, "runtime": runtime }).to_string();
        if let Err(e) = self.state.db.set_transfer_final(id, state, error.as_deref(), &info) {
            tracing::error!("copy {id}: its outcome could not be saved: {e}");
        }
    }
}

/// Reads one image from `source` and stores it on `target`, as it arrives. An image the target
/// already has is fine (Orthanc answers "AlreadyStored").
async fn copy_image(source: &OrthancClient, target: &OrthancClient, image: &str) -> AppResult<()> {
    let file = source.get_stream(&format!("/instances/{image}/file")).await?;
    let length = file.content_length();
    target.upload_instance(reqwest::Body::wrap_stream(file.bytes_stream()), length).await?;
    Ok(())
}
