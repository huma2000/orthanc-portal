//! # routes/export.rs
//!
//! **Patient CD / USB export**: the classic "burn a CD for the patient". One ZIP to unpack onto a CD,
//! DVD or USB stick:
//!
//! ```text
//! DICOMDIR, IMAGES/...     the images with their DICOMDIR index (made by Orthanc, /tools/create-media)
//! README.html              a page for the patient: who, which studies, how to look at them
//! <the viewer's files>     a portable DICOM viewer, when an administrator gave its folder
//!                          (Settings > Features > Folder of the CD viewer), e.g. Weasis portable
//!                          with its autorun.inf
//! ```
//!
//! * `GET /api/export/cd?studies=<id>,<id>`  (permission `download`; Settings > Features, on by default)
//!
//! Orthanc's archive is first written to disk, then its entries are copied into the final ZIP exactly
//! as they are (no recompression), next to the page and the viewer, then the result is streamed to
//! the browser and deleted. Both files live in `<folder of the database>/exports` (in Docker the
//! `/data` volume: the rest of the container is read-only). At most two exports are built at once.

use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use crate::models::StudySummary;
use crate::permissions::Permission;
use crate::settings::Language;
use crate::state::AppState;
use crate::util::{feature, now_unix, orthanc_id, random_token};
use axum::body::{Body, Bytes};
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::Response;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::json;
use std::io::Write;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Exports built at the same time (each needs about twice its size on disk for a moment).
static EXPORT_GATE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
/// Most files copied from the viewer's folder (a portable Weasis has a few thousand).
const MAX_VIEWER_FILES: usize = 20_000;

/// Deletes its file when dropped: at the end of the download, or on any failure on the way.
struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Where exports are assembled; files left over by an interrupted program are removed at start-up.
pub fn export_dir(state: &AppState) -> PathBuf {
    state.db.data_dir().join("exports")
}

/// Removes what an earlier run left in the exports folder (main.rs, at start-up).
pub fn clean_export_dir(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[derive(Deserialize)]
pub struct ExportQuery {
    /// Orthanc ids separated by commas (1 to 100).
    studies: String,
}

pub async fn cd_export(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Query(q): Query<ExportQuery>,
) -> AppResult<Response> {
    user.require(Permission::Download)?;
    let settings = state.settings.get();
    feature(settings.cd_export, "The patient CD / USB export is switched off")?;
    let ids: Vec<String> = q.studies.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect();
    if ids.is_empty() || ids.len() > 100 {
        return Err(AppError::BadRequest("Choose 1 to 100 studies".into()));
    }
    for id in &ids {
        orthanc_id(id)?;
    }
    let _slot = EXPORT_GATE.try_acquire().map_err(|_| {
        AppError::TooManyRequests("Two exports are being prepared already: try again in a minute".into())
    })?;

    // The studies (for the patient's page and the file name); this also checks that they exist.
    let mut studies = Vec::with_capacity(ids.len());
    for id in &ids {
        studies.push(StudySummary::from_orthanc(&state.orthanc.get_json(&format!("/studies/{id}")).await?));
    }

    let dir = export_dir(&state);
    std::fs::create_dir_all(&dir).map_err(|e| AppError::Internal(format!("cannot create {}: {e}", dir.display())))?;
    let base = random_token()[..16].to_string();
    let media = TempFile(dir.join(format!("{base}-media.zip")));
    let output = TempFile(dir.join(format!("{base}.zip")));

    // 1. Orthanc's DICOMDIR archive, to disk.
    let upstream =
        state.orthanc.post_stream("/tools/create-media", &json!({ "Resources": ids, "Synchronous": true })).await?;
    {
        let mut file = tokio::fs::File::create(&media.0)
            .await
            .map_err(|e| AppError::Internal(format!("cannot write the export: {e}")))?;
        let mut chunks = upstream.bytes_stream();
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk?;
            file.write_all(&chunk).await.map_err(|e| AppError::Internal(format!("cannot write the export: {e}")))?;
        }
        file.flush().await.map_err(|e| AppError::Internal(format!("cannot write the export: {e}")))?;
    }

    // 2. The final ZIP: Orthanc's entries as they are, the page for the patient, the viewer.
    let page = patient_page(&settings.portal_name, &studies, settings.language);
    let viewer_dir = settings.cd_viewer_dir.clone();
    let (media_path, output_path) = (media.0.clone(), output.0.clone());
    let viewer_files = tokio::task::spawn_blocking(move || assemble(&media_path, &output_path, &page, &viewer_dir))
        .await
        .map_err(|e| AppError::Internal(format!("export task failed: {e}")))??;
    drop(media); // no longer needed

    let size = std::fs::metadata(&output.0).map(|m| m.len()).unwrap_or(0);
    let template = &settings.download_name_template;
    let file_name = if studies.len() == 1 {
        crate::routes::files::study_file_name(template, &studies[0], &ids[0]).replace(".zip", "_CD.zip")
    } else {
        format!("{}_studies_CD.zip", studies.len())
    };
    state.db.audit(
        &user.username,
        "export_cd",
        &format!("{} studies: {} ({file_name}, viewer files: {viewer_files})", ids.len(), ids.join(",")),
    );

    // 3. Stream it, then delete it (the guard travels with the stream).
    let file = tokio::fs::File::open(&output.0)
        .await
        .map_err(|e| AppError::Internal(format!("cannot read the export: {e}")))?;
    let stream = futures_util::stream::unfold((file, output), |(mut file, guard)| async move {
        let mut buffer = vec![0u8; 256 * 1024];
        match file.read(&mut buffer).await {
            Ok(0) => None,
            Ok(n) => {
                buffer.truncate(n);
                Some((Ok::<Bytes, std::io::Error>(Bytes::from(buffer)), (file, guard)))
            }
            Err(e) => Some((Err(e), (file, guard))),
        }
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/zip")
        .header(header::CONTENT_LENGTH, size)
        .header(header::CONTENT_DISPOSITION, format!("attachment; filename=\"{file_name}\""))
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(stream))
        .map_err(|e| AppError::Internal(format!("cannot build the export response: {e}")))
}

/// Builds the final ZIP (blocking: runs on a worker thread). Returns how many viewer files were added.
fn assemble(media: &Path, output: &Path, page: &str, viewer_dir: &str) -> AppResult<usize> {
    let fail = |what: &str, e: &dyn std::fmt::Display| AppError::Internal(format!("export: {what}: {e}"));
    let mut archive = zip::ZipArchive::new(std::fs::File::open(media).map_err(|e| fail("open", &e))?)
        .map_err(|e| fail("read", &e))?;
    let mut writer = zip::ZipWriter::new(std::fs::File::create(output).map_err(|e| fail("create", &e))?);
    let mut taken: Vec<String> = Vec::with_capacity(archive.len() + 1);
    for i in 0..archive.len() {
        let entry = archive.by_index_raw(i).map_err(|e| fail("entry", &e))?;
        taken.push(entry.name().to_ascii_uppercase());
        writer.raw_copy_file(entry).map_err(|e| fail("copy", &e))?;
    }

    let stored =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored).large_file(false);
    writer.start_file("README.html", stored).map_err(|e| fail("page", &e))?;
    writer.write_all(page.as_bytes()).map_err(|e| fail("page", &e))?;
    taken.push("README.HTML".into());

    let mut added = 0;
    if !viewer_dir.is_empty() {
        let root = Path::new(viewer_dir);
        if !root.is_dir() {
            return Err(AppError::BadRequest(format!(
                "The folder of the CD viewer ({viewer_dir}) does not exist on the portal's computer (Settings > Features)"
            )));
        }
        let mut pending = vec![root.to_path_buf()];
        while let Some(folder) = pending.pop() {
            let entries = std::fs::read_dir(&folder).map_err(|e| fail("viewer folder", &e))?;
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(kind) = entry.file_type() else { continue };
                if kind.is_symlink() {
                    continue; // never follow links out of the viewer's folder
                }
                if kind.is_dir() {
                    pending.push(path);
                    continue;
                }
                let Ok(relative) = path.strip_prefix(root) else { continue };
                let name: String = relative
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                let upper = name.to_ascii_uppercase();
                // The images and the index belong to Orthanc's part: never overwritten by the viewer.
                if taken.contains(&upper) || upper.starts_with("IMAGES/") {
                    continue;
                }
                if added >= MAX_VIEWER_FILES {
                    return Err(AppError::BadRequest(format!(
                        "The folder of the CD viewer has more than {MAX_VIEWER_FILES} files"
                    )));
                }
                let mut source = std::fs::File::open(&path).map_err(|e| fail("viewer file", &e))?;
                let big = source.metadata().map(|m| m.len() >= u32::MAX as u64).unwrap_or(false);
                writer.start_file(name, stored.large_file(big)).map_err(|e| fail("viewer file", &e))?;
                std::io::copy(&mut source, &mut writer).map_err(|e| fail("viewer file", &e))?;
                added += 1;
            }
        }
    }
    writer.finish().map_err(|e| fail("finish", &e))?;
    Ok(added)
}

/// Escapes text for HTML.
fn esc(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// `DOE^JOHN` -> `DOE JOHN`; `20240517` -> `2024-05-17`.
fn plain_name(raw: &str) -> String {
    raw.split('^').map(str::trim).filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" ")
}
fn plain_date(raw: &str) -> String {
    if raw.len() == 8 && raw.chars().all(|c| c.is_ascii_digit()) {
        format!("{}-{}-{}", &raw[..4], &raw[4..6], &raw[6..])
    } else {
        raw.to_string()
    }
}

/// The page for the patient (no script, no external resource: it opens from the disc as it is).
pub fn patient_page(portal: &str, studies: &[StudySummary], lang: Language) -> String {
    let (title, intro, how, cols, made) = match lang {
        Language::En => (
            "Your imaging studies",
            "This disc (or USB stick) holds the images of the studies below, in the standard DICOM format.",
            "To look at them: open the viewer program on this disc if there is one (on Windows it may start by \
             itself), or any DICOM viewer, and open the file DICOMDIR. Your doctor can also import them into their own system.",
            ["Date", "Study", "Modalities", "Images"],
            "Prepared by",
        ),
        Language::Es => (
            "Sus estudios de imagen",
            "Este disco (o memoria USB) contiene las imágenes de los estudios siguientes, en el formato estándar DICOM.",
            "Para verlas: abra el programa visor de este disco si lo hay (en Windows puede abrirse solo), o \
             cualquier visor DICOM, y abra el archivo DICOMDIR. Su médico también puede importarlas a su propio sistema.",
            ["Fecha", "Estudio", "Modalidades", "Imágenes"],
            "Preparado por",
        ),
    };
    let patient = studies.first().map(|s| plain_name(&s.patient_name)).unwrap_or_default();
    let patient_id = studies.first().map(|s| s.patient_id.clone()).unwrap_or_default();
    let rows: String = studies
        .iter()
        .map(|s| {
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                esc(&plain_date(&s.study_date)),
                esc(&s.study_description),
                esc(&s.modalities.join(", ")),
                s.instances_count.map(|n| n.to_string()).unwrap_or_default()
            )
        })
        .collect();
    let day = {
        // The date of the export, from the clock (UTC), without a date library.
        let days = now_unix().div_euclid(86_400);
        let (y, m, d) = civil_from_days(days);
        format!("{y:04}-{m:02}-{d:02}")
    };
    format!(
        "<!doctype html>\n<html lang=\"{lang}\"><head><meta charset=\"utf-8\"><title>{title}</title>\
         <style>body{{font-family:system-ui,sans-serif;max-width:46rem;margin:2rem auto;padding:0 1rem;color:#15212c}}\
         table{{border-collapse:collapse;width:100%}}td,th{{border-bottom:1px solid #ccd;padding:.4rem;text-align:left}}\
         .muted{{color:#556}}</style></head><body>\
         <h1>{title}</h1><p><b>{patient}</b>{pid}</p><p>{intro}</p>\
         <table><thead><tr><th>{c0}</th><th>{c1}</th><th>{c2}</th><th>{c3}</th></tr></thead><tbody>{rows}</tbody></table>\
         <p>{how}</p><p class=\"muted\">{made} {portal}, {day}.</p></body></html>\n",
        lang = lang.code(),
        title = title,
        patient = esc(&patient),
        pid = if patient_id.is_empty() { String::new() } else { format!(" ({})", esc(&patient_id)) },
        intro = intro,
        c0 = cols[0],
        c1 = cols[1],
        c2 = cols[2],
        c3 = cols[3],
        rows = rows,
        how = how,
        made = made,
        portal = esc(portal),
        day = day,
    )
}

/// Days since 1970-01-01 -> (year, month, day) (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_without_a_library() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_860), (2024, 5, 17));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }

    #[test]
    fn the_patient_page_escapes_what_comes_from_dicom() {
        let study = StudySummary::from_orthanc(&json!({
            "ID": "a", "MainDicomTags": { "StudyDate": "20240517", "StudyDescription": "CT <script>alert(1)</script>" },
            "PatientMainDicomTags": { "PatientName": "DOE^JOHN", "PatientID": "P-001" },
        }));
        let page = patient_page("Clinic & Co", &[study], Language::En);
        assert!(page.contains("DOE JOHN") && page.contains("2024-05-17") && page.contains("Clinic &amp; Co"));
        assert!(!page.contains("<script>") && page.contains("&lt;script&gt;"));
    }

    /// The final ZIP keeps Orthanc's entries as they were and adds the page and the viewer.
    #[test]
    fn the_export_keeps_the_images_and_adds_the_rest() {
        let dir = std::env::temp_dir().join(format!("portal-export-test-{}", random_token()));
        std::fs::create_dir_all(dir.join("viewer/bin")).unwrap();
        std::fs::write(dir.join("viewer/autorun.inf"), "[autorun]\nopen=bin/viewer.exe\n").unwrap();
        std::fs::write(dir.join("viewer/bin/viewer.exe"), b"MZ").unwrap();
        std::fs::write(dir.join("viewer/DICOMDIR"), b"must not replace the real one").unwrap();
        let media = dir.join("media.zip");
        {
            let mut w = zip::ZipWriter::new(std::fs::File::create(&media).unwrap());
            let o = zip::write::SimpleFileOptions::default();
            w.start_file("DICOMDIR", o).unwrap();
            w.write_all(b"real index").unwrap();
            w.start_file("IMAGES/IM0", o).unwrap();
            w.write_all(b"pixels").unwrap();
            w.finish().unwrap();
        }
        let out = dir.join("out.zip");
        let added = assemble(&media, &out, "<html>page</html>", &dir.join("viewer").to_string_lossy()).unwrap();
        assert_eq!(added, 2, "autorun.inf and bin/viewer.exe, not the second DICOMDIR");
        let mut z = zip::ZipArchive::new(std::fs::File::open(&out).unwrap()).unwrap();
        let mut names: Vec<String> = (0..z.len()).map(|i| z.by_index(i).unwrap().name().to_string()).collect();
        names.sort();
        assert_eq!(names, ["DICOMDIR", "IMAGES/IM0", "README.html", "autorun.inf", "bin/viewer.exe"]);
        let mut index = String::new();
        std::io::Read::read_to_string(&mut z.by_name("DICOMDIR").unwrap(), &mut index).unwrap();
        assert_eq!(index, "real index");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
