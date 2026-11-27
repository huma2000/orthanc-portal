//! # routes/export.rs
//!
//! **Patient CD / USB export**: the classic "burn a CD for the patient". One ZIP to unpack onto a CD,
//! DVD or USB stick:
//!
//! ```text
//! DICOMDIR, IMAGES/...     the images with their DICOMDIR index (made by Orthanc, /tools/create-media)
//! README.html              a page for the patient: who, which studies, how to look at them
//! viewer/, Autorun.inf,    the viewer (Settings > Features > Viewer on the CD): Weasis, kept
//! RUN.bat                  compressed by the portal and set up for this CD (see cdviewer.rs), or
//!                          the files of a folder an administrator gave
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
use crate::util::{civil_from_days, feature, now_unix, random_token, temp_file_download, TempFile};
use axum::extract::{Query, State};
use axum::response::Response;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::json;
use std::io::Write;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

/// Exports built at the same time (each needs about twice its size on disk for a moment).
static EXPORT_GATE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
/// Most files copied from the viewer's folder (a portable Weasis has a few thousand).
const MAX_VIEWER_FILES: usize = 20_000;

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
    let raw: Vec<&str> = ids.iter().map(String::as_str).collect();
    // The audit log names the studies with their server (`key:id`).
    let named = raw.join(",");
    let (orthanc, _, ids) = state.orthancs.resolve_together(&user, &raw)?;
    let _slot = EXPORT_GATE.try_acquire().map_err(|_| {
        AppError::TooManyRequests("Two exports are being prepared already: try again in a minute".into())
    })?;

    // The studies (for the patient's page and the file name); this also checks that they exist.
    let orthanc = &orthanc;
    let read: Vec<AppResult<serde_json::Value>> = futures_util::stream::iter(ids.clone())
        .map(|id| async move { orthanc.get_json(&format!("/studies/{id}")).await })
        .buffered(6)
        .collect()
        .await;
    let mut studies = Vec::with_capacity(ids.len());
    for study in read {
        studies.push(StudySummary::from_orthanc(&study?));
    }

    let dir = export_dir(&state);
    std::fs::create_dir_all(&dir).map_err(|e| AppError::Internal(format!("cannot create {}: {e}", dir.display())))?;
    let base = random_token()[..16].to_string();
    let media = TempFile(dir.join(format!("{base}-media.zip")));
    let output = TempFile(dir.join(format!("{base}.zip")));

    // 1. Orthanc's DICOMDIR archive, to disk.
    let upstream =
        orthanc.post_stream("/tools/create-media", &json!({ "Resources": ids, "Synchronous": true })).await?;
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
    let viewer = match settings.cd_viewer.as_str() {
        "weasis" if crate::cdviewer::package_ready_off_thread(&state).await => Viewer::Weasis {
            package: crate::cdviewer::package_path(&state),
            cd: crate::cdviewer::CdBranding {
                name: crate::cdviewer::viewer_name(&state),
                locked: settings.cd_weasis_locked,
                splash: crate::cdviewer::splash_image(&state),
            },
        },
        "weasis" => {
            // Not downloaded yet: this CD goes without the viewer (and the download starts, if allowed).
            if settings.cd_weasis_autodownload {
                crate::cdviewer::start_download(&state);
            }
            Viewer::None
        }
        "folder" if !settings.cd_viewer_dir.is_empty() => Viewer::Folder(settings.cd_viewer_dir.clone()),
        _ => Viewer::None,
    };
    let weasis_name = match &viewer {
        Viewer::Weasis { cd, .. } => Some(cd.name.clone()),
        _ => None,
    };
    let page = patient_page(&settings.portal_name, &studies, settings.language, weasis_name.as_deref());
    let (media_path, output_path) = (media.0.clone(), output.0.clone());
    let viewer_files = tokio::task::spawn_blocking(move || assemble(&media_path, &output_path, &page, viewer))
        .await
        .map_err(|e| AppError::Internal(format!("export task failed: {e}")))??;
    drop(media); // no longer needed

    let template = &settings.download_name_template;
    let file_name = if studies.len() == 1 {
        crate::routes::files::study_file_name(template, &studies[0], &ids[0]).replace(".zip", "_CD.zip")
    } else {
        format!("{}_studies_CD.zip", studies.len())
    };
    state.db.audit(
        &user.username,
        "export_cd",
        &format!("{} studies: {named} ({file_name}, viewer files: {viewer_files})", ids.len()),
    );

    // 3. Stream it, then delete it.
    temp_file_download(output, "application/zip", &file_name).await
}

/// The viewer put on a CD.
pub enum Viewer {
    None,
    /// The files of a folder (Settings > Features > Folder of the CD viewer).
    Folder(String),
    /// Weasis, from the compressed package, set up for this CD.
    Weasis {
        package: PathBuf,
        cd: crate::cdviewer::CdBranding,
    },
}

/// Builds the final ZIP (blocking: runs on a worker thread). Returns how many viewer files were added.
fn assemble(media: &Path, output: &Path, page: &str, viewer: Viewer) -> AppResult<usize> {
    let fail = |what: &str, e: &dyn std::fmt::Display| AppError::Internal(format!("export: {what}: {e}"));
    let mut archive = zip::ZipArchive::new(std::fs::File::open(media).map_err(|e| fail("open", &e))?)
        .map_err(|e| fail("read", &e))?;
    let mut writer = zip::ZipWriter::new(std::fs::File::create(output).map_err(|e| fail("create", &e))?);
    let mut taken: Vec<String> = Vec::with_capacity(archive.len() + 1);
    for i in 0..archive.len() {
        let entry = archive.by_index_raw(i).map_err(|e| fail("entry", &e))?;
        taken.push(entry.name().map_err(|e| fail("entry name", &e))?.to_ascii_uppercase());
        writer.raw_copy_file(entry).map_err(|e| fail("copy", &e))?;
    }

    let stored =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored).large_file(false);
    writer.start_file("README.html", stored).map_err(|e| fail("page", &e))?;
    writer.write_all(page.as_bytes()).map_err(|e| fail("page", &e))?;
    taken.push("README.HTML".into());

    let mut added = 0;
    if let Viewer::Weasis { package, cd } = &viewer {
        added = add_weasis(&mut writer, &taken, package, cd).map_err(|e| fail("CD viewer", &e))?;
    }
    if let Viewer::Folder(viewer_dir) = &viewer {
        let viewer_dir = viewer_dir.as_str();
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

/// Weasis on the CD: the package's entries copied as they are (never unpacked or recompressed),
/// except `viewer/app/Weasis.cfg`, rewritten for the CD; then the CD's settings, its splash image,
/// `Autorun.inf` and `RUN.bat`. Returns the number of files added.
fn add_weasis(
    writer: &mut zip::ZipWriter<std::fs::File>,
    taken: &[String],
    package: &Path,
    cd: &crate::cdviewer::CdBranding,
) -> Result<usize, String> {
    use std::io::Read as _;
    let mut archive =
        zip::ZipArchive::new(std::fs::File::open(package).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let mut original_cfg = None;
    let mut added = 0;
    for i in 0..archive.len() {
        let entry = archive.by_index_raw(i).map_err(|e| e.to_string())?;
        let name = entry.name().map_err(|e| e.to_string())?.to_string();
        // Only the viewer's own folder, never a name of Orthanc's part or one going elsewhere.
        if !name.starts_with("viewer/")
            || name.contains("..")
            || name.contains('\\')
            || taken.contains(&name.to_ascii_uppercase())
        {
            continue;
        }
        if name == "viewer/app/Weasis.cfg" {
            drop(entry);
            let mut text = String::new();
            // A launcher configuration is a few hundred bytes: at most 1 MB is read.
            archive
                .by_index(i)
                .map_err(|e| e.to_string())?
                .take(1024 * 1024)
                .read_to_string(&mut text)
                .map_err(|e| e.to_string())?;
            original_cfg = Some(text);
            continue;
        }
        writer.raw_copy_file(entry).map_err(|e| e.to_string())?;
        added += 1;
    }
    let original_cfg = original_cfg.ok_or("the package has no viewer/app/Weasis.cfg")?;
    let small = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let splash_name = cd.splash.as_ref().map(|(_, ext)| format!("portal-splash.{ext}"));
    let files: [(String, Vec<u8>); 4] = [
        (
            "viewer/app/Weasis.cfg".into(),
            crate::cdviewer::weasis_cfg(&original_cfg, splash_name.as_deref()).into_bytes(),
        ),
        ("viewer/app/conf/portal-cd.json".into(), crate::cdviewer::portal_cd_json(cd).into_bytes()),
        ("Autorun.inf".into(), crate::cdviewer::autorun_inf(&cd.name).into_bytes()),
        ("RUN.bat".into(), crate::cdviewer::run_bat().into_bytes()),
    ];
    for (name, data) in files {
        writer.start_file(name, small).map_err(|e| e.to_string())?;
        writer.write_all(&data).map_err(|e| e.to_string())?;
        added += 1;
    }
    // The splash image, written from the settings' copy (not copied again).
    if let (Some((data, _)), Some(file)) = (&cd.splash, &splash_name) {
        writer.start_file(format!("viewer/app/resources/images/{file}"), small).map_err(|e| e.to_string())?;
        writer.write_all(data).map_err(|e| e.to_string())?;
        added += 1;
    }
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
pub fn patient_page(portal: &str, studies: &[StudySummary], lang: Language, weasis: Option<&str>) -> String {
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
         <p>{how}</p>{viewer}<p class=\"muted\">{made} {portal}, {day}.</p></body></html>\n",
        lang = lang.code(),
        // Weasis on the CD: how to start it on Windows, and its licence.
        viewer = match weasis {
            None => String::new(),
            Some(name) => match lang {
                Language::En => format!(
                    "<p><b>On Windows:</b> double-click <b>RUN.bat</b> on this disc to open the images in the viewer \
                     ({}, based on Weasis). The viewer is free software: see viewer/LICENSES/NOTICE.txt.</p>",
                    esc(name)
                ),
                Language::Es => format!(
                    "<p><b>En Windows:</b> haga doble clic en <b>RUN.bat</b> en este disco para abrir las imágenes en \
                     el visor ({}, basado en Weasis). El visor es software libre: vea viewer/LICENSES/NOTICE.txt.</p>",
                    esc(name)
                ),
            },
        },
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_patient_page_escapes_what_comes_from_dicom() {
        let study = StudySummary::from_orthanc(&json!({
            "ID": "a", "MainDicomTags": { "StudyDate": "20240517", "StudyDescription": "CT <script>alert(1)</script>" },
            "PatientMainDicomTags": { "PatientName": "DOE^JOHN", "PatientID": "P-001" },
        }));
        let page = patient_page("Clinic & Co", &[study], Language::En, None);
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
        let added = assemble(
            &media,
            &out,
            "<html>page</html>",
            Viewer::Folder(dir.join("viewer").to_string_lossy().into_owned()),
        )
        .unwrap();
        assert_eq!(added, 2, "autorun.inf and bin/viewer.exe, not the second DICOMDIR");
        let mut z = zip::ZipArchive::new(std::fs::File::open(&out).unwrap()).unwrap();
        let mut names: Vec<String> = (0..z.len()).map(|i| z.by_index(i).unwrap().name().unwrap().to_string()).collect();
        names.sort();
        assert_eq!(names, ["DICOMDIR", "IMAGES/IM0", "README.html", "autorun.inf", "bin/viewer.exe"]);
        let mut index = String::new();
        std::io::Read::read_to_string(&mut z.by_name("DICOMDIR").unwrap(), &mut index).unwrap();
        assert_eq!(index, "real index");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
