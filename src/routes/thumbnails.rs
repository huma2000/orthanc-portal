//! # routes/thumbnails.rs
//!
//! Preview thumbnails shown next to studies and series.
//!
//! * `GET /api/studies/{id}/thumbnail[?size=64|128|256]`
//! * `GET /api/series/{id}/thumbnail[?size=64|128|256]`
//!
//! Answers a small JPEG, or **404 when there is nothing to show** (a report, a
//! series without pixel data, an unsupported format): the web UI then draws a
//! placeholder with the modality instead.
//!
//! A thumbnail is a picture of the patient's image, so it needs the **`visualize`**
//! permission (not just `read`). Like viewing, it is available on the internet-facing
//! listener too. The feature can be switched off in Settings > Viewers and uploads.
//!
//! The selection of the image and the memory cache live in `thumbnails.rs`.

use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use crate::permissions::Permission;
use crate::state::AppState;
use crate::thumbnails::{self, Thumb};
use crate::util::is_valid_orthanc_id;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::Response;
use serde::Deserialize;
use std::sync::Arc;

#[derive(Deserialize)]
pub struct ThumbQuery {
    /// Wanted width in pixels. Rounded up to 64, 128 or 256 so that the cache is
    /// shared by everybody (default 128: crisp when shown at 64 px on a HiDPI screen).
    size: Option<u32>,
}

/// What a thumbnail is of.
#[derive(Clone, Copy)]
enum Kind {
    Study,
    Series,
}

fn normalised_width(size: Option<u32>) -> u32 {
    match size.unwrap_or(128) {
        0..=64 => 64,
        65..=128 => 128,
        _ => 256,
    }
}

/// Wraps the image in a response. Medical data: never stored by browsers or proxies.
fn image_response(thumb: Option<Arc<Thumb>>) -> AppResult<Response> {
    let thumb = thumb.ok_or(AppError::NotFound)?;
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, thumb.content_type.as_str())
        .header(header::CACHE_CONTROL, "private, no-store")
        .body(Body::from(thumb.bytes.clone()))
        .map_err(|e| AppError::Internal(format!("cannot build thumbnail response: {e}")))
}

async fn serve(
    state: &AppState,
    user: &crate::db::User,
    kind: Kind,
    id: String,
    query: ThumbQuery,
) -> AppResult<Response> {
    user.require(Permission::Visualize)?;
    let settings = state.settings.get();
    if !settings.thumbnails_enabled {
        return Err(AppError::NotFound);
    }
    if !is_valid_orthanc_id(&id) {
        return Err(AppError::BadRequest("Invalid id".into()));
    }
    let width = normalised_width(query.size);
    let prefix = match kind {
        Kind::Study => "study",
        Kind::Series => "series",
    };
    let key = format!("{prefix}:{id}:{width}");

    // 1. Already known?
    if let Some(known) = state.thumbs.get(&key) {
        return image_response(known);
    }

    // 2. Produce it, a few at a time. Another request may have produced it while we waited.
    let _slot = state.thumbs.permit().await?;
    if let Some(known) = state.thumbs.get(&key) {
        return image_response(known);
    }
    let made = match kind {
        Kind::Study => thumbnails::study_thumbnail(&state.orthanc, &id, width).await?,
        Kind::Series => thumbnails::series_thumbnail(&state.orthanc, &id, width).await?,
    };
    let made = made.map(Arc::new);
    state.thumbs.put(key, made.clone(), settings.thumbnail_cache_bytes());
    image_response(made)
}

pub async fn study_thumbnail(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
    Query(query): Query<ThumbQuery>,
) -> AppResult<Response> {
    serve(&state, &user, Kind::Study, id, query).await
}

pub async fn series_thumbnail(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
    Query(query): Query<ThumbQuery>,
) -> AppResult<Response> {
    serve(&state, &user, Kind::Series, id, query).await
}
