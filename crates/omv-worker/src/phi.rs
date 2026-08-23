//! Pixel-domain PHI stripping (design §7.2).
//!
//! DICOM overlay planes (group 60xx) never reach the video: Orthanc's
//! /rendered endpoint draws pixel data only. What remains is PHI *burned
//! into the pixels themselves* — ultrasound demographic banners, cath-lab
//! annotations — which no tag can remove. Those are handled by per-model
//! crop/mask rules, loaded from a JSON file (OMV_PHI_RULES) so ops can add
//! rules for newly observed machines without a release:
//!
//! ```json
//! [
//!   { "match":  { "modality": "US" },
//!     "action": { "mask": [ { "x": 0, "y": 0, "w": 10000, "h": 48 } ] } },
//!   { "match":  { "manufacturer": "acme", "model": "echomax" },
//!     "action": { "crop_top": 60 } }
//! ]
//! ```
//!
//! Matching is case-insensitive substring on modality / Manufacturer /
//! ManufacturerModelName; the FIRST matching rule applies. Masks paint
//! black boxes (regions clamp to the frame), crops remove edge bands.
//! An instance whose BurnedInAnnotation tag says YES but matches no rule is
//! a policy decision: convert with a loud warning (default) or skip the
//! series (OMV_PHI_UNMATCHED_BURNEDIN=skip).

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::info;

#[derive(Debug, Deserialize)]
pub struct Rule {
    #[serde(default, rename = "match")]
    pub matcher: Matcher,
    pub action: Action,
}

#[derive(Debug, Default, Deserialize)]
pub struct Matcher {
    pub modality: Option<String>,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Action {
    #[serde(default)]
    pub crop_top: u32,
    #[serde(default)]
    pub crop_bottom: u32,
    #[serde(default)]
    pub mask: Vec<Rect>,
}

#[derive(Debug, Deserialize)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

pub fn load(path: Option<&str>) -> Result<Vec<Rule>> {
    let Some(path) = path else { return Ok(Vec::new()) };
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading PHI rules from {path}"))?;
    let rules: Vec<Rule> =
        serde_json::from_str(&text).with_context(|| format!("parsing PHI rules {path}"))?;
    info!(count = rules.len(), path, "PHI-strip rules loaded");
    Ok(rules)
}

fn matches(needle: &Option<String>, hay: &str) -> bool {
    match needle {
        None => true,
        Some(n) => hay.to_lowercase().contains(&n.to_lowercase()),
    }
}

/// First rule whose (all-present) criteria match this series.
pub fn find<'a>(
    rules: &'a [Rule],
    modality: &str,
    manufacturer: &str,
    model: &str,
) -> Option<&'a Action> {
    rules
        .iter()
        .find(|r| {
            matches(&r.matcher.modality, modality)
                && matches(&r.matcher.manufacturer, manufacturer)
                && matches(&r.matcher.model, model)
        })
        .map(|r| &r.action)
}

/// Compiles an action into an ffmpeg filter fragment (masks first, then
/// crops), to be prepended to the encoder's filter chain.
pub fn to_filter(action: &Action) -> Option<String> {
    let mut parts: Vec<String> = action
        .mask
        .iter()
        .map(|m| {
            format!(
                "drawbox=x={}:y={}:w=min({}\\,iw-{}):h=min({}\\,ih-{}):color=black:t=fill",
                m.x, m.y, m.w, m.x, m.h, m.y
            )
        })
        .collect();
    if action.crop_top > 0 || action.crop_bottom > 0 {
        parts.push(format!(
            "crop=iw:ih-{}:0:{}",
            action.crop_top + action.crop_bottom,
            action.crop_top
        ));
    }
    if parts.is_empty() { None } else { Some(parts.join(",")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> Vec<Rule> {
        serde_json::from_str(
            r#"[
              {"match": {"manufacturer": "acme", "model": "echomax"},
               "action": {"crop_top": 60}},
              {"match": {"modality": "US"},
               "action": {"mask": [{"x":0,"y":0,"w":10000,"h":48}]}}
            ]"#,
        )
        .unwrap()
    }

    #[test]
    fn first_match_wins_and_is_case_insensitive() {
        let r = rules();
        // Specific machine rule outranks the generic US rule (listed first).
        let a = find(&r, "US", "ACME Medical", "EchoMax 3000").unwrap();
        assert_eq!(a.crop_top, 60);
        // Other US machines fall through to the generic mask.
        let a = find(&r, "US", "Other Corp", "Sono1").unwrap();
        assert_eq!(a.mask.len(), 1);
        // CT matches nothing.
        assert!(find(&r, "CT", "ACME Medical", "").is_none());
    }

    #[test]
    fn filter_compilation() {
        let a: Action = serde_json::from_str(
            r#"{"crop_top": 60, "mask": [{"x":0,"y":0,"w":10000,"h":48}]}"#,
        )
        .unwrap();
        let f = to_filter(&a).unwrap();
        assert!(f.starts_with("drawbox="), "masks apply before crops: {f}");
        assert!(f.contains("color=black:t=fill"));
        assert!(f.ends_with("crop=iw:ih-60:0:60"));
        assert_eq!(to_filter(&Action::default()), None);
    }
}
