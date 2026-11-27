//! # updates.rs
//!
//! **Is there a newer version of the portal?** Once a day (and when an administrator presses
//! *Check now*), the portal asks GitHub for the latest release of its repository and compares its
//! version with its own. It only tells: administrators see a notice and the release's page;
//! installing the update stays their decision (a new release file, or `docker compose pull`).
//!
//! * Off with *Settings > General > Check for new versions* (`update_check`), for sites that must
//!   not reach the internet. A manual *Check now* still works.
//! * The request carries no information about the installation: GitHub's public API, a generic
//!   `User-Agent` with the version, nothing else.
//! * `UPDATE_CHECK_URL` (environment) replaces GitHub's address: a mirror, or the tests' fake.
//! * The last answer is kept in the database, so it is shown again after a restart.

use crate::state::AppState;
use crate::util::now_unix;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Mutex;
use std::time::Duration;

/// The repository the releases are published in.
pub const REPOSITORY: &str = "huma2000/orthanc-portal";
/// This program's version.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
/// How often the background check runs, and how long after start the first one waits.
const EVERY_SECS: u64 = 24 * 3600;
const FIRST_AFTER_SECS: u64 = 60;
/// Where the last answer is kept.
const STORE_KEY: &str = "update_check";

/// The latest release, as GitHub describes it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Release {
    /// The tag, e.g. `v0.1.3`.
    pub tag: String,
    pub name: String,
    /// The release's page on GitHub.
    pub url: String,
    pub published_at: String,
    /// Its notes (Markdown), shortened.
    pub notes: String,
    /// Its files (ready-made programs, checksums).
    #[serde(default)]
    pub assets: Vec<Asset>,
}

/// The outcome of the last check.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Status {
    checked_at: Option<i64>,
    latest: Option<Release>,
    error: Option<String>,
}

static STATUS: Mutex<Option<Status>> = Mutex::new(None);

fn lock() -> std::sync::MutexGuard<'static, Option<Status>> {
    STATUS.lock().unwrap_or_else(|e| e.into_inner())
}

/// The address asked: GitHub's latest release of [`REPOSITORY`], unless `UPDATE_CHECK_URL` says otherwise.
fn address() -> String {
    std::env::var("UPDATE_CHECK_URL")
        .ok()
        .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
        .unwrap_or_else(|| format!("https://api.github.com/repos/{REPOSITORY}/releases/latest"))
}

/// `v1.2.3`, `1.2.3` or `1.2.3-rc1` -> (1, 2, 3, is a pre-release). `None` for anything else.
fn parse(version: &str) -> Option<(u64, u64, u64, bool)> {
    let v = version.trim().trim_start_matches(['v', 'V']);
    let (core, pre) = match v.split_once(['-', '+']) {
        Some((core, rest)) => (core, !rest.is_empty()),
        None => (v, false),
    };
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    let (major, minor) = (parts.next()??, parts.next()??);
    let patch = parts.next().unwrap_or(Some(0))?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch, pre))
}

/// Is `latest` a newer version than `current`? A pre-release counts as older than its release.
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse(latest), parse(current)) {
        (Some((a, b, c, pre_l)), Some((x, y, z, pre_c))) => {
            (a, b, c) > (x, y, z) || ((a, b, c) == (x, y, z) && pre_c && !pre_l)
        }
        _ => false,
    }
}

/// Reads GitHub's answer (`/releases/latest`). Drafts and pre-releases are not offered.
fn release_from(v: &Value) -> Result<Release, String> {
    let text = |key: &str| v[key].as_str().unwrap_or_default().trim().to_string();
    let tag = text("tag_name");
    if tag.is_empty() || parse(&tag).is_none() {
        return Err(format!("the latest release has no version number ('{tag}')"));
    }
    if v["draft"].as_bool() == Some(true) || v["prerelease"].as_bool() == Some(true) {
        return Err("the latest release is a draft or a pre-release".into());
    }
    let url = text("html_url");
    // Only a link to a web page is shown (never `javascript:` or the like).
    let url = if url.starts_with("https://") || url.starts_with("http://") {
        url
    } else {
        format!("https://github.com/{REPOSITORY}/releases")
    };
    let notes: String = text("body").chars().take(4000).collect();
    // Only files served by https (never another kind of link).
    let assets = v["assets"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|a| {
                    let url = a["browser_download_url"].as_str()?.to_string();
                    let name = a["name"].as_str()?.chars().filter(|c| !c.is_control()).take(120).collect::<String>();
                    url.starts_with("https://").then_some(Asset { name, url })
                })
                .take(20)
                .collect()
        })
        .unwrap_or_default();
    Ok(Release { name: text("name"), tag, url, published_at: text("published_at"), notes, assets })
}

/// Asks for the latest release now, remembers the answer (memory and database) and gives it.
pub async fn check(state: &AppState) -> Value {
    let outcome = async {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent(concat!("orthanc-portal/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| e.to_string())?;
        let answer = client
            .get(address())
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .map_err(|e| format!("GitHub could not be reached: {e}"))?;
        match answer.status().as_u16() {
            200 => {}
            404 => return Err("no release is published yet".to_string()),
            403 | 429 => return Err("GitHub refused for now (too many questions): it is asked again later".to_string()),
            code => return Err(format!("GitHub answered HTTP {code}")),
        }
        let body: Value = answer.json().await.map_err(|e| format!("GitHub's answer could not be read: {e}"))?;
        release_from(&body)
    }
    .await;
    let mut status = lock().clone().unwrap_or_default();
    status.checked_at = Some(now_unix());
    match outcome {
        Ok(release) => {
            status.latest = Some(release);
            status.error = None;
        }
        // The release found earlier stays known; the error says why this check failed.
        Err(e) => {
            tracing::info!("update check: {e}");
            status.error = Some(e);
        }
    }
    if let Ok(stored) = serde_json::to_string(&status) {
        let _ = state.db.set_value(STORE_KEY, &stored);
    }
    *lock() = Some(status);
    status_json(state)
}

/// What the System page and the notice show: this version, the latest one, whether it is newer.
pub fn status_json(state: &AppState) -> Value {
    let mut guard = lock();
    if guard.is_none() {
        // After a restart: the last answer kept in the database.
        *guard = state.db.get_value(STORE_KEY).ok().flatten().and_then(|s| serde_json::from_str(&s).ok());
    }
    let status = guard.clone().unwrap_or_default();
    drop(guard);
    let newer = status.latest.as_ref().is_some_and(|r| is_newer(&r.tag, CURRENT));
    let install = this_install();
    let steps = instructions(&install, status.latest.as_ref());
    json!({
        "install": install,
        "instructions": steps,
        "current": CURRENT,
        "enabled": state.settings.get().update_check,
        "checked_at": status.checked_at,
        "latest": status.latest,
        "newer": newer,
        "error": status.error,
        "releases": format!("https://github.com/{REPOSITORY}/releases"),
    })
}

/// The newer release, if there is one (for the administrators' notice): `{ version, url }`.
pub fn newer_release(state: &AppState) -> Option<Value> {
    let status = status_json(state);
    if status["newer"] != json!(true) || status["enabled"] != json!(true) {
        return None;
    }
    let latest = &status["latest"];
    Some(json!({ "version": latest["tag"], "url": latest["url"], "name": latest["name"] }))
}

// ------------------------------------------------------------- how to update this installation

/// A file of a release (a ready-made program for one operating system, its checksum...).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Asset {
    pub name: String,
    pub url: String,
}

/// How this copy of the portal was installed, as far as it can tell.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Install {
    /// `docker` (the image of this project's Dockerfile), `source` (built with cargo from a copy of
    /// the source code) or `binary` (a program file from a release).
    pub kind: &'static str,
    /// `windows`, `linux`, `macos`...
    pub os: &'static str,
    /// Docker: the folder of the compose file it was started from (`deploy/existing-orthanc`...).
    pub compose_dir: Option<String>,
    /// The program file.
    pub exe: String,
    /// Built from source: the folder of the source code.
    pub source_dir: Option<String>,
    /// Started by systemd (Linux service).
    pub systemd: bool,
}

/// One step of the instructions: what to do, and the command to type (if any).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Step {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

fn step(text: impl Into<String>, command: Option<String>) -> Step {
    Step { text: text.into(), command, link: None }
}

/// Works out the installation from what the program can see. `docker`: the image's
/// `PORTAL_INSTALL=docker` (or `/.dockerenv`); `exe`: the program's path. Built from source when
/// the program is in cargo's `target/.../release` folder (the source code is the folder that holds
/// `target`).
pub fn detect(
    docker: bool,
    compose_dir: Option<String>,
    exe: &std::path::Path,
    os: &'static str,
    systemd: bool,
) -> Install {
    let exe_text = exe.display().to_string();
    if docker {
        let compose_dir = compose_dir.map(|d| d.trim().trim_matches('/').to_string()).filter(|d| {
            !d.is_empty()
                && d.len() <= 100
                && d.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'))
        });
        return Install { kind: "docker", os, compose_dir, exe: exe_text, source_dir: None, systemd: false };
    }
    // The program is in `<source>/target/.../release/`: walk up to `target`, keeping the path as written.
    let release = exe.ancestors().find(|a| a.file_name().is_some_and(|n| n == "release"));
    let target = release.and_then(|r| r.ancestors().find(|a| a.file_name().is_some_and(|n| n == "target")));
    if let Some(source) = target.and_then(|t| t.parent()) {
        return Install {
            kind: "source",
            os,
            compose_dir: None,
            exe: exe_text,
            source_dir: Some(source.display().to_string()),
            systemd,
        };
    }
    Install { kind: "binary", os, compose_dir: None, exe: exe_text, source_dir: None, systemd }
}

/// This copy's installation.
pub fn this_install() -> Install {
    let docker = std::env::var("PORTAL_INSTALL").is_ok_and(|v| v.eq_ignore_ascii_case("docker"))
        || std::path::Path::new("/.dockerenv").exists();
    let exe = std::env::current_exe().unwrap_or_default();
    detect(
        docker,
        std::env::var("PORTAL_COMPOSE_DIR").ok(),
        &exe,
        std::env::consts::OS,
        std::env::var_os("INVOCATION_ID").is_some(),
    )
}

/// The ready-made program of a release for an operating system (the zip named after it).
fn asset_for<'a>(assets: &'a [Asset], os: &str) -> Option<&'a Asset> {
    assets.iter().find(|a| {
        let name = a.name.to_lowercase();
        name.contains(os) && !name.ends_with(".sha256")
    })
}

/// The steps that update THIS installation to `release` (or to the latest one, when not known yet).
pub fn instructions(install: &Install, release: Option<&Release>) -> Vec<Step> {
    let version = release.map(|r| r.tag.clone()).unwrap_or_else(|| "the new version".into());
    let restart = |install: &Install| -> Step {
        if install.systemd {
            step(
                "Restart the portal's service (use the name of your service):",
                Some("sudo systemctl restart orthanc-portal".into()),
            )
        } else if install.os == "windows" {
            step("Stop the running portal (close its window, or stop its Windows service), then start it again.", None)
        } else {
            step("Stop the running portal, then start it again (the same way it is started now).", None)
        }
    };
    let from_source = |install: &Install, source: &str| -> Vec<Step> {
        let cd = if install.os == "windows" { format!("cd \"{source}\"") } else { format!("cd '{source}'") };
        vec![
            step("Go to the folder of the source code:", Some(cd)),
            step(format!("Get {version}:"), Some("git pull".into())),
            step("Build it (the first time after an update takes a few minutes):", Some("cargo build --release".into())),
            restart(install),
            step("Users, settings, links and studies are kept: they are in the database and in Orthanc, not in the program.", None),
        ]
    };
    match install.kind {
        "docker" => {
            let folder = match &install.compose_dir {
                Some(dir) => format!("<folder of the source code>/{dir}"),
                None => "<folder of the source code>/deploy".into(),
            };
            let mut steps = vec![
                step(
                    "On the server, go to the folder of the compose file the portal was started from (in the copy of the source code made with git clone):",
                    Some(format!("cd {folder}")),
                ),
            ];
            if install.compose_dir.is_none() {
                steps.push(step(
                    "It is deploy, deploy/existing-orthanc or deploy/portal-only: the one that holds the .env file of this installation.",
                    None,
                ));
            }
            steps.extend([
                step(format!("Get {version}:"), Some("git pull".into())),
                step(
                    "Rebuild the portal's image and restart it (the image is built from the source code: docker compose pull does not update it):",
                    Some("docker compose up -d --build".into()),
                ),
                step("Check that it started (healthy):", Some("docker compose ps".into())),
                step("Users, settings, links and studies are kept: they are in the Docker volumes, not in the image.", None),
            ]);
            steps
        }
        "source" => from_source(install, install.source_dir.as_deref().unwrap_or(".")),
        _ => match release.and_then(|r| asset_for(&r.assets, install.os)) {
            Some(asset) => {
                let checksum =
                    release.and_then(|r| r.assets.iter().find(|a| a.name == format!("{}.sha256", asset.name)));
                let mut steps = vec![Step {
                    text: format!("Download {} from the release page.", asset.name),
                    command: None,
                    link: Some(asset.url.clone()),
                }];
                if let Some(sum) = checksum {
                    let command = if install.os == "windows" {
                        format!("Get-FileHash .\\{} -Algorithm SHA256", asset.name)
                    } else {
                        format!("sha256sum {}", asset.name)
                    };
                    steps.push(Step {
                        text: format!("Check the download: its SHA-256 must be the one in {}.", sum.name),
                        command: Some(command),
                        link: Some(sum.url.clone()),
                    });
                }
                steps.extend([
                    if install.os == "windows" {
                        step("Stop the running portal (close its window, or stop its Windows service).", None)
                    } else if install.systemd {
                        step("Stop the portal's service:", Some("sudo systemctl stop orthanc-portal".into()))
                    } else {
                        step("Stop the running portal.", None)
                    },
                    step(format!("Unzip it and put the new program in place of {}.", install.exe), None),
                    if install.systemd {
                        step("Start the service again:", Some("sudo systemctl start orthanc-portal".into()))
                    } else {
                        step("Start the portal again (the same way as before).", None)
                    },
                    step("Keep the database file and the .env file where they are: users, settings, links and studies are kept.", None),
                ]);
                steps
            }
            None => {
                let mut steps = vec![step(
                    format!(
                        "This release has no ready-made program for {}: build it from the source code.",
                        install.os
                    ),
                    None,
                )];
                steps.push(step(
                    "Once, if there is no copy of the source code on this computer yet:",
                    Some(format!("git clone https://github.com/{REPOSITORY}.git")),
                ));
                steps.extend(from_source(install, "orthanc-portal"));
                steps
                    .push(step(format!("Then run the new program (target/release) instead of {}.", install.exe), None));
                steps
            }
        },
    }
}

/// Runs for as long as the program does (started by main.rs): a first check shortly after the
/// start, then once a day, while the setting is on.
pub async fn run(state: AppState) {
    tokio::time::sleep(Duration::from_secs(FIRST_AFTER_SECS)).await;
    let mut tick = tokio::time::interval(Duration::from_secs(EVERY_SECS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if state.settings.get().update_check {
            check(&state).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_like_numbers() {
        assert!(is_newer("v0.1.10", "0.1.9"), "10 is more than 9, not less");
        assert!(is_newer("0.2.0", "0.1.12"));
        assert!(is_newer("v1.0", "0.9.9"), "a missing patch number is 0");
        assert!(!is_newer("v0.1.2", "0.1.2"), "the same version");
        assert!(!is_newer("v0.1.1", "0.1.2"), "an older one");
        assert!(is_newer("0.1.2", "0.1.2-rc1"), "the release after its pre-release");
        assert!(!is_newer("0.1.2-rc1", "0.1.2"));
        assert!(!is_newer("latest", "0.1.2"), "not a version");
        assert!(!is_newer("1.2.3.4", "0.1.2"));
    }

    fn release(assets: &[&str]) -> Release {
        Release {
            tag: "v0.2.0".into(),
            name: String::new(),
            url: String::new(),
            published_at: String::new(),
            notes: String::new(),
            assets: assets.iter().map(|n| Asset { name: n.to_string(), url: format!("https://x/{n}") }).collect(),
        }
    }

    fn commands(steps: &[Step]) -> Vec<String> {
        steps.iter().filter_map(|s| s.command.clone()).collect()
    }

    #[test]
    fn each_installation_gets_its_own_instructions() {
        use std::path::Path;
        // Docker, started from deploy/existing-orthanc: rebuild the image there.
        let docker = detect(
            true,
            Some("deploy/existing-orthanc".into()),
            Path::new("/usr/local/bin/orthanc-portal"),
            "linux",
            false,
        );
        assert_eq!(docker.kind, "docker");
        let steps = instructions(&docker, Some(&release(&[])));
        assert_eq!(
            commands(&steps),
            [
                "cd <folder of the source code>/deploy/existing-orthanc",
                "git pull",
                "docker compose up -d --build",
                "docker compose ps"
            ]
        );
        // A strange folder name from the environment is not shown.
        assert_eq!(detect(true, Some("x; rm -rf /".into()), Path::new("/x"), "linux", false).compose_dir, None);

        // Built with cargo: the source folder is the one that holds target/.
        let source = detect(false, None, Path::new("/opt/orthanc-portal/target/release/orthanc-portal"), "linux", true);
        assert_eq!((source.kind, source.source_dir.as_deref()), ("source", Some("/opt/orthanc-portal")));
        let steps = commands(&instructions(&source, Some(&release(&[]))));
        assert_eq!(
            steps,
            ["cd '/opt/orthanc-portal'", "git pull", "cargo build --release", "sudo systemctl restart orthanc-portal"]
        );
        let work = detect(false, None, Path::new("/src/p/target/work/release/orthanc-portal"), "linux", false);
        assert_eq!(work.source_dir.as_deref(), Some("/src/p"), "a target folder of its own (CARGO_TARGET_DIR)");

        // A program from a release, on Windows: its zip, the checksum, replace the file.
        let binary = detect(false, None, Path::new("C:/Portal/orthanc-portal.exe"), "windows", false);
        assert_eq!(binary.kind, "binary");
        let steps = instructions(
            &binary,
            Some(&release(&["orthanc-portal-v0.2.0-windows-x64.zip", "orthanc-portal-v0.2.0-windows-x64.zip.sha256"])),
        );
        assert_eq!(steps[0].link.as_deref(), Some("https://x/orthanc-portal-v0.2.0-windows-x64.zip"));
        assert_eq!(commands(&steps), ["Get-FileHash .\\orthanc-portal-v0.2.0-windows-x64.zip -Algorithm SHA256"]);
        // ...and on Linux, where the release has no program: build it from the source code.
        let linux = detect(false, None, Path::new("/usr/local/bin/orthanc-portal"), "linux", false);
        let steps = commands(&instructions(&linux, Some(&release(&["orthanc-portal-v0.2.0-windows-x64.zip"]))));
        assert!(steps.contains(&"cargo build --release".to_string()) && steps[0].starts_with("git clone"));
    }

    #[test]
    fn github_answers_are_read_carefully() {
        let ok = json!({ "tag_name": "v0.2.0", "name": "Version 0.2.0", "html_url": "https://github.com/x/y/releases/tag/v0.2.0",
                         "published_at": "2026-12-01T10:00:00Z", "body": "Notes", "draft": false, "prerelease": false });
        let release = release_from(&ok).unwrap();
        assert_eq!((release.tag.as_str(), release.notes.as_str()), ("v0.2.0", "Notes"));
        let mut bad_link = ok.clone();
        bad_link["html_url"] = json!("javascript:alert(1)");
        assert!(release_from(&bad_link).unwrap().url.starts_with("https://github.com/"), "only web links");
        let mut pre = ok.clone();
        pre["prerelease"] = json!(true);
        assert!(release_from(&pre).is_err());
        assert!(release_from(&json!({ "tag_name": "nightly" })).is_err());
    }
}
