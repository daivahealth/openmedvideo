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
    Ready,
    Failed,
}

impl StudyStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Converting => "converting",
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

/// Phase 1: hardcoded presets per modality (body-part-driven selection is
/// Phase 2). Cine modalities keep their native rendering; CT gets the three
/// standard chest/general windows.
pub fn presets_for(modality: &str) -> &'static [WindowPreset] {
    const DEFAULT: &[WindowPreset] =
        &[WindowPreset { key: "default", label: "Default", center_width: None }];
    const CT: &[WindowPreset] = &[
        WindowPreset { key: "soft", label: "Soft tissue", center_width: Some((40, 400)) },
        WindowPreset { key: "lung", label: "Lung", center_width: Some((-600, 1500)) },
        WindowPreset { key: "bone", label: "Bone", center_width: Some((300, 1500)) },
    ];
    match modality {
        "CT" => CT,
        _ => DEFAULT,
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
