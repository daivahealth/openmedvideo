//! Conversion worker (design §3.1 component 3, §4).
//!
//! Consumes jobs from the Redis stream, pulls rendered frames from Orthanc,
//! pipes them through ffmpeg into HLS, uploads to object storage, and
//! registers renditions in the catalog. Stateless: run as many as needed.

mod encode;
mod notify;
mod orthanc;
mod phi;
mod sorting;

use anyhow::{Context, Result};
use omv_core::{
    config::{self, Config},
    models::{self, ConversionJob, Rendition, StudyManifest, StudyStatus},
    storage::Storage,
};
use orthanc::Orthanc;
use redis::streams::{StreamReadOptions, StreamReadReply};
use redis::AsyncCommands;
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use tracing::{error, info, warn};

struct Worker {
    cfg: Config,
    db: PgPool,
    store: Storage,
    orthanc: Orthanc,
    encoder: encode::Encoder,
    notifier: notify::Notifier,
    phi_rules: Vec<phi::Rule>,
    /// OMV_PHI_UNMATCHED_BURNEDIN=skip: refuse to convert a series whose
    /// BurnedInAnnotation tag says YES when no strip rule matches it.
    phi_skip_unmatched: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cfg = Config::from_env()?;
    let encoder_pref = std::env::var("OMV_ENCODER").unwrap_or_else(|_| "auto".into());
    let worker = Worker {
        db: PgPoolOptions::new().max_connections(4).connect(&cfg.database_url).await?,
        store: Storage::from_url(&cfg.storage_url)?,
        orthanc: Orthanc::new(&cfg.orthanc_url, &cfg.orthanc_user, &cfg.orthanc_password),
        encoder: encode::detect_encoder(&encoder_pref).await?,
        notifier: notify::Notifier::default(),
        phi_rules: phi::load(std::env::var("OMV_PHI_RULES").ok().as_deref())?,
        phi_skip_unmatched: std::env::var("OMV_PHI_UNMATCHED_BURNEDIN").as_deref()
            == Ok("skip"),
        cfg,
    };

    let client = redis::Client::open(worker.cfg.redis_url.clone())?;
    let mut conn = client.get_multiplexed_async_connection().await?;

    // Create the consumer group; BUSYGROUP just means it already exists.
    let created: redis::RedisResult<String> = conn
        .xgroup_create_mkstream(config::JOB_STREAM, config::JOB_GROUP, "0")
        .await;
    if let Err(e) = created {
        if !e.to_string().contains("BUSYGROUP") {
            return Err(e.into());
        }
    }

    let consumer = format!("worker-{}", std::process::id());
    let read_opts = StreamReadOptions::default()
        .group(config::JOB_GROUP, &consumer)
        .count(1)
        .block(5000);
    info!(consumer, "omv-worker ready");

    loop {
        let reply: StreamReadReply = match conn
            .xread_options(&[config::JOB_STREAM], &[">"], &read_opts)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "queue read failed, retrying");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }
        };

        for key in reply.keys {
            for entry in key.ids {
                let job: Option<ConversionJob> = entry
                    .get::<String>("job")
                    .and_then(|p| serde_json::from_str(&p).ok());
                if let Some(job) = job {
                    info!(study = %job.orthanc_study_id, "converting");
                    if let Err(e) = worker.process_study(&job.orthanc_study_id).await {
                        error!(study = %job.orthanc_study_id, error = ?e, "conversion failed");
                        worker.mark_failed(&job.orthanc_study_id, &format!("{e:#}")).await;
                    }
                }
                // Ack either way; failed studies stay visible in the catalog
                // with status=failed. Dead-letter retry/backoff is Phase 2.
                let _: redis::RedisResult<i64> = conn
                    .xack(config::JOB_STREAM, config::JOB_GROUP, &[&entry.id])
                    .await;
            }
        }
    }
}

impl Worker {
    async fn mark_failed(&self, orthanc_id: &str, err: &str) {
        let row = sqlx::query(
            "UPDATE studies SET status='failed', error=$2, updated_at=now()
             WHERE orthanc_id=$1 RETURNING study_uid",
        )
        .bind(orthanc_id)
        .bind(err)
        .fetch_optional(&self.db)
        .await;
        if let Ok(Some(r)) = row {
            let study_uid: String = r.get("study_uid");
            self.notifier
                .broadcast(&self.db, "study.failed", serde_json::json!({
                    "study_uid": study_uid,
                    "error": err,
                }))
                .await;
        }
    }

    async fn process_study(&self, orthanc_id: &str) -> Result<()> {
        let study = self.orthanc.get_json(&format!("/studies/{orthanc_id}")).await?;
        let tags = &study["MainDicomTags"];
        let study_uid = tags["StudyInstanceUID"]
            .as_str()
            .context("study has no StudyInstanceUID")?
            .to_string();
        let description = tags["StudyDescription"].as_str().unwrap_or("").to_string();
        // Coded reference only — never demographics (design §7.2).
        let patient_ref = study["ParentPatient"].as_str().unwrap_or("").to_string();

        sqlx::query(
            "INSERT INTO studies (study_uid, orthanc_id, description, patient_ref, status)
             VALUES ($1,$2,$3,$4,'converting')
             ON CONFLICT (study_uid) DO UPDATE
               SET status='converting', error=NULL, updated_at=now()",
        )
        .bind(&study_uid).bind(orthanc_id).bind(&description).bind(&patient_ref)
        .execute(&self.db)
        .await?;
        sqlx::query("DELETE FROM renditions WHERE study_uid=$1")
            .bind(&study_uid)
            .execute(&self.db)
            .await?;

        let mut renditions = Vec::new();
        let mut modalities = Vec::new();
        let mut poster_done = false;

        for series_id in study["Series"].as_array().context("no Series")?.iter() {
            let series_id = series_id.as_str().context("series id")?;
            match self
                .process_series(&study_uid, series_id, &mut poster_done)
                .await
            {
                Ok(Some((modality, mut rs))) => {
                    if !modalities.contains(&modality) {
                        modalities.push(modality);
                    }
                    renditions.append(&mut rs);
                }
                Ok(None) => {}
                // One bad series shouldn't sink the study; record and move on.
                Err(e) => warn!(series = series_id, error = ?e, "series skipped"),
            }
        }

        for r in &renditions {
            sqlx::query(
                "INSERT INTO renditions (study_uid, series_uid, series_description, modality,
                   preset, preset_label, playlist, frames, fps)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)
                 ON CONFLICT (study_uid, series_uid, preset) DO NOTHING",
            )
            .bind(&study_uid).bind(&r.series_uid).bind(&r.series_description)
            .bind(&r.modality).bind(&r.preset).bind(&r.preset_label)
            .bind(&r.playlist).bind(r.frames).bind(r.fps)
            .execute(&self.db)
            .await?;
        }

        let manifest = StudyManifest {
            study_uid: study_uid.clone(),
            description: description.clone(),
            poster: "poster.jpg".into(),
            renditions: renditions.clone(),
            disclaimer: models::DISCLAIMER.into(),
        };
        self.store
            .put(
                &format!("studies/{study_uid}/manifest.json"),
                serde_json::to_vec_pretty(&manifest)?,
            )
            .await?;

        let status =
            if renditions.is_empty() { StudyStatus::Failed } else { StudyStatus::Ready };
        sqlx::query(
            "UPDATE studies SET status=$2, modalities=$3, updated_at=now() WHERE study_uid=$1",
        )
        .bind(&study_uid)
        .bind(status.as_str())
        .bind(modalities.join(","))
        .execute(&self.db)
        .await?;

        let event = if status == StudyStatus::Ready { "study.ready" } else { "study.failed" };
        self.notifier
            .broadcast(&self.db, event, serde_json::json!({
                "study_uid": study_uid,
                "description": description,
                "modalities": modalities.join(","),
                "renditions": renditions.len(),
            }))
            .await;

        info!(study = %study_uid, renditions = renditions.len(), "study ready");
        Ok(())
    }

    /// Renders one series into HLS, one output per window preset.
    /// Returns None for modalities that don't get a video (e.g. CR/DX stills,
    /// which are a Phase 2 image path).
    async fn process_series(
        &self,
        study_uid: &str,
        series_id: &str,
        poster_done: &mut bool,
    ) -> Result<Option<(String, Vec<Rendition>)>> {
        let series = self.orthanc.get_json(&format!("/series/{series_id}")).await?;
        let tags = &series["MainDicomTags"];
        let modality = tags["Modality"].as_str().unwrap_or("").to_string();
        let series_uid = tags["SeriesInstanceUID"]
            .as_str()
            .context("series has no SeriesInstanceUID")?
            .to_string();
        let series_description =
            tags["SeriesDescription"].as_str().unwrap_or("").to_string();
        let body_part = tags["BodyPartExamined"].as_str().unwrap_or("");

        if matches!(modality.as_str(), "CR" | "DX" | "MG" | "PR" | "SR" | "KO") {
            info!(series = %series_uid, %modality, "non-video modality, skipping");
            return Ok(None);
        }

        // (instance id, frame index) in playback order. Phase 1 sorts by
        // InstanceNumber; geometric sort by ImagePositionPatient is Phase 2.
        let frames = self.frame_list(&series).await?;
        if frames.is_empty() {
            return Ok(None);
        }

        let cine = models::is_cine(&modality);
        let fps = if cine {
            self.cine_fps(&frames[0].0).await.unwrap_or(15.0)
        } else {
            models::stack_fps(&modality)
        };

        // PHI stripping (design §7.2): match per-model crop/mask rules on
        // modality + manufacturer + model from the first instance's tags.
        let inst_tags = self
            .orthanc
            .get_json(&format!("/instances/{}/simplified-tags", frames[0].0))
            .await
            .unwrap_or_default();
        let manufacturer = tags["Manufacturer"]
            .as_str()
            .or_else(|| inst_tags["Manufacturer"].as_str())
            .unwrap_or("");
        let model = inst_tags["ManufacturerModelName"].as_str().unwrap_or("");
        let burned_in = inst_tags["BurnedInAnnotation"]
            .as_str()
            .map(|s| s.trim().eq_ignore_ascii_case("YES"))
            .unwrap_or(false);

        let action = phi::find(&self.phi_rules, &modality, manufacturer, model);
        let phi_filter = action.and_then(phi::to_filter);
        match (&phi_filter, burned_in) {
            (Some(f), _) => {
                info!(series = %series_uid, manufacturer, model, filter = %f,
                      "PHI-strip rule applied");
            }
            (None, true) if self.phi_skip_unmatched => {
                warn!(series = %series_uid, manufacturer, model,
                      "BurnedInAnnotation=YES with no matching PHI rule; \
                       series skipped (OMV_PHI_UNMATCHED_BURNEDIN=skip)");
                return Ok(None);
            }
            (None, true) => {
                warn!(series = %series_uid, manufacturer, model,
                      "BurnedInAnnotation=YES with no matching PHI rule; \
                       converting anyway — add a rule for this machine");
            }
            (None, false) => {}
        }

        let presets = models::presets_for(&modality, Some(body_part));
        info!(
            series = %series_uid, %modality, body_part,
            presets = ?presets.iter().map(|p| p.key).collect::<Vec<_>>(),
            "window presets selected"
        );

        let mut renditions = Vec::new();
        for preset in presets {
            let dir = tempfile::tempdir().context("tempdir")?;
            let mut enc = encode::HlsEncoder::start(
                dir.path(), fps, !cine, self.encoder, phi_filter.as_deref(),
            )?;
            for (instance_id, frame) in &frames {
                let png = self
                    .orthanc
                    .rendered_frame(instance_id, *frame, preset.center_width)
                    .await?;
                enc.write_frame(&png).await?;
            }
            enc.finish().await?;
            // export.mp4 lands in the same dir and uploads with the segments.
            encode::mux_export(dir.path(), self.encoder).await?;

            // Poster: middle frame of the ENCODED video, so it inherits the
            // PHI stripping and windowing — never straight from the source.
            if !*poster_done {
                let mid_secs = (frames.len() / 2) as f64 / fps;
                let jpg = encode::extract_poster(dir.path(), mid_secs).await?;
                self.store
                    .put(&format!("studies/{study_uid}/poster.jpg"), jpg)
                    .await?;
                *poster_done = true;
            }

            let prefix = format!("studies/{study_uid}/{series_uid}/{}", preset.key);
            self.upload_dir(dir.path(), &prefix).await?;

            renditions.push(Rendition {
                series_uid: series_uid.clone(),
                series_description: series_description.clone(),
                modality: modality.clone(),
                preset: preset.key.into(),
                preset_label: preset.label.into(),
                playlist: format!("{series_uid}/{}/index.m3u8", preset.key),
                frames: frames.len() as i32,
                fps,
            });
        }
        Ok(Some((modality, renditions)))
    }

    /// Builds the ordered (instance, frame) list for a series: multi-frame
    /// cine instances expand to their frames; single-frame stacks are ordered
    /// geometrically by ImagePositionPatient projected onto the series
    /// normal, with InstanceNumber as the fallback (design §4.1).
    async fn frame_list(&self, series: &Value) -> Result<Vec<(String, u32)>> {
        let ids: Vec<String> = series["Instances"]
            .as_array()
            .context("no Instances")?
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();

        let mut slices = Vec::new();
        for id in &ids {
            let meta = self.orthanc.get_json(&format!("/instances/{id}")).await?;
            let tags = &meta["MainDicomTags"];
            let frame_count = self
                .orthanc
                .get_json(&format!("/instances/{id}/frames"))
                .await?
                .as_array()
                .map(|a| a.len() as u32)
                .unwrap_or(1);
            slices.push(sorting::SliceRef {
                id: id.clone(),
                instance_number: tags["InstanceNumber"]
                    .as_str()
                    .and_then(|s| s.trim().parse().ok())
                    .unwrap_or(0),
                position: tags["ImagePositionPatient"]
                    .as_str()
                    .and_then(sorting::parse_position),
                frames: frame_count,
            });
        }

        // Orientation is a series-level property; read it from one instance.
        let orientation = match slices.first() {
            Some(first) if slices.len() > 1 => self
                .orthanc
                .get_json(&format!("/instances/{}/simplified-tags", first.id))
                .await
                .ok()
                .and_then(|t| {
                    t["ImageOrientationPatient"]
                        .as_str()
                        .and_then(sorting::parse_orientation)
                }),
            _ => None,
        };

        let method = sorting::sort_slices(orientation, &mut slices);
        info!(slices = slices.len(), method, "slice order determined");

        let mut frames = Vec::new();
        for s in slices {
            for f in 0..s.frames.max(1) {
                frames.push((s.id.clone(), f));
            }
        }
        Ok(frames)
    }

    /// Native cine rate from the DICOM tags, per design §4.2.
    async fn cine_fps(&self, instance_id: &str) -> Option<f64> {
        let tags = self
            .orthanc
            .get_json(&format!("/instances/{instance_id}/simplified-tags"))
            .await
            .ok()?;
        if let Some(r) = tags["RecommendedDisplayFrameRate"].as_str() {
            return r.trim().parse().ok();
        }
        if let Some(r) = tags["CineRate"].as_str() {
            return r.trim().parse().ok();
        }
        if let Some(ft) = tags["FrameTime"].as_str() {
            let ms: f64 = ft.trim().parse().ok()?;
            if ms > 0.0 {
                return Some(1000.0 / ms);
            }
        }
        None
    }

    /// Uploads every file in a flat directory under the given storage prefix.
    async fn upload_dir(&self, dir: &std::path::Path, prefix: &str) -> Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let data = std::fs::read(entry.path())?;
            self.store.put(&format!("{prefix}/{name}"), data).await?;
        }
        Ok(())
    }
}
