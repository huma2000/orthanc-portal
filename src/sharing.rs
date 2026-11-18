//! # sharing.rs
//!
//! The pure rules of **invitation links** (no HTTP in here, so they are easy to test):
//! generating the unguessable token and the password, and - most important - the rules
//! that confine an outside guest to **one study**.
//!
//! ## Why the guest rules are strict
//! A guest sees the study in the browser viewer (OHIF), which reads it through the portal's
//! DICOMweb proxy. If that proxy let a guest ask for anything, the guest could
//!  * list or open OTHER patients' studies, or
//!  * fetch each DICOM file of the shared study one by one, getting around the "N downloads"
//!    limit of the invitation.
//!
//! So a guest may only read what belongs to the shared study (matched by its
//! StudyInstanceUID), and **never whole DICOM files**: the only way to get the files is the
//! ZIP download, which is counted. A viewer needs only searches, metadata and pixel frames.

use crate::util::random_token;
use percent_encoding::percent_decode_str;
use rand::Rng;

/// Characters of generated passwords: no `0 O 1 I L`, which are mistaken for each other when a
/// password is read aloud or typed from paper. 31 symbols x 12 positions = about 59 bits.
const PASSWORD_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
const PASSWORD_LEN: usize = 12;

/// A new password, in groups of four to make it easy to read: `K7QX-M2PD-9RHW`.
pub fn generate_password() -> String {
    let mut rng = rand::rngs::OsRng;
    let mut password = String::with_capacity(PASSWORD_LEN + 2);
    for i in 0..PASSWORD_LEN {
        if i > 0 && i % 4 == 0 {
            password.push('-');
        }
        password.push(PASSWORD_ALPHABET[rng.gen_range(0..PASSWORD_ALPHABET.len())] as char);
    }
    password
}

/// What is hashed and compared: upper case, only password symbols. So `k7qx m2pd-9rhw` and
/// `K7QX-M2PD-9RHW` are the same password. (At most 64 characters are looked at.)
pub fn normalize_password(input: &str) -> String {
    input
        .chars()
        .take(64)
        .map(|c| c.to_ascii_uppercase())
        .filter(|c| c.is_ascii() && PASSWORD_ALPHABET.contains(&(*c as u8)))
        .collect()
}

/// The random part of a link: 192 bits as 48 hexadecimal characters.
pub fn new_token() -> String {
    random_token()[..48].to_string()
}

/// Cheap shape test before any database lookup (rejects garbage and oversized input).
pub fn is_token_shape(token: &str) -> bool {
    token.len() == 48 && token.chars().all(|c| c.is_ascii_hexdigit())
}

/// The address to give to the guest.
pub fn share_url(base: &str, token: &str) -> String {
    format!("{base}/s/{token}")
}

/// `active`, `expired` or `revoked` (revoked wins), for the Shares list.
pub fn status_name(revoked: bool, expired: bool) -> &'static str {
    if revoked {
        "revoked"
    } else if expired {
        "expired"
    } else {
        "active"
    }
}

// ------------------------------------------------------------------------ guest rules

/// DICOMweb requests that return WHOLE DICOM files: a whole study, series or instance. (A
/// viewer asks for metadata and pixel frames instead.) Anything with more path after the
/// instance (`/metadata`, `/frames/1`, `/rendered`, `/thumbnail`) is not a whole file.
pub fn is_whole_dicom_retrieval(path: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    matches!(
        parts.as_slice(),
        ["studies", _]
            | ["series", _]
            | ["instances", _]
            | ["studies", _, "series", _]
            | ["studies", _, "series", _, "instances", _]
    )
}

/// The decoded values of the query parameters called `keys` (compared ignoring case).
fn query_values(query: Option<&str>, keys: &[&str]) -> Vec<String> {
    query
        .unwrap_or_default()
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .filter(|(key, _)| keys.iter().any(|k| key.eq_ignore_ascii_case(k)))
        .map(|(_, value)| percent_decode_str(value).decode_utf8_lossy().trim().to_string())
        .collect()
}

/// True when the query names shared studies, and ONLY those (every value must be one of them).
fn names_only_study(query: Option<&str>, keys: &[&str], study_uids: &[String]) -> bool {
    let values = query_values(query, keys);
    !values.is_empty() && values.iter().all(|v| study_uids.iter().any(|uid| uid == v))
}

/// May a guest of the share for `study_uid` read this DICOMweb path?
///
/// * searches (`studies`, `series`, `instances`) only when narrowed to the shared study with
///   `StudyInstanceUID=<uid>` (otherwise they would list the whole archive);
/// * anything below `studies/<uid>/...` when `<uid>` is the shared study, except whole files;
/// * nothing else.
pub fn guest_may_read_dicomweb(path: &str, query: Option<&str>, study_uids: &[String]) -> bool {
    if is_whole_dicom_retrieval(path) {
        return false;
    }
    let parts: Vec<&str> = path.split('/').collect();
    match parts.as_slice() {
        ["studies"] | ["series"] | ["instances"] => {
            names_only_study(query, &["StudyInstanceUID", "0020000D"], study_uids)
        }
        ["studies", uid, ..] => study_uids.iter().any(|shared| shared == uid),
        _ => false,
    }
}

/// WADO-URI (`/wado?requestType=WADO&studyUID=...&contentType=image/jpeg`): only for the shared
/// study and only as a picture. Without a picture type Orthanc returns a whole DICOM file.
pub fn guest_may_read_wado(query: Option<&str>, study_uids: &[String]) -> bool {
    names_only_study(query, &["studyUID"], study_uids) && is_picture_request(query)
}

/// True when every `contentType` asked for is a picture type (and there is at least one).
fn is_picture_request(query: Option<&str>) -> bool {
    let types: Vec<String> = query_values(query, &["contentType"])
        .iter()
        .flat_map(|v| v.split(',').map(|t| t.trim().to_ascii_lowercase()).collect::<Vec<_>>())
        .collect();
    !types.is_empty() && types.iter().all(|t| t.starts_with("image/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const UID: &str = "1.2.826.0.1.3680043.8.498.1";

    fn one() -> Vec<String> {
        vec![UID.to_string()]
    }

    #[test]
    fn generated_passwords_are_readable_and_strong_enough() {
        let p = generate_password();
        assert_eq!(p.len(), 14, "{p}");
        assert!(
            p.chars().enumerate().all(|(i, c)| if i == 4 || i == 9 {
                c == '-'
            } else {
                PASSWORD_ALPHABET.contains(&(c as u8))
            }),
            "{p}"
        );
        assert_ne!(generate_password(), generate_password());
        assert_eq!(normalize_password(&p).len(), 12);
    }

    #[test]
    fn typed_passwords_are_forgiving() {
        assert_eq!(normalize_password("k7qx m2pd-9rhw"), "K7QXM2PD9RHW");
        assert_eq!(normalize_password("  K7QX-M2PD-9RHW \n"), "K7QXM2PD9RHW");
        assert_eq!(normalize_password("K7QX-M2PD-9RHW"), normalize_password("k7qxm2pd9rhw"));
        assert_eq!(normalize_password("0O1IL"), ""); // symbols that are never generated
        assert!(normalize_password(&"A".repeat(500)).len() <= 64);
    }

    #[test]
    fn token_shape() {
        assert!(is_token_shape(&new_token()));
        assert!(!is_token_shape("short"));
        assert!(!is_token_shape(&"z".repeat(48)));
        assert!(!is_token_shape(&"a".repeat(49)));
    }

    #[test]
    fn guests_never_get_whole_dicom_files() {
        for path in [
            format!("studies/{UID}"),
            format!("studies/{UID}/series/1.2"),
            format!("studies/{UID}/series/1.2/instances/3.4"),
            "series/1.2".to_string(),
            "instances/3.4".to_string(),
        ] {
            assert!(!guest_may_read_dicomweb(&path, None, &one()), "{path}");
        }
    }

    #[test]
    fn guests_read_what_a_viewer_needs_of_their_study() {
        for path in [
            format!("studies/{UID}/series"),
            format!("studies/{UID}/instances"),
            format!("studies/{UID}/metadata"),
            format!("studies/{UID}/series/1.2/instances"),
            format!("studies/{UID}/series/1.2/metadata"),
            format!("studies/{UID}/series/1.2/instances/3.4/metadata"),
            format!("studies/{UID}/series/1.2/instances/3.4/frames/1"),
            format!("studies/{UID}/series/1.2/instances/3.4/rendered"),
            format!("studies/{UID}/series/1.2/instances/3.4/thumbnail"),
        ] {
            assert!(guest_may_read_dicomweb(&path, None, &one()), "{path}");
        }
    }

    #[test]
    fn guests_cannot_reach_other_studies() {
        assert!(!guest_may_read_dicomweb("studies/9.9.9/series", None, &one()));
        assert!(!guest_may_read_dicomweb("studies/9.9.9/metadata", None, &one()));
        assert!(!guest_may_read_dicomweb("studies", None, &one()), "an unfiltered search lists everybody");
        assert!(!guest_may_read_dicomweb("studies", Some("limit=100"), &one()));
        assert!(!guest_may_read_dicomweb("series", Some("Modality=CT"), &one()));
        assert!(!guest_may_read_dicomweb("studies", Some("StudyInstanceUID=9.9.9"), &one()));
        assert!(!guest_may_read_dicomweb(
            "studies",
            Some(&format!("StudyInstanceUID={UID}&StudyInstanceUID=9.9.9")),
            &one()
        ));
        assert!(!guest_may_read_dicomweb("studies", Some(&format!("StudyInstanceUID={UID}\\9.9.9")), &one()));
        assert!(!guest_may_read_dicomweb("studies", Some(&format!("StudyInstanceUID=*{UID}")), &one()));
        assert!(!guest_may_read_dicomweb("series/1.2/metadata", None, &one()));
        assert!(!guest_may_read_dicomweb("servers", None, &one()));
    }

    #[test]
    fn searches_narrowed_to_the_shared_study_are_fine() {
        assert!(guest_may_read_dicomweb("studies", Some(&format!("StudyInstanceUID={UID}")), &one()));
        assert!(guest_may_read_dicomweb(
            "studies",
            Some(&format!("limit=101&offset=0&includefield=00081030&StudyInstanceUID={UID}")),
            &one()
        ));
        assert!(guest_may_read_dicomweb("studies", Some(&format!("0020000d={UID}")), &one()));
        assert!(guest_may_read_dicomweb("series", Some(&format!("StudyInstanceUID={UID}")), &one()));
        assert!(guest_may_read_dicomweb(
            "instances",
            Some(&format!("StudyInstanceUID={}", UID.replace('.', "%2E"))),
            &one()
        ));
    }

    #[test]
    fn wado_only_as_a_picture_of_the_shared_study() {
        let q = |extra: &str| format!("requestType=WADO&studyUID={UID}&seriesUID=1&objectUID=2{extra}");
        assert!(guest_may_read_wado(Some(&q("&contentType=image/jpeg")), &one()));
        assert!(guest_may_read_wado(Some(&q("&contentType=image%2Fpng")), &one()));
        assert!(!guest_may_read_wado(Some(&q("")), &one()), "no type = a whole DICOM file");
        assert!(!guest_may_read_wado(Some(&q("&contentType=application/dicom")), &one()));
        assert!(!guest_may_read_wado(Some(&q("&contentType=image/jpeg,application/dicom")), &one()));
        assert!(!guest_may_read_wado(Some(&q("&contentType=image/jpeg&contentType=application/dicom")), &one()));
        assert!(!guest_may_read_wado(Some("requestType=WADO&studyUID=9.9&contentType=image/jpeg"), &one()));
        assert!(!guest_may_read_wado(Some("contentType=image/jpeg"), &one()));
        assert!(!guest_may_read_wado(None, &one()));
    }

    #[test]
    fn a_link_for_several_studies_reaches_each_of_them_only() {
        let uids = vec![UID.to_string(), "1.2.3.9".to_string()];
        assert!(guest_may_read_dicomweb("studies/1.2.3.9/metadata", None, &uids));
        assert!(guest_may_read_dicomweb(&format!("studies/{UID}/series"), None, &uids));
        assert!(guest_may_read_dicomweb("studies", Some("StudyInstanceUID=1.2.3.9"), &uids));
        assert!(!guest_may_read_dicomweb("studies/9.9.9/metadata", None, &uids));
        assert!(!guest_may_read_dicomweb("studies", Some("StudyInstanceUID=1.2.3.9&StudyInstanceUID=9.9"), &uids));
        assert!(!guest_may_read_dicomweb("studies/1.2.3.9", None, &uids), "never whole files");
    }
}
