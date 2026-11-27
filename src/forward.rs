//! # forward.rs
//!
//! **Auto-forward**: studies that arrive in Orthanc and match a rule (Settings > Auto-forward) are
//! sent on automatically: "every CT from the scanner goes to the reporting workstation too", and/or
//! announced by email: "tell the radiologist when a CT from Riverside arrives, or one labelled urgent".
//!
//! How it works: every 15 seconds the portal reads Orthanc's list of changes (`GET /changes`) from
//! where it stopped last time (the position is kept in the database, so a restart misses nothing and
//! repeats nothing). A study is looked at when Orthanc says it is **stable** (no new image for a
//! while, Orthanc's `StableAge`): it is complete by then. Each matching rule starts an ordinary
//! transfer, shown on the Transfers page as sent by "auto: <rule>", and written in the audit log.
//! A rule with addresses to notify emails them, once per study (a study that becomes stable again
//! is not announced twice). The email never names the patient.
//!
//! Switching the feature on starts from the CURRENT end of the list: studies that arrived while it
//! was off are not sent all at once. A study that gets more images later becomes stable again and is
//! sent again, as Orthanc's own routing scripts do (the destination stores each image once).

use crate::error::AppError;
use crate::mailer::{self, RuleNotice};
use crate::routes::send::{find_destination, start_send};
use crate::settings::ForwardRule;
use crate::state::AppState;
use crate::util::{is_valid_orthanc_id, now_unix};
use serde_json::{json, Value};
use std::sync::Mutex;
use std::time::Duration;

/// Where the portal stopped reading Orthanc's changes ("" = start from the end next time).
const SEQ_KEY: &str = "forward_seq";
/// Changes read per question to Orthanc, and questions per round at most.
const BATCH: u64 = 200;
const BATCHES_PER_ROUND: usize = 10;

/// What Settings > Auto-forward shows about the last rounds.
#[derive(Default)]
struct Status {
    last_check: Option<i64>,
    last_error: Option<String>,
    forwarded: u64,
    last_forward: Option<Value>,
    notified: u64,
}

static STATUS: Mutex<Status> =
    Mutex::new(Status { last_check: None, last_error: None, forwarded: 0, last_forward: None, notified: 0 });

fn status_lock() -> std::sync::MutexGuard<'static, Status> {
    STATUS.lock().unwrap_or_else(|e| e.into_inner())
}

/// The state of the forwarding, for the Settings screen.
pub fn status() -> Value {
    let s = status_lock();
    json!({
        "last_check": s.last_check,
        "last_error": s.last_error,
        "forwarded": s.forwarded,
        "last_forward": s.last_forward,
        "notified": s.notified,
    })
}

/// Runs for as long as the program does (started by main.rs).
pub async fn run(state: AppState) {
    let mut tick = tokio::time::interval(Duration::from_secs(15));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let settings = state.settings.get();
        let rules: Vec<ForwardRule> = settings.forward_rules.iter().filter(|r| r.enabled).cloned().collect();
        if !settings.forward_enabled || rules.is_empty() {
            // Off: forget the position, so switching it on later starts from that moment.
            if matches!(state.db.get_value(SEQ_KEY), Ok(Some(seq)) if !seq.is_empty()) {
                let _ = state.db.set_value(SEQ_KEY, "");
            }
            continue;
        }
        let outcome = round(&state, &rules).await;
        let mut s = status_lock();
        s.last_check = Some(now_unix());
        s.last_error = outcome.err().map(|e| e.to_string());
    }
}

/// One round: reads the new changes and forwards the stable studies that match.
async fn round(state: &AppState, rules: &[ForwardRule]) -> Result<(), AppError> {
    let stored = state.db.get_value(SEQ_KEY)?.unwrap_or_default();
    let Ok(mut since) = stored.parse::<u64>() else {
        // First round after switching on: start at the current end of the list.
        let last = state.orthanc.get_json("/changes?last").await?;
        let seq = last["Last"].as_u64().unwrap_or(0);
        state.db.set_value(SEQ_KEY, &seq.to_string())?;
        return Ok(());
    };
    for _ in 0..BATCHES_PER_ROUND {
        let page = state.orthanc.get_json(&format!("/changes?since={since}&limit={BATCH}")).await?;
        let changes = page["Changes"].as_array().cloned().unwrap_or_default();
        for change in &changes {
            if change["ChangeType"].as_str() == Some("StableStudy") {
                if let Some(id) = change["ID"].as_str().filter(|id| is_valid_orthanc_id(id)) {
                    forward_study(state, rules, id).await;
                }
            }
        }
        let last = page["Last"].as_u64().unwrap_or(since);
        if last > since {
            since = last;
            state.db.set_value(SEQ_KEY, &since.to_string())?;
        }
        if page["Done"].as_bool().unwrap_or(true) || changes.is_empty() {
            break;
        }
    }
    Ok(())
}

/// What the rules look at in a study.
#[derive(Debug, Default)]
pub struct StudyFacts {
    pub modalities: Vec<String>,
    pub institution: String,
    pub description: String,
    /// The AE title of the system that sent it to Orthanc (empty when unknown).
    pub calling_aet: String,
    pub labels: Vec<String>,
    // For the emails only.
    pub date: String,
    pub accession: String,
    pub uid: String,
}

async fn facts(state: &AppState, id: &str, need_aet: bool) -> Result<StudyFacts, AppError> {
    let study = state.orthanc.get_json(&format!("/studies/{id}")).await?;
    let series = state.orthanc.get_json(&format!("/studies/{id}/series")).await?;
    let series = series.as_array().cloned().unwrap_or_default();
    let mut modalities: Vec<String> = series
        .iter()
        .filter_map(|s| s["MainDicomTags"]["Modality"].as_str())
        .map(|m| m.trim().to_uppercase())
        .filter(|m| !m.is_empty())
        .collect();
    modalities.sort();
    modalities.dedup();
    let mut calling_aet = String::new();
    if need_aet {
        let instance = series.iter().find_map(|s| s["Instances"].as_array()?.first()?.as_str().map(String::from));
        if let Some(instance) = instance.filter(|i| is_valid_orthanc_id(i)) {
            calling_aet = state
                .orthanc
                .get_text(&format!("/instances/{instance}/metadata/RemoteAET"))
                .await
                .unwrap_or_default()
                .trim()
                .to_string();
        }
    }
    let tag = |name: &str| study["MainDicomTags"][name].as_str().unwrap_or_default().to_string();
    Ok(StudyFacts {
        modalities,
        institution: tag("InstitutionName"),
        description: tag("StudyDescription"),
        calling_aet,
        labels: study["Labels"]
            .as_array()
            .map(|a| a.iter().filter_map(|l| l.as_str().map(str::to_string)).collect())
            .unwrap_or_default(),
        date: tag("StudyDate"),
        accession: tag("AccessionNumber"),
        uid: tag("StudyInstanceUID"),
    })
}

/// Does `text` match a pattern with `*` (anything) and `?` (one character)? Case does not matter.
fn wildcard(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) =
        (pattern.to_uppercase().chars().collect(), text.trim().to_uppercase().chars().collect());
    // Classic two-pointer match with backtracking on the last `*`.
    let (mut i, mut j, mut star, mut mark) = (0, 0, None, 0);
    while j < t.len() {
        if i < p.len() && (p[i] == '?' || p[i] == t[j]) {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == '*' {
            star = Some(i);
            mark = j;
            i += 1;
        } else if let Some(s) = star {
            i = s + 1;
            mark += 1;
            j = mark;
        } else {
            return false;
        }
    }
    p[i..].iter().all(|c| *c == '*')
}

/// Does the study match the rule? Every condition that is filled in must hold.
pub fn matches(rule: &ForwardRule, study: &StudyFacts) -> bool {
    let wanted: Vec<String> = rule
        .modalities
        .iter()
        .flat_map(|m| if m == "RX" { vec!["CR".to_string(), "DX".to_string()] } else { vec![m.clone()] })
        .collect();
    let contains = |haystack: &str, needle: &str| haystack.to_lowercase().contains(&needle.to_lowercase());
    (wanted.is_empty() || study.modalities.iter().any(|m| wanted.contains(m)))
        && (rule.calling_aet.is_empty() || wildcard(&rule.calling_aet, &study.calling_aet))
        && (rule.institution.is_empty() || contains(&study.institution, &rule.institution))
        && (rule.description.is_empty() || contains(&study.description, &rule.description))
        && (rule.labels.is_empty() || study.labels.iter().any(|l| rule.labels.contains(l)))
}

async fn forward_study(state: &AppState, rules: &[ForwardRule], id: &str) {
    // The sender is one more question to Orthanc: only when a rule looks at it or tells it.
    let need_aet = rules.iter().any(|r| !r.calling_aet.is_empty() || !r.notify.is_empty());
    let study = match facts(state, id, need_aet).await {
        Ok(study) => study,
        Err(AppError::NotFound) => return, // deleted meanwhile
        Err(e) => {
            tracing::warn!("auto-forward: study {id} could not be read: {e}");
            return;
        }
    };
    for rule in rules.iter().filter(|r| matches(r, &study)) {
        if !rule.notify.is_empty() {
            notify(state, rule, id, &study);
        }
        if rule.target.is_empty() {
            continue; // a rule that only notifies
        }
        let who = format!("auto: {}", rule.name);
        let outcome = async {
            let node = find_destination(state, &rule.target).await?;
            start_send(state, &who, &node, id).await
        }
        .await;
        match outcome {
            Ok((_, job)) => {
                let mut s = status_lock();
                s.forwarded += 1;
                s.last_forward = Some(json!({ "ts": now_unix(), "rule": rule.name, "study": id, "job": job }));
            }
            Err(e) => {
                tracing::warn!("auto-forward '{}': study {id} could not be sent to {}: {e}", rule.name, rule.target);
                state.db.audit(&who, "auto_forward_failed", &format!("{id} -> {}: {e}", rule.target));
            }
        }
    }
}

/// Emails the rule's addresses about the study, once per study and rule, in the background (the
/// rounds never wait for the mail server). Written in the audit log either way.
fn notify(state: &AppState, rule: &ForwardRule, id: &str, study: &StudyFacts) {
    let who = format!("auto: {}", rule.name);
    match state.db.first_rule_notice(&rule.name, id, now_unix()) {
        Ok(true) => {}
        Ok(false) => return, // already announced
        Err(e) => {
            tracing::warn!("auto-forward '{}': {e}", rule.name);
            return;
        }
    }
    let settings = state.settings.get();
    let base = settings.public_base_url.trim_end_matches('/');
    // The UID comes from the file that was sent in: only a real one (digits and dots) goes in the
    // link, so a crafted file cannot put another address or text into the email.
    let real_uid =
        !study.uid.is_empty() && study.uid.len() <= 64 && study.uid.bytes().all(|b| b.is_ascii_digit() || b == b'.');
    let link = (!base.is_empty() && real_uid).then(|| format!("{base}/?study_uid={}", study.uid));
    let (subject, body) = RuleNotice {
        portal: &settings.portal_name,
        rule: &rule.name,
        modalities: &study.modalities,
        institution: &study.institution,
        description: &study.description,
        date: &study.date,
        accession: &study.accession,
        calling_aet: &study.calling_aet,
        labels: &study.labels,
        link,
        lang: settings.language,
    }
    .email();
    let (state, to, id) = (state.clone(), rule.notify.clone(), id.to_string());
    tokio::spawn(async move {
        let outcome = match mailer::parse_recipients(&to) {
            Ok(mailboxes) => mailer::send(&settings, mailboxes, &subject, &body).await,
            Err(e) => Err(e),
        };
        match outcome {
            Ok(()) => {
                status_lock().notified += 1;
                state.db.audit(&who, "auto_notified", &format!("{id} -> {to}"));
            }
            Err(e) => {
                tracing::warn!("{who}: the email about study {id} could not be sent: {e}");
                state.db.audit(&who, "auto_notify_failed", &format!("{id} -> {to}: {e}"));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule() -> ForwardRule {
        ForwardRule {
            name: "CT to reporting".into(),
            enabled: true,
            modalities: vec![],
            calling_aet: String::new(),
            institution: String::new(),
            description: String::new(),
            labels: vec![],
            target: "ris".into(),
            notify: String::new(),
        }
    }

    #[test]
    fn empty_conditions_match_everything() {
        assert!(matches(&rule(), &StudyFacts::default()));
    }

    #[test]
    fn every_condition_filled_in_must_hold() {
        let study = StudyFacts {
            modalities: vec!["CT".into(), "SR".into()],
            institution: "Riverside Vets".into(),
            description: "CT CHEST".into(),
            calling_aet: "SCANNER1".into(),
            labels: vec!["urgent".into()],
            ..Default::default()
        };
        let with = |f: fn(&mut ForwardRule)| {
            let mut r = rule();
            f(&mut r);
            matches(&r, &study)
        };
        assert!(with(|r| r.modalities = vec!["MR".into(), "CT".into()]));
        assert!(!with(|r| r.modalities = vec!["MR".into()]));
        assert!(with(|r| r.calling_aet = "scan*".into()));
        assert!(with(|r| r.calling_aet = "SCANNER?".into()));
        assert!(!with(|r| r.calling_aet = "MRI*".into()));
        assert!(with(|r| r.institution = "riverside".into()));
        assert!(!with(|r| r.institution = "hillside".into()));
        assert!(with(|r| r.description = "chest".into()));
        assert!(!with(|r| {
            r.modalities = vec!["CT".into()];
            r.description = "knee".into();
        }));
        assert!(with(|r| r.labels = vec!["teaching".into(), "urgent".into()]));
        assert!(!with(|r| r.labels = vec!["teaching".into()]));
    }

    #[test]
    fn rx_means_both_xray_codes() {
        let mut r = rule();
        r.modalities = vec!["RX".into()];
        assert!(matches(&r, &StudyFacts { modalities: vec!["DX".into()], ..Default::default() }));
        assert!(!matches(&r, &StudyFacts { modalities: vec!["CT".into()], ..Default::default() }));
    }

    #[test]
    fn wildcards() {
        assert!(wildcard("*", ""));
        assert!(wildcard("A*C", "abc") && wildcard("A*C", "AC") && !wildcard("A*C", "ABD"));
        assert!(wildcard("??", "AB") && !wildcard("??", "ABC"));
        assert!(!wildcard("SCANNER", "") && wildcard("SCANNER", " scanner "));
    }
}
