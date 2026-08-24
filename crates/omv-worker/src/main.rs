//! Conversion worker (design §3.1 component 3, §4).
//!
//! Consumes jobs from the Redis stream, pulls rendered frames from Orthanc,
//! pipes them through ffmpeg into HLS, uploads to object storage, and
//! registers renditions in the catalog. Stateless: run as many as needed.

mod encode;
mod metrics;
mod notify;
mod orthanc;
mod phi;
mod reformat;
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
use sqlx::PgPool;
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
    /// Total conversion attempts before a job is dead-lettered.
    max_attempts: u64,
    /// How long a failed job stays pending before redelivery (the backoff).
    retry_idle: std::time::Duration,
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
        max_attempts: std::env::var("OMV_MAX_ATTEMPTS")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(4),
        retry_idle: std::time::Duration::from_secs(
            std::env::var("OMV_RETRY_IDLE_SECS")
                .ok().and_then(|v| v.parse().ok()).unwrap_or(60),
        ),
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

    let metrics_addr =
        std::env::var("OMV_METRICS_ADDR").unwrap_or_else(|_| "0.0.0.0:9464".into());
    tokio::spawn(async move {
        if let Err(e) = metrics::serve(&metrics_addr).await {
            error!(error = %e, "metrics server exited");
        }
    });

    let consumer = format!("worker-{}", std::process::id());
    let read_opts = StreamReadOptions::default()
        .group(config::JOB_GROUP, &consumer)
        .count(1)
        .block(5000);
    info!(consumer, "omv-worker ready");

    loop {
        // Retry pass: reclaim jobs whose previous attempt failed (they stay
        // pending un-acked) once they have been idle for the backoff period.
        // The stream's delivery counter is the attempt counter.
        if let Err(e) = worker.handle_retries(&mut conn, &consumer).await {
            warn!(error = %e, "retry pass failed");
        }
        worker.update_queue_gauges(&mut conn).await;

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
                worker.handle_delivery(&mut conn, &entry, 1).await;
            }
        }
    }
}

/// Extracts the job payload from a stream entry.
fn job_of(entry: &redis::streams::StreamId) -> Option<(String, ConversionJob)> {
    let payload: String = entry.get("job")?;
    let job = serde_json::from_str(&payload).ok()?;
    Some((payload, job))
}

impl Worker {
    /// One delivery of a job (attempt N of max). Success acks; failure
    /// leaves the message pending for the retry pass, or dead-letters it
    /// when this was the final attempt.
    async fn handle_delivery(
        &self,
        conn: &mut redis::aio::MultiplexedConnection,
        entry: &redis::streams::StreamId,
        attempt: u64,
    ) {
        let Some((payload, job)) = job_of(entry) else {
            // Unparseable payload: nothing will ever succeed — dead-letter.
            warn!(id = %entry.id, "unparseable job payload, dead-lettering");
            self.dead_letter(conn, &entry.id, "?", "unparseable payload", attempt).await;
            return;
        };

        info!(study = %job.orthanc_study_id, attempt, max = self.max_attempts, "converting");
        let started = std::time::Instant::now();
        match self.process_study(&job.orthanc_study_id).await {
            Ok(()) => {
                metrics::CONVERSIONS.with_label_values(&["success"]).inc();
                metrics::CONVERSION_SECONDS.observe(started.elapsed().as_secs_f64());
                if let Some(age) = metrics::age_of_entry(&entry.id) {
                    metrics::JOB_TOTAL_SECONDS.observe(age);
                }
                let _: redis::RedisResult<i64> =
                    conn.xack(config::JOB_STREAM, config::JOB_GROUP, &[&entry.id]).await;
            }
            Err(e) if attempt >= self.max_attempts => {
                metrics::CONVERSIONS.with_label_values(&["dead_letter"]).inc();
                if let Some(age) = metrics::age_of_entry(&entry.id) {
                    metrics::JOB_TOTAL_SECONDS.observe(age);
                }
                error!(study = %job.orthanc_study_id, attempt, error = ?e,
                       "conversion failed permanently, dead-lettering");
                self.mark_status(&job.orthanc_study_id, "failed", &format!("{e:#}")).await;
                self.dead_letter(conn, &entry.id, &payload, &format!("{e:#}"), attempt).await;
                self.notifier
                    .broadcast(&self.db, "study.failed", serde_json::json!({
                        "orthanc_study_id": job.orthanc_study_id,
                        "error": format!("{e:#}"),
                        "attempts": attempt,
                    }))
                    .await;
            }
            Err(e) => {
                metrics::CONVERSIONS.with_label_values(&["retry"]).inc();
                warn!(study = %job.orthanc_study_id, attempt, error = ?e,
                      "conversion failed, will retry after backoff");
                self.mark_status(&job.orthanc_study_id, "retrying", &format!("{e:#}")).await;
                // No ack: the message stays pending until the retry pass
                // reclaims it after retry_idle.
            }
        }
    }

    /// Reclaims failed jobs that have been idle long enough and re-runs
    /// them, with the stream's delivery counter as the attempt number.
    async fn handle_retries(
        &self,
        conn: &mut redis::aio::MultiplexedConnection,
        consumer: &str,
    ) -> anyhow::Result<()> {
        let reply: redis::streams::StreamAutoClaimReply = conn
            .xautoclaim_options(
                config::JOB_STREAM,
                config::JOB_GROUP,
                consumer,
                self.retry_idle.as_millis() as usize,
                "0-0",
                redis::streams::StreamAutoClaimOptions::default().count(5),
            )
            .await?;

        for entry in reply.claimed {
            // Delivery count for this specific message.
            let pending: redis::streams::StreamPendingCountReply = conn
                .xpending_count(config::JOB_STREAM, config::JOB_GROUP, &entry.id, &entry.id, 1)
                .await?;
            let attempt = pending
                .ids
                .first()
                .map(|p| p.times_delivered as u64)
                .unwrap_or(self.max_attempts);
            self.handle_delivery(conn, &entry, attempt).await;
        }
        Ok(())
    }

    /// Moves a job to the dead-letter stream with its final error, and acks
    /// it off the work stream. Re-drive by re-POSTing the Orthanc event.
    async fn dead_letter(
        &self,
        conn: &mut redis::aio::MultiplexedConnection,
        entry_id: &str,
        payload: &str,
        error: &str,
        attempts: u64,
    ) {
        let added: redis::RedisResult<String> = redis::cmd("XADD")
            .arg(config::DEAD_STREAM).arg("*")
            .arg("job").arg(payload)
            .arg("error").arg(error)
            .arg("attempts").arg(attempts)
            .arg("at").arg(chrono::Utc::now().to_rfc3339())
            .query_async(conn)
            .await;
        if let Err(e) = added {
            error!(error = %e, "failed to write dead-letter entry");
        }
        let _: redis::RedisResult<i64> =
            conn.xack(config::JOB_STREAM, config::JOB_GROUP, &[entry_id]).await;
    }

    /// Refreshes the queue gauges (stream length and delivered-but-unacked
    /// count); runs once per main-loop iteration, i.e. at least every 5 s.
    async fn update_queue_gauges(&self, conn: &mut redis::aio::MultiplexedConnection) {
        if let Ok(len) = redis::cmd("XLEN")
            .arg(config::JOB_STREAM)
            .query_async::<i64>(conn)
            .await
        {
            metrics::QUEUE_DEPTH.set(len);
        }
        // XPENDING summary reply: [count, min-id, max-id, consumers].
        if let Ok(redis::Value::Array(items)) = redis::cmd("XPENDING")
            .arg(config::JOB_STREAM)
            .arg(config::JOB_GROUP)
            .query_async::<redis::Value>(conn)
            .await
        {
            if let Some(redis::Value::Int(count)) = items.first() {
                metrics::QUEUE_PENDING.set(*count);
            }
        }
    }

    async fn mark_status(&self, orthanc_id: &str, status: &str, err: &str) {
        let _ = sqlx::query(
            "UPDATE studies SET status=$2, error=$3, updated_at=now() WHERE orthanc_id=$1",
        )
        .bind(orthanc_id)
        .bind(status)
        .bind(err)
        .execute(&self.db)
        .await;
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

        // (instance id, frame index) in playback order.
        let (frames, sort_method) = self.frame_list(&series).await?;
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

        // Multiplanar reformats need trustworthy geometry and clean pixels:
        // geometric ordering succeeded, a real stack, and no PHI mask (a
        // masked band would streak through every reformatted frame).
        let reformat_eligible = modality == "CT"
            && sort_method == "geometric"
            && frames.len() >= 20
            && phi_filter.is_none();
        let mut volume = if reformat_eligible {
            Some(reformat::Volume::new())
        } else {
            None
        };

        let presets = models::presets_for(&modality, Some(body_part));
        info!(
            series = %series_uid, %modality, body_part,
            presets = ?presets.iter().map(|p| p.key).collect::<Vec<_>>(),
            "window presets selected"
        );

        let mut renditions = Vec::new();
        for (preset_idx, preset) in presets.iter().enumerate() {
            let dir = tempfile::tempdir().context("tempdir")?;
            let mut enc = encode::HlsEncoder::start(
                dir.path(), fps, !cine, self.encoder, phi_filter.as_deref(),
            )?;
            for (instance_id, frame) in &frames {
                let png = self
                    .orthanc
                    .rendered_frame(instance_id, *frame, preset.center_width)
                    .await?;
                // Collect the first preset's slices into a volume for the
                // coronal/sagittal reformats.
                if preset_idx == 0 {
                    if let Some(vol) = volume.as_mut() {
                        if let Err(e) = vol.push_png(&png) {
                            warn!(error = %e, "reformat volume abandoned");
                            volume = None;
                        }
                    }
                }
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

        if let Some(vol) = volume.filter(|v| v.n_slices() >= 20) {
            match self
                .encode_reformats(study_uid, &series_uid, &series_description,
                                  &modality, &vol, &inst_tags, &presets[0])
                .await
            {
                Ok(mut r) => renditions.append(&mut r),
                // Reformats are a bonus view; never sink the series on them.
                Err(e) => warn!(series = %series_uid, error = ?e, "reformats skipped"),
            }
        }

        Ok(Some((modality, renditions)))
    }

    /// Encodes coronal and sagittal renditions from the stacked volume,
    /// stretching the slice axis to true aspect via the DICOM spacings.
    #[allow(clippy::too_many_arguments)]
    async fn encode_reformats(
        &self,
        study_uid: &str,
        series_uid: &str,
        series_description: &str,
        modality: &str,
        vol: &reformat::Volume,
        inst_tags: &Value,
        first_preset: &models::WindowPreset,
    ) -> Result<Vec<Rendition>> {
        let tag_f64 = |name: &str| -> Option<f64> {
            inst_tags[name]
                .as_str()
                .and_then(|s| s.split('\\').next())
                .and_then(|s| s.trim().parse::<f64>().ok())
        };
        let pixel_spacing = tag_f64("PixelSpacing").unwrap_or(1.0);
        let slice_gap = tag_f64("SpacingBetweenSlices")
            .or_else(|| tag_f64("SliceThickness"))
            .unwrap_or(pixel_spacing);
        let z_scale = (slice_gap / pixel_spacing).clamp(0.2, 10.0);
        let z_out = ((vol.n_slices() as f64) * z_scale).round() as usize;
        let fps = models::stack_fps(modality);

        let mut out = Vec::new();
        for plane in ["coronal", "sagittal"] {
            let (in_w, n_frames) = match plane {
                "coronal" => (vol.width, vol.height),
                _ => (vol.height, vol.width),
            };
            let dir = tempfile::tempdir().context("tempdir")?;
            let mut enc = encode::HlsEncoder::start_raw(
                dir.path(), fps, self.encoder,
                (in_w, vol.n_slices()), (in_w, z_out),
            )?;
            match plane {
                "coronal" => {
                    for frame in vol.coronal() {
                        enc.write_frame(&frame).await?;
                    }
                }
                _ => {
                    for frame in vol.sagittal() {
                        enc.write_frame(&frame).await?;
                    }
                }
            }
            enc.finish().await?;
            encode::mux_export(dir.path(), self.encoder).await?;

            let key = format!("{}-{}", first_preset.key, &plane[..3]);
            let prefix = format!("studies/{study_uid}/{series_uid}/{key}");
            self.upload_dir(dir.path(), &prefix).await?;
            info!(series = %series_uid, plane, frames = n_frames, z_out, "reformat encoded");

            out.push(Rendition {
                series_uid: series_uid.to_string(),
                series_description: series_description.to_string(),
                modality: modality.to_string(),
                preset: key.clone(),
                preset_label: format!(
                    "{} ({})",
                    if plane == "coronal" { "Coronal" } else { "Sagittal" },
                    first_preset.label
                ),
                playlist: format!("{series_uid}/{key}/index.m3u8"),
                frames: n_frames as i32,
                fps,
            });
        }
        Ok(out)
    }

    /// Builds the ordered (instance, frame) list for a series: multi-frame
    /// cine instances expand to their frames; single-frame stacks are ordered
    /// geometrically by ImagePositionPatient projected onto the series
    /// normal, with InstanceNumber as the fallback (design §4.1).
    async fn frame_list(&self, series: &Value) -> Result<(Vec<(String, u32)>, &'static str)> {
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
        Ok((frames, method))
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
