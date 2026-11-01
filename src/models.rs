//! # models.rs
//!
//! Orthanc answers with its own JSON layout (`MainDicomTags`, `PatientMainDicomTags`,
//! `RequestedTags`...). These structs convert it once, on the server, into a
//! simple flat shape, so the web UI does not depend on Orthanc internals and
//! unknown/missing DICOM values simply become empty strings.

use serde::Serialize;
use serde_json::Value;

/// Reads a JSON string value, trimmed; missing or non-string values become `""`.
fn text(v: &Value) -> String {
    v.as_str().unwrap_or_default().trim().to_string()
}

/// A DICOM study as shown in the results table.
#[derive(Debug, Clone, Serialize)]
pub struct StudySummary {
    /// Orthanc's internal id (used for ZIP downloads and series listing).
    pub id: String,
    /// DICOM StudyInstanceUID (used to open the study in Weasis).
    pub study_instance_uid: String,
    /// Raw DICOM person name, e.g. `DOE^JOHN` (the UI prettifies it).
    pub patient_name: String,
    pub patient_id: String,
    pub patient_birth_date: String,
    pub patient_sex: String,
    /// `YYYYMMDD` / `HHMMSS` as stored in DICOM.
    pub study_date: String,
    pub study_time: String,
    pub study_description: String,
    pub accession_number: String,
    /// E.g. `["CT", "SR"]`. Empty when Orthanc is too old to provide it.
    pub modalities: Vec<String>,
    pub series_count: Option<usize>,
    pub instances_count: Option<u64>,
}

impl StudySummary {
    /// Builds a summary from an *expanded* Orthanc study (`GET /studies/{id}` or
    /// `/tools/find` with `"Expand": true`).
    pub fn from_orthanc(v: &Value) -> Self {
        let main = &v["MainDicomTags"];
        let patient = &v["PatientMainDicomTags"];
        // `RequestedTags` only exists when the find request asked for it.
        let requested = &v["RequestedTags"];

        let modalities = text(&requested["ModalitiesInStudy"])
            .split('\\') // DICOM multi-value separator
            .filter(|m| !m.is_empty())
            .map(str::to_string)
            .collect();

        Self {
            id: text(&v["ID"]),
            study_instance_uid: text(&main["StudyInstanceUID"]),
            patient_name: text(&patient["PatientName"]),
            patient_id: text(&patient["PatientID"]),
            patient_birth_date: text(&patient["PatientBirthDate"]),
            patient_sex: text(&patient["PatientSex"]),
            study_date: text(&main["StudyDate"]),
            study_time: text(&main["StudyTime"]),
            study_description: text(&main["StudyDescription"]),
            accession_number: text(&main["AccessionNumber"]),
            modalities,
            series_count: v["Series"].as_array().map(|a| a.len()),
            instances_count: text(&requested["NumberOfStudyRelatedInstances"]).parse().ok(),
        }
    }
}

/// A DICOM series (one acquisition / reconstruction inside a study).
#[derive(Debug, Serialize)]
pub struct SeriesSummary {
    pub id: String,
    /// Orthanc id of the parent study (for ZIP download of the whole study).
    pub study_id: String,
    pub series_instance_uid: String,
    pub modality: String,
    pub description: String,
    pub body_part: String,
    pub series_number: String,
    /// Number of images (instances) in the series.
    pub instances_count: usize,
    /// Parent study details. Only filled for *series search* results, where the
    /// patient / study context is needed to make sense of a lone series.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub study: Option<StudySummary>,
}

impl SeriesSummary {
    /// Builds a summary from an *expanded* Orthanc series.
    pub fn from_orthanc(v: &Value) -> Self {
        let main = &v["MainDicomTags"];
        Self {
            id: text(&v["ID"]),
            study_id: text(&v["ParentStudy"]),
            series_instance_uid: text(&main["SeriesInstanceUID"]),
            modality: text(&main["Modality"]),
            description: text(&main["SeriesDescription"]),
            body_part: text(&main["BodyPartExamined"]),
            series_number: text(&main["SeriesNumber"]),
            instances_count: v["Instances"].as_array().map(|a| a.len()).unwrap_or(0),
            study: None,
        }
    }
}

/// A remote DICOM node Orthanc can send studies to (one entry of Orthanc's
/// `DicomModalities` configuration).
#[derive(Debug, Serialize)]
pub struct ModalityNode {
    /// Alias used in Orthanc's configuration and REST API (`/modalities/{id}`).
    pub id: String,
    /// Application Entity Title of the remote node.
    pub aet: String,
    pub host: String,
    pub port: u64,
    pub manufacturer: String,
}

impl ModalityNode {
    /// Builds a node from its expanded Orthanc configuration.
    pub fn from_orthanc(id: &str, cfg: &Value) -> Self {
        Self {
            id: id.to_string(),
            aet: text(&cfg["AET"]),
            host: text(&cfg["Host"]),
            port: cfg["Port"].as_u64().unwrap_or(0),
            manufacturer: text(&cfg["Manufacturer"]),
        }
    }

    /// A node known only by its alias (fallback when Orthanc gives no details).
    pub fn bare(id: &str) -> Self {
        Self { id: id.to_string(), aet: String::new(), host: String::new(), port: 0, manufacturer: String::new() }
    }
}
