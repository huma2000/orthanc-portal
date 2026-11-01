//! # thumbnails.rs
//!
//! Preview images for studies and series, and the memory cache that keeps them.
//!
//! ## Which image represents what
//! Orthanc can render any single instance as a small JPEG
//! (`GET /instances/{id}/rendered?width=N`), but it has no "thumbnail of a series"
//! or "of a study", so the portal chooses the image itself:
//!
//! * **series** -> the instance in the *middle* of the series (in the order Orthanc
//!   keeps them). The first and last slices of a CT/MR are often almost empty,
//!   the middle one usually shows the anatomy. If it cannot be rendered, the first
//!   and the last are tried.
//! * **study** -> the series that holds the most images, ignoring series without
//!   pixel data (reports, presentation states, RT structures...); ties go to the
//!   lowest series number. Its middle image is used. The next best series are
//!   tried if that one cannot be rendered.
//!
//! ## Cost control
//! Rendering needs Orthanc to decode an image, so:
//! * at most [`MAX_PARALLEL`] thumbnails are produced at the same time;
//! * results are remembered in memory (size set in Settings > Viewers and
//!   uploads), including the fact that a series "has no preview", for a short time.
//!
//! Nothing is written to disk and thumbnails are sent with `Cache-Control:
//! no-store`, so patient images do not end up in browser caches.

use crate::error::{AppError, AppResult};
use crate::orthanc::OrthancClient;
use axum::body::Bytes;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// A found thumbnail is remembered this long (a series can still be receiving images).
const FOUND_TTL: Duration = Duration::from_secs(15 * 60);
/// "This has no preview" is remembered much shorter (it may be fixed by new uploads).
const MISSING_TTL: Duration = Duration::from_secs(2 * 60);
/// Thumbnails produced at the same time (protects Orthanc).
const MAX_PARALLEL: usize = 4;

/// Modalities whose series contain no ordinary picture. They are skipped when
/// choosing the series that represents a study.
const NON_IMAGE_MODALITIES: [&str; 13] =
    ["SR", "PR", "KO", "DOC", "RTSTRUCT", "RTPLAN", "RTRECORD", "REG", "SEG", "ECG", "AU", "FID", "RWV"];

/// One generated preview.
pub struct Thumb {
    pub bytes: Bytes,
    pub content_type: String,
}

/// A remembered answer. `thumb = None` means "known to have no preview".
struct Entry {
    thumb: Option<Arc<Thumb>>,
    stored_at: Instant,
    last_used: u64,
    /// Approximate memory used (image + key + bookkeeping).
    cost: usize,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<String, Entry>,
    bytes: usize,
    /// Counter increased on every use; the entry with the smallest value is evicted first.
    clock: u64,
}

/// Cheap-to-clone handle to the cache and to the "parallel rendering" gate.
#[derive(Clone)]
pub struct ThumbnailCache {
    inner: Arc<Mutex<Inner>>,
    gate: Arc<Semaphore>,
}

impl ThumbnailCache {
    pub fn new() -> Self {
        Self { inner: Arc::new(Mutex::new(Inner::default())), gate: Arc::new(Semaphore::new(MAX_PARALLEL)) }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `None` = not remembered; `Some(None)` = remembered as "no preview";
    /// `Some(Some(t))` = remembered thumbnail.
    pub fn get(&self, key: &str) -> Option<Option<Arc<Thumb>>> {
        let mut inner = self.lock();
        let expired = match inner.entries.get(key) {
            None => return None,
            Some(e) => e.stored_at.elapsed() > if e.thumb.is_some() { FOUND_TTL } else { MISSING_TTL },
        };
        if expired {
            if let Some(old) = inner.entries.remove(key) {
                inner.bytes -= old.cost;
            }
            return None;
        }
        inner.clock += 1;
        let tick = inner.clock;
        let entry = inner.entries.get_mut(key)?;
        entry.last_used = tick;
        Some(entry.thumb.clone())
    }

    /// Remembers an answer, evicting the least recently used ones while over
    /// `capacity` bytes. A capacity of 0 disables the cache.
    pub fn put(&self, key: String, thumb: Option<Arc<Thumb>>, capacity: usize) {
        let cost = thumb.as_ref().map(|t| t.bytes.len()).unwrap_or(0) + key.len() + 64;
        if capacity == 0 || cost > capacity {
            return;
        }
        let mut inner = self.lock();
        inner.clock += 1;
        let tick = inner.clock;
        let entry = Entry { thumb, stored_at: Instant::now(), last_used: tick, cost };
        if let Some(previous) = inner.entries.insert(key, entry) {
            inner.bytes -= previous.cost;
        }
        inner.bytes += cost;
        Self::evict(&mut inner, capacity);
    }

    /// Shrinks the cache to `capacity` bytes (0 empties it). Called when the settings change.
    pub fn trim(&self, capacity: usize) {
        let mut inner = self.lock();
        if capacity == 0 {
            inner.entries.clear();
            inner.bytes = 0;
        } else {
            Self::evict(&mut inner, capacity);
        }
    }

    fn evict(inner: &mut Inner, capacity: usize) {
        while inner.bytes > capacity {
            let oldest = inner.entries.iter().min_by_key(|(_, e)| e.last_used).map(|(k, _)| k.clone());
            match oldest {
                Some(key) => {
                    if let Some(removed) = inner.entries.remove(&key) {
                        inner.bytes -= removed.cost;
                    }
                }
                None => break,
            }
        }
    }

    /// Waits for one of the [`MAX_PARALLEL`] rendering slots.
    pub async fn permit(&self) -> AppResult<OwnedSemaphorePermit> {
        self.gate.clone().acquire_owned().await.map_err(|_| AppError::Internal("the thumbnail gate was closed".into()))
    }
}

/// Instances to try for a series, best first: the middle one, then the first and the last.
fn candidates(ids: &[String]) -> Vec<&String> {
    let mut chosen: Vec<&String> = Vec::new();
    if ids.is_empty() {
        return chosen;
    }
    for index in [ids.len() / 2, 0, ids.len() - 1] {
        if !chosen.contains(&&ids[index]) {
            chosen.push(&ids[index]);
        }
    }
    chosen
}

/// Preview of a series (its middle image). `Ok(None)` = no image could be rendered.
/// A series that does not exist gives `Err(NotFound)`.
pub async fn series_thumbnail(orthanc: &OrthancClient, series_id: &str, width: u32) -> AppResult<Option<Thumb>> {
    let series = orthanc.get_json(&format!("/series/{series_id}")).await?;
    let ids: Vec<String> = series["Instances"]
        .as_array()
        .map(|items| items.iter().filter_map(|v| v.as_str()).map(str::to_string).collect())
        .unwrap_or_default();

    for instance in candidates(&ids) {
        if let Some((bytes, content_type)) = orthanc.rendered_image(instance, width).await? {
            return Ok(Some(Thumb { bytes, content_type }));
        }
    }
    Ok(None)
}

/// Preview of a study: the middle image of its most substantial picture series.
pub async fn study_thumbnail(orthanc: &OrthancClient, study_id: &str, width: u32) -> AppResult<Option<Thumb>> {
    let series_list = orthanc.get_json(&format!("/studies/{study_id}/series")).await?;

    // (number of images, series number, Orthanc id) of every series that holds pictures.
    let mut options: Vec<(usize, i64, String)> = series_list
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter(|s| {
                    let modality = s["MainDicomTags"]["Modality"].as_str().unwrap_or_default().to_uppercase();
                    !NON_IMAGE_MODALITIES.contains(&modality.as_str())
                })
                .filter_map(|s| {
                    let id = s["ID"].as_str()?.to_string();
                    let images = s["Instances"].as_array().map(|a| a.len()).unwrap_or(0);
                    let number = s["MainDicomTags"]["SeriesNumber"]
                        .as_str()
                        .and_then(|n| n.trim().parse::<i64>().ok())
                        .unwrap_or(i64::MAX);
                    Some((images, number, id))
                })
                .collect()
        })
        .unwrap_or_default();

    // Most images first; the lowest series number wins a tie.
    options.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));

    // Try the three best series: a study whose biggest series cannot be decoded
    // (unsupported compression...) should still get a picture from another one.
    for (_, _, series_id) in options.into_iter().take(3) {
        if let Some(thumb) = series_thumbnail(orthanc, &series_id, width).await? {
            return Ok(Some(thumb));
        }
    }
    Ok(None)
}
