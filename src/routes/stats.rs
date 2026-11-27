//! # routes/stats.rs
//!
//! **Configuration > Statistics** (administrators): how the archive and the portal are used.
//!
//! * `GET /api/admin/stats[?refresh=true]`
//!
//! * studies per month for the last 12 months (by the date of the study), in total and for each
//!   modality button of the search pages; counted by Orthanc (`/tools/count-resources`);
//! * the size of the archive (`/statistics`);
//! * share links created / opened / downloaded and upload links per month, the links active now,
//!   transfers per month, who uploads and who shares the most (last 90 days) - from the portal's
//!   own records (the audit log keeps them as long as Settings > Security & roles says).
//!
//! Months are calendar months in UTC. The answer is remembered for 10 minutes (counting asks Orthanc
//! about a hundred small questions); `refresh=true` counts again.

use crate::auth::AuthUser;
use crate::error::AppResult;
use crate::orthanc::OrthancClient;
use crate::permissions::Permission;
use crate::servers::Chosen;
use crate::state::AppState;
use crate::util::{civil_from_days, days_from_civil, feature, now_unix};
use axum::extract::{Query, State};
use axum::Json;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Mutex;

const CACHE_SECS: i64 = 600;
const MONTHS: usize = 12;

/// The last answer of each server (by key) and when it was computed.
static CACHE: Mutex<BTreeMap<String, (i64, Value)>> = Mutex::new(BTreeMap::new());

/// The last `MONTHS` months, oldest first: (year, month).
pub fn last_months(now: i64) -> Vec<(i64, u32)> {
    let (mut y, mut m, _) = civil_from_days(now.div_euclid(86_400));
    let mut out = Vec::with_capacity(MONTHS);
    for _ in 0..MONTHS {
        out.push((y, m));
        if m == 1 {
            y -= 1;
            m = 12;
        } else {
            m -= 1;
        }
    }
    out.reverse();
    out
}

/// The DICOM date range of a month: `20240201-20240229`.
fn month_range(y: i64, m: u32) -> String {
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    let last = civil_from_days(days_from_civil(ny, nm) - 1).2;
    format!("{y:04}{m:02}01-{y:04}{m:02}{last:02}")
}

#[derive(Deserialize)]
pub struct StatsQuery {
    #[serde(default)]
    refresh: bool,
}

pub async fn overview(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    chosen: Chosen,
    Query(q): Query<StatsQuery>,
) -> AppResult<Json<Value>> {
    user.require(Permission::Admin)?;
    feature(state.settings.get().stats_page, "The Statistics page is switched off")?;
    // The studies counted are those of the chosen server (`?server=`; the default one without it).
    let orthanc = state.orthancs.chosen(&user, &chosen)?;
    let key = chosen.key().to_string();
    let now = now_unix();
    if !q.refresh {
        if let Some((at, value)) = CACHE.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
            if now - at < CACHE_SECS {
                return Ok(Json(value.clone()));
            }
        }
    }
    let value = compute(&state, &orthanc, now).await?;
    CACHE.lock().unwrap_or_else(|e| e.into_inner()).insert(key, (now, value.clone()));
    Ok(Json(value))
}

async fn compute(state: &AppState, orthanc: &OrthancClient, now: i64) -> AppResult<Value> {
    let settings = state.settings.get();
    let months = last_months(now);
    let keys: Vec<String> = months.iter().map(|(y, m)| format!("{y:04}-{m:02}")).collect();

    // ---- studies per month, total and per modality button (Orthanc counts)
    let buttons = settings.visible_modality_buttons();
    let mut questions: Vec<(usize, Option<usize>, Value)> = Vec::new(); // (month, button, query)
    for (i, (y, m)) in months.iter().enumerate() {
        let range = month_range(*y, *m);
        questions.push((i, None, json!({ "StudyDate": range })));
        for (b, button) in buttons.iter().enumerate() {
            let codes: Vec<String> = button
                .codes
                .iter()
                .flat_map(|c| if c == "RX" { vec!["CR".to_string(), "DX".to_string()] } else { vec![c.clone()] })
                .collect();
            questions.push((i, Some(b), json!({ "StudyDate": range, "ModalitiesInStudy": codes.join("\\") })));
        }
    }
    let answers: Vec<(usize, Option<usize>, Option<u64>)> = futures_util::stream::iter(questions)
        .map(|(i, b, query)| async move {
            let body = json!({ "Level": "Study", "Query": query });
            let count = orthanc.post_json("/tools/count-resources", &body).await.ok().and_then(|v| v["Count"].as_u64());
            (i, b, count)
        })
        .buffer_unordered(8)
        .collect()
        .await;
    let mut total = vec![0u64; MONTHS];
    let mut per_button = vec![vec![0u64; MONTHS]; buttons.len()];
    let mut counted = true;
    for (i, b, count) in answers {
        let Some(count) = count else {
            counted = false;
            continue;
        };
        match b {
            None => total[i] = count,
            Some(b) => per_button[b][i] = count,
        }
    }
    let by_modality: Vec<Value> = buttons
        .iter()
        .zip(per_button)
        .filter(|(_, counts)| counts.iter().any(|c| *c > 0))
        .map(|(b, counts)| json!({ "label": b.label, "title": b.title, "counts": counts }))
        .collect();

    // ---- the archive
    let storage = orthanc.get_json("/statistics").await.unwrap_or(Value::Null);

    // ---- the portal's own records
    let since = {
        let (y, m) = months[0];
        days_from_civil(y, m) * 86_400
    };
    let per_month = |rows: Vec<(String, i64)>| -> Vec<i64> {
        keys.iter().map(|k| rows.iter().find(|(month, _)| month == k).map(|(_, n)| *n).unwrap_or(0)).collect()
    };
    let audit = state.db.audit_per_month(
        &["share_unlock", "share_download", "upload_link_study", "login", "download_study", "export_cd"],
        since,
    )?;
    let audit_series = |action: &str| -> Vec<i64> {
        per_month(audit.iter().filter(|(a, _, _)| a == action).map(|(_, m, n)| (m.clone(), *n)).collect())
    };
    let (active_shares, active_uploads) = state.db.active_link_counts(now)?;
    let ninety_days = now - 90 * 86_400;
    let top = |action: &str| -> AppResult<Vec<Value>> {
        Ok(state
            .db
            .top_users(action, ninety_days, 8)?
            .into_iter()
            .map(|(u, n)| json!({ "user": u, "count": n }))
            .collect())
    };

    Ok(json!({
        "generated_at": now,
        "months": keys,
        "studies": {
            "counted": counted,
            "total": total,
            "by_modality": by_modality,
        },
        "storage": {
            "disk_mb": storage["TotalDiskSizeMB"],
            "patients": storage["CountPatients"],
            "studies": storage["CountStudies"],
            "series": storage["CountSeries"],
            "instances": storage["CountInstances"],
        },
        "links": {
            "shares_created": per_month(state.db.rows_per_month("shares", since)?),
            "shares_opened": audit_series("share_unlock"),
            "shares_downloaded": audit_series("share_download"),
            "upload_links_created": per_month(state.db.rows_per_month("upload_links", since)?),
            "studies_received": audit_series("upload_link_study"),
            "active_shares": active_shares,
            "active_upload_links": active_uploads,
        },
        "activity": {
            "sign_ins": audit_series("login"),
            "downloads": audit_series("download_study"),
            "cd_exports": audit_series("export_cd"),
            "transfers": per_month(state.db.rows_per_month("transfers", since)?),
        },
        "top": {
            "uploaders": top("upload")?,
            "sharers": top("share_created")?,
            "downloaders": top("download_study")?,
        },
        "audit_retention_days": settings.audit_retention_days,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn months_and_ranges() {
        // 2024-11-19
        let months = last_months(1_731_985_500);
        assert_eq!(months.len(), 12);
        assert_eq!(months[0], (2023, 12));
        assert_eq!(months[11], (2024, 11));
        assert_eq!(month_range(2024, 2), "20240201-20240229");
        assert_eq!(month_range(2023, 2), "20230201-20230228");
        assert_eq!(month_range(2024, 12), "20241201-20241231");
        assert_eq!(civil_from_days(days_from_civil(2024, 3)), (2024, 3, 1));
    }
}
