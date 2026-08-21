//! Minimal Orthanc REST client.
//!
//! Phase 1 deliberately uses Orthanc's `/rendered` frame endpoint instead of
//! decoding pixel data in Rust: Orthanc applies the modality LUT, VOI LUT and
//! window parameters, and handles every DICOM transfer syntax it stores. That
//! absorbs a decade of DICOM edge cases (design D3). Native decoding with
//! dicom-rs is a Phase 2 optimization for throughput, not correctness.

use anyhow::{Context, Result};
use serde_json::Value;

#[derive(Clone)]
pub struct Orthanc {
    http: reqwest::Client,
    base: String,
    user: String,
    password: String,
}

impl Orthanc {
    pub fn new(base: &str, user: &str, password: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            base: base.trim_end_matches('/').to_string(),
            user: user.to_string(),
            password: password.to_string(),
        }
    }

    pub async fn get_json(&self, path: &str) -> Result<Value> {
        let url = format!("{}{}", self.base, path);
        let res = self
            .http
            .get(&url)
            .basic_auth(&self.user, Some(&self.password))
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .with_context(|| format!("GET {url}"))?;
        Ok(res.json().await?)
    }

    pub async fn get_bytes(&self, path: &str, accept: &str) -> Result<Vec<u8>> {
        let url = format!("{}{}", self.base, path);
        let res = self
            .http
            .get(&url)
            .basic_auth(&self.user, Some(&self.password))
            .header(reqwest::header::ACCEPT, accept)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .with_context(|| format!("GET {url}"))?;
        Ok(res.bytes().await?.to_vec())
    }

    /// One rendered frame as PNG, optionally with an explicit window.
    /// Orthanc scales the image and applies LUTs server-side.
    pub async fn rendered_frame(
        &self,
        instance_id: &str,
        frame: u32,
        window: Option<(i32, i32)>,
    ) -> Result<Vec<u8>> {
        let mut path = format!("/instances/{instance_id}/frames/{frame}/rendered");
        if let Some((center, width)) = window {
            path = format!("{path}?window-center={center}&window-width={width}");
        }
        self.get_bytes(&path, "image/png").await
    }
}
