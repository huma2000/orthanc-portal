//! # cdviewer.rs
//!
//! **Weasis on the patient CD / USB.** The portal puts a portable copy of the Weasis DICOM viewer
//! (https://weasis.org, EPL-2.0 OR Apache-2.0) on the CDs it exports, set to open the images of
//! that CD, optionally **locked** to them, and **branded** with the practice's name and logo.
//!
//! ## The package
//! One ZIP, published next to the portal's releases on GitHub (a pre-release, so the update check
//! never mistakes it for a portal version): the files of the official Weasis Windows installer,
//! unmodified, in `viewer/`, with Weasis' licence texts and a notice in `viewer/LICENSES/`. The
//! portal downloads it once (Settings > Features: *Download Weasis*, or by itself when *download
//! it automatically* is on), checks its SHA-256 (fixed in this file), and keeps it **compressed** in
//! `<folder of the database>/cd-viewer/`. Its entries are copied into each CD export as they are,
//! without unpacking or recompressing them. A site without internet copies the file there by hand.
//! `CD_VIEWER_URL` / `CD_VIEWER_SHA256` (environment) replace the address and checksum (a mirror;
//! the tests use a small fake package).
//!
//! ## What each CD gets
//! Next to Orthanc's `DICOMDIR` and images: `viewer/` (Weasis), `Autorun.inf` and `RUN.bat` (start
//! Weasis on the images of the CD, as Weasis' own CD writer does), and two configuration files:
//! `viewer/app/conf/portal-cd.json` (the CD's settings) and `viewer/app/Weasis.cfg` (with the lines
//! that make Weasis read them, a control port of its own and the splash image). No program file of
//! Weasis is changed.
//!
//! ## Locked to the CD
//! Weasis has no "this study only" mode; the lock removes what would let the CD's copy reach other
//! data: the components that send, query / retrieve and write CDs are not loaded at all, the DICOM
//! and image import menus and the DICOM export are hidden, it keeps its preferences apart from any
//! Weasis already on the computer (its own profile) and it uses a control port of its own (so the
//! CD's command never goes to another running Weasis). Dropping files on its window cannot be
//! switched off: this stops casual use for other studies, not a determined user.

use crate::state::AppState;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

/// The Weasis version of the package.
pub const WEASIS_VERSION: &str = "4.8.1";
/// The package's file name (also its name on disk).
pub const PACKAGE_NAME: &str = "weasis-4.8.1-cd-viewer-windows-x64.zip";
/// Where it is published, and its SHA-256: a different file is never kept.
const PACKAGE_URL: &str =
    "https://github.com/huma2000/orthanc-portal/releases/download/weasis-cd-viewer-4.8.1/weasis-4.8.1-cd-viewer-windows-x64.zip";
const PACKAGE_SHA256: &str = "65df004c895f221e54a36fd4b8444b9b3be51c452bd9fa39ea5f99ff4d1f85f6";
/// A package bigger than this is refused while downloading.
const MAX_PACKAGE_BYTES: u64 = 300 * 1024 * 1024;
/// Weasis' control port on the CD (the installed Weasis uses 17179): the CD's command never goes
/// to a Weasis already running on the computer.
const CD_CONTROL_PORT: u32 = 17189;
/// The Weasis profile of the CD: its preferences stay apart from an installed Weasis.
const CD_PROFILE: &str = "portal-cd";

fn package_url() -> String {
    std::env::var("CD_VIEWER_URL")
        .ok()
        .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
        .unwrap_or_else(|| PACKAGE_URL.into())
}

fn package_sha256() -> String {
    std::env::var("CD_VIEWER_SHA256")
        .ok()
        .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_else(|| PACKAGE_SHA256.into())
}

/// Where the package is kept.
pub fn package_path(state: &AppState) -> PathBuf {
    state.db.data_dir().join("cd-viewer").join(PACKAGE_NAME)
}

/// A download under way, or how the last one ended.
#[derive(Default, Clone)]
struct Download {
    running: bool,
    done_bytes: u64,
    total_bytes: Option<u64>,
    error: Option<String>,
    /// (size, modification time) of a file already checked: not hashed again.
    verified: Option<(u64, i64)>,
}

static DOWNLOAD: Mutex<Option<Download>> = Mutex::new(None);

fn download_state() -> Download {
    DOWNLOAD.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_default()
}

fn update(change: impl FnOnce(&mut Download)) {
    let mut guard = DOWNLOAD.lock().unwrap_or_else(|e| e.into_inner());
    change(guard.get_or_insert_with(Download::default));
}

fn file_stamp(path: &std::path::Path) -> Option<(u64, i64)> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64;
    Some((meta.len(), modified))
}

/// Is the package there and the right one? A file copied by hand is checked once (SHA-256), then
/// remembered by its size and date.
pub fn package_ready(state: &AppState) -> bool {
    let path = package_path(state);
    let Some(stamp) = file_stamp(&path) else { return false };
    if download_state().verified == Some(stamp) {
        return true;
    }
    let ok = sha256_of_file(&path).is_some_and(|h| h == package_sha256());
    if ok {
        update(|d| d.verified = Some(stamp));
    }
    ok
}

fn sha256_of_file(path: &std::path::Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).ok()?;
    Some(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// What Settings > Features shows: present (and checked), size, a download under way, an error.
pub fn status(state: &AppState) -> Value {
    let path = package_path(state);
    let d = download_state();
    let ready = !d.running && package_ready(state);
    let present = path.exists();
    json!({
        "version": WEASIS_VERSION,
        "ready": ready,
        "wrong_file": present && !ready && !d.running,
        "size": if ready { std::fs::metadata(&path).map(|m| m.len()).ok() } else { None },
        "downloading": d.running,
        "done_bytes": d.done_bytes,
        "total_bytes": d.total_bytes,
        "error": d.error,
        "path": path.display().to_string(),
        "url": package_url(),
    })
}

/// Starts downloading the package in the background (nothing when one is under way or the right
/// file is already there). The file is written next to its final place, checked, then renamed.
pub fn start_download(state: &AppState) {
    {
        let mut guard = DOWNLOAD.lock().unwrap_or_else(|e| e.into_inner());
        let d = guard.get_or_insert_with(Download::default);
        if d.running {
            return;
        }
        d.running = true;
        d.done_bytes = 0;
        d.total_bytes = None;
        d.error = None;
    }
    let state = state.clone();
    tokio::spawn(async move {
        let outcome = if package_ready(&state) { Ok(()) } else { download(&state).await };
        update(|d| {
            d.running = false;
            d.error = outcome.err();
        });
        if let Some(error) = download_state().error {
            tracing::warn!("the CD viewer could not be downloaded: {error}");
        } else {
            tracing::info!("the CD viewer (Weasis {WEASIS_VERSION}) is ready");
        }
    });
}

async fn download(state: &AppState) -> Result<(), String> {
    use futures_util::StreamExt;
    let path = package_path(state);
    let dir = path.parent().ok_or("no folder")?.to_path_buf();
    std::fs::create_dir_all(&dir).map_err(|e| format!("the folder {} cannot be created: {e}", dir.display()))?;
    let partial = dir.join(format!("{PACKAGE_NAME}.part"));
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(30 * 60))
        .user_agent(concat!("orthanc-portal/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())?;
    let answer = client.get(package_url()).send().await.map_err(|e| format!("it could not be reached: {e}"))?;
    if !answer.status().is_success() {
        return Err(format!("the download answered HTTP {}", answer.status().as_u16()));
    }
    let total = answer.content_length();
    if total.is_some_and(|n| n > MAX_PACKAGE_BYTES) {
        return Err("the file is too big".into());
    }
    update(|d| d.total_bytes = total);
    let mut file = std::fs::File::create(&partial).map_err(|e| format!("{}: {e}", partial.display()))?;
    let mut hasher = Sha256::new();
    let mut done: u64 = 0;
    let mut body = answer.bytes_stream();
    let result: Result<(), String> = async {
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|e| format!("the download stopped: {e}"))?;
            done += chunk.len() as u64;
            if done > MAX_PACKAGE_BYTES {
                return Err("the file is too big".into());
            }
            hasher.update(&chunk);
            file.write_all(&chunk).map_err(|e| format!("it could not be written: {e}"))?;
            update(|d| d.done_bytes = done);
        }
        file.flush().map_err(|e| e.to_string())?;
        Ok(())
    }
    .await;
    drop(file);
    if let Err(e) = result {
        let _ = std::fs::remove_file(&partial);
        return Err(e);
    }
    let got = hex(&hasher.finalize());
    if got != package_sha256() {
        let _ = std::fs::remove_file(&partial);
        return Err(format!("the file is not the expected one (SHA-256 {got}); it was not kept"));
    }
    std::fs::rename(&partial, &path).map_err(|e| format!("it could not be put in place: {e}"))?;
    update(|d| d.verified = file_stamp(&path));
    state.db.audit("portal", "cd_viewer_downloaded", &format!("Weasis {WEASIS_VERSION}, {done} bytes"));
    Ok(())
}

/// At start-up: when the CD viewer is Weasis, "download it automatically" is on and it is missing.
pub fn download_if_wanted(state: &AppState) {
    let settings = state.settings.get();
    if settings.cd_export && settings.cd_viewer == "weasis" && settings.cd_weasis_autodownload && !package_ready(state)
    {
        start_download(state);
    }
}

// ------------------------------------------------------------------ the files of each CD

/// The settings of one CD export (from Settings > Features and Branding).
pub struct CdBranding {
    /// The name in Weasis' window and menus.
    pub name: String,
    /// Locked to the CD (see the module documentation).
    pub locked: bool,
    /// The splash image (PNG, JPEG or GIF) and its extension, if any.
    pub splash: Option<(Vec<u8>, &'static str)>,
}

/// `viewer/app/conf/portal-cd.json`: Weasis' settings for this CD (read through
/// `felix.extended.config.properties`, as Weasis' own Dicomizer does).
pub fn portal_cd_json(cd: &CdBranding) -> String {
    let mut prefs = vec![
        json!({ "code": "weasis.name", "value": cd.name }),
        json!({ "code": "weasis.profile", "value": CD_PROFILE }),
        json!({ "code": "weasis.show.release", "value": "false" }),
        json!({ "code": "weasis.update.release", "value": "false" }),
    ];
    if cd.locked {
        prefs.extend([
            // The components that send, query / retrieve and write CDs are not loaded.
            json!({ "code": "felix.auto.start.110", "value": "",
                    "description": "Locked to this CD: no DICOM send, query / retrieve or CD writer" }),
            json!({ "code": "weasis.import.dicom", "value": "false" }),
            json!({ "code": "weasis.import.dicom.qr", "value": "false" }),
            json!({ "code": "weasis.import.images", "value": "false" }),
            json!({ "code": "weasis.export.dicom", "value": "false" }),
            json!({ "code": "weasis.export.dicom.send", "value": "false" }),
        ]);
    }
    serde_json::to_string_pretty(&json!({ "weasisPreferences": prefs })).unwrap_or_default()
}

/// `viewer/app/Weasis.cfg`, from the package's: reads the CD's settings, uses the CD's control
/// port and (when given) the CD's splash image. Every other line is kept as it is.
pub fn weasis_cfg(original: &str, splash: Option<&str>) -> String {
    let mut out = String::new();
    for line in original.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("java-options=-Dgosh.port=")
            || trimmed.starts_with("java-options=-Dfelix.extended.config.properties=")
            || (splash.is_some() && trimmed.starts_with("java-options=-splash:"))
        {
            continue; // replaced below
        }
        out.push_str(line);
        out.push_str("\r\n");
        if trimmed == "[JavaOptions]" {
            out.push_str("java-options=-Dfelix.extended.config.properties=file:conf/portal-cd.json\r\n");
            out.push_str(&format!("java-options=-Dgosh.port={CD_CONTROL_PORT}\r\n"));
            if let Some(file) = splash {
                out.push_str(&format!("java-options=-splash:$APPDIR\\resources\\images\\{file}\r\n"));
            }
        }
    }
    out
}

/// The command that opens the images of the CD in its Weasis (Weasis' own CD writer uses it).
const OPEN_COMMAND: &str = "weasis://%24dicom%3Aget%20-p%20%24weasis%3Aconfig%20pro%3D%22weasis.portable.dir%20.%22";

/// `Autorun.inf` at the root of the CD (Windows offers it for discs; USB sticks ignore it).
pub fn autorun_inf(label: &str) -> String {
    let label: String =
        label.chars().filter(|c| !c.is_control() && *c != '"' && *c != '[' && *c != ']').take(32).collect();
    format!(
        "[autorun]\r\nOpen=viewer\\Weasis.exe {OPEN_COMMAND}\r\nAction=Open the images with {label}\r\nIcon=viewer\\Weasis.exe\r\nLabel={label}\r\n"
    )
}

/// `RUN.bat` at the root: what the patient double-clicks (from the CD's own folder).
pub fn run_bat() -> String {
    let command = OPEN_COMMAND.replace('%', "%%");
    format!(
        "@echo off\r\nREM Opens the images of this CD / USB stick in the viewer.\r\ncd /d \"%~dp0\"\r\nstart \"\" \"viewer\\Weasis.exe\" \"{command}\"\r\n"
    )
}

/// The splash image of a CD: the CD logo of Settings, else the portal's logo, when it is a
/// PNG, JPEG or GIF (what Java's splash screen can show). `None` keeps Weasis' own.
pub fn splash_image(state: &AppState) -> Option<(Vec<u8>, &'static str)> {
    let image = state.branding.get(crate::branding::CD_LOGO).or_else(|| state.branding.get(crate::branding::LOGO))?;
    let ext = match image.content_type {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        _ => return None,
    };
    Some((image.data.to_vec(), ext))
}

/// The name in Weasis' window for this portal's CDs.
pub fn viewer_name(state: &AppState) -> String {
    let settings = state.settings.get();
    let name = settings.cd_weasis_name.trim();
    if name.is_empty() {
        format!("{} viewer", settings.portal_name)
    } else {
        name.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cd_settings_lock_and_brand_weasis() {
        let locked = portal_cd_json(&CdBranding { name: "Chalkland viewer".into(), locked: true, splash: None });
        let v: Value = serde_json::from_str(&locked).unwrap();
        let get = |code: &str| {
            v["weasisPreferences"].as_array().unwrap().iter().find(|p| p["code"] == code).map(|p| p["value"].clone())
        };
        assert_eq!(get("weasis.name"), Some(json!("Chalkland viewer")));
        assert_eq!(get("weasis.profile"), Some(json!("portal-cd")));
        assert_eq!(get("felix.auto.start.110"), Some(json!("")), "no send, Q/R or CD writer");
        assert_eq!(get("weasis.import.dicom"), Some(json!("false")));
        assert_eq!(get("weasis.export.dicom"), Some(json!("false")));
        let open = portal_cd_json(&CdBranding { name: "X".into(), locked: false, splash: None });
        assert!(
            !open.contains("felix.auto.start.110") && !open.contains("weasis.import.dicom\""),
            "unlocked: only the name and profile"
        );
    }

    #[test]
    fn the_launcher_reads_the_cd_settings() {
        let original = "[Application]\napp.mainclass=org.weasis.launcher.AppLauncher\n\n[JavaOptions]\njava-options=-Djpackage.app-version=4.8.1\njava-options=-splash:$APPDIR\\resources\\images\\about-round.png\njava-options=-Dgosh.port=17179\n";
        let cfg = weasis_cfg(original, Some("portal-splash.png"));
        assert!(cfg.contains("java-options=-Dfelix.extended.config.properties=file:conf/portal-cd.json"));
        assert!(cfg.contains("-Dgosh.port=17189") && !cfg.contains("17179"), "a control port of its own");
        assert!(cfg.contains("portal-splash.png") && !cfg.contains("about-round.png"));
        assert!(cfg.contains("java-options=-Djpackage.app-version=4.8.1"), "the other lines are kept");
        // Without a splash of its own, Weasis' splash stays.
        assert!(weasis_cfg(original, None).contains("about-round.png"));
    }

    #[test]
    fn autorun_and_run_bat() {
        let inf = autorun_inf("Chalkland [viewer]\r\n");
        assert!(inf.contains("Open=viewer\\Weasis.exe weasis://") && inf.contains("Label=Chalkland viewer"), "{inf}");
        let bat = run_bat();
        assert!(bat.contains("%%24dicom%%3Aget") && bat.contains("cd /d \"%~dp0\""), "{bat}");
    }
}
