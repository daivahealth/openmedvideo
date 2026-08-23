use serde::{Deserialize, Serialize};

/// A conversion job, carried on the Redis stream. `orthanc_study_id` is
/// Orthanc's internal id (not the DICOM StudyInstanceUID).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversionJob {
    pub orthanc_study_id: String,
}

/// Processing status of a study in the catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StudyStatus {
    Queued,
    Converting,
    /// A conversion attempt failed; the job is pending redelivery.
    Retrying,
    Ready,
    /// All attempts exhausted; the job sits in the dead-letter stream.
    Failed,
}

impl StudyStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Converting => "converting",
            Self::Retrying => "retrying",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }
}

/// One playable output registered in the catalog: a series rendered with a
/// particular window preset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rendition {
    pub series_uid: String,
    pub series_description: String,
    pub modality: String,
    /// Window preset key, e.g. "default", "lung", "bone".
    pub preset: String,
    /// Human label for the preset switcher in client UIs.
    pub preset_label: String,
    /// Storage key of the HLS playlist, relative to the study prefix.
    pub playlist: String,
    pub frames: i32,
    pub fps: f64,
}

/// The per-study manifest stored alongside the media (§5.3: "the manifest is
/// the contract; every player is just a view over it").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StudyManifest {
    pub study_uid: String,
    pub description: String,
    /// Storage key of the poster image, relative to the study prefix.
    pub poster: String,
    pub renditions: Vec<Rendition>,
    /// Fixed disclaimer — every output is for review, not primary diagnosis.
    pub disclaimer: String,
}

pub const DISCLAIMER: &str = "For clinical review and communication — not for primary diagnosis.";

/// A window/level preset applied when rendering frames.
#[derive(Debug, Clone, Copy)]
pub struct WindowPreset {
    pub key: &'static str,
    pub label: &'static str,
    /// None = let Orthanc apply the VOI LUT from the DICOM tags.
    pub center_width: Option<(i32, i32)>,
}

/// Window presets for a series (design §4.2): cine modalities keep their
/// native rendering; CT selects a clinically appropriate window set from
/// the BodyPartExamined tag, falling back to a general set when the tag is
/// missing or unrecognized.
pub fn presets_for(modality: &str, body_part: Option<&str>) -> &'static [WindowPreset] {
    const DEFAULT: &[WindowPreset] =
        &[WindowPreset { key: "default", label: "Default", center_width: None }];
    match modality {
        "CT" => ct_presets_for_body_part(body_part.unwrap_or("")),
        _ => DEFAULT,
    }
}

/// CT window sets by body part. Matching is contains-based over the
/// uppercased tag because BodyPartExamined arrives as DICOM defined terms
/// (CHEST, HEAD, CSPINE, ...) but also as free-ish text from some consoles.
pub fn ct_presets_for_body_part(body_part: &str) -> &'static [WindowPreset] {
    const HEAD: &[WindowPreset] = &[
        WindowPreset { key: "brain", label: "Brain", center_width: Some((40, 80)) },
        WindowPreset { key: "subdural", label: "Subdural", center_width: Some((75, 215)) },
        WindowPreset { key: "bone", label: "Bone", center_width: Some((600, 2800)) },
    ];
    const CHEST: &[WindowPreset] = &[
        WindowPreset { key: "lung", label: "Lung", center_width: Some((-600, 1500)) },
        WindowPreset { key: "mediastinal", label: "Mediastinal", center_width: Some((50, 350)) },
        WindowPreset { key: "bone", label: "Bone", center_width: Some((300, 1500)) },
    ];
    const ABDOMEN: &[WindowPreset] = &[
        WindowPreset { key: "soft", label: "Soft tissue", center_width: Some((40, 400)) },
        WindowPreset { key: "bone", label: "Bone", center_width: Some((300, 1500)) },
    ];
    const SPINE_NECK: &[WindowPreset] = &[
        WindowPreset { key: "soft", label: "Soft tissue", center_width: Some((50, 350)) },
        WindowPreset { key: "bone", label: "Bone", center_width: Some((600, 2800)) },
    ];
    const GENERAL: &[WindowPreset] = &[
        WindowPreset { key: "soft", label: "Soft tissue", center_width: Some((40, 400)) },
        WindowPreset { key: "lung", label: "Lung", center_width: Some((-600, 1500)) },
        WindowPreset { key: "bone", label: "Bone", center_width: Some((300, 1500)) },
    ];

    let bp = body_part.trim().to_uppercase();
    let has = |terms: &[&str]| terms.iter().any(|t| bp.contains(t));
    if has(&["HEAD", "BRAIN", "SKULL"]) {
        HEAD
    } else if has(&["CHEST", "LUNG", "THORAX"]) {
        CHEST
    } else if has(&["ABDOMEN", "PELVIS", "LIVER", "KIDNEY"]) {
        ABDOMEN
    } else if has(&["SPINE", "NECK"]) {
        SPINE_NECK
    } else {
        GENERAL
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(presets: &[WindowPreset]) -> Vec<&'static str> {
        presets.iter().map(|p| p.key).collect()
    }

    #[test]
    fn ct_presets_follow_body_part() {
        assert_eq!(keys(presets_for("CT", Some("HEAD"))), ["brain", "subdural", "bone"]);
        assert_eq!(keys(presets_for("CT", Some("CHEST"))), ["lung", "mediastinal", "bone"]);
        assert_eq!(keys(presets_for("CT", Some("ABDOMEN"))), ["soft", "bone"]);
        assert_eq!(keys(presets_for("CT", Some("CSPINE"))), ["soft", "bone"]);
        // Free-text and case variants from real consoles.
        assert_eq!(keys(presets_for("CT", Some("Head Brain"))), ["brain", "subdural", "bone"]);
        // Missing or unknown tag falls back to the general set.
        assert_eq!(keys(presets_for("CT", None)), ["soft", "lung", "bone"]);
        assert_eq!(keys(presets_for("CT", Some("EXTREMITY"))), ["soft", "lung", "bone"]);
        // Non-CT modalities are untouched by body part.
        assert_eq!(keys(presets_for("US", Some("HEAD"))), ["default"]);
    }
}

/// Playback frame rate for stack modalities (cine modalities use the rate
/// from the DICOM tags instead).
pub fn stack_fps(modality: &str) -> f64 {
    match modality {
        "CT" | "MR" => 8.0,
        _ => 10.0,
    }
}

/// True for modalities whose frames form a time axis (cine) rather than a
/// spatial stack.
pub fn is_cine(modality: &str) -> bool {
    matches!(modality, "US" | "XA" | "RF")
}
