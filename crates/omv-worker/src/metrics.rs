//! Worker metrics (design §9 Phase 2: queue depth, conversion p95, SLO).
//!
//! Exposed as Prometheus text on a dedicated internal port (9464) that nginx
//! never proxies — scrape targets live inside the deployment network only.
//!
//! The 60 s SLO is `omv_job_total_seconds`: enqueue (the Redis stream id
//! carries the XADD timestamp) to conversion outcome, so it includes queue
//! wait and every retry — the doctor-visible number, not just encode time.

use once_cell::sync::Lazy;
use prometheus::{
    register_histogram, register_int_counter_vec, register_int_gauge, Encoder, Histogram,
    IntCounterVec, IntGauge, TextEncoder,
};

pub static CONVERSIONS: Lazy<IntCounterVec> = Lazy::new(|| {
    register_int_counter_vec!(
        "omv_conversions_total",
        "Conversion attempts by outcome",
        &["outcome"] // success | retry | dead_letter
    )
    .unwrap()
});

pub static CONVERSION_SECONDS: Lazy<Histogram> = Lazy::new(|| {
    register_histogram!(
        "omv_conversion_seconds",
        "Wall time of one successful conversion attempt",
        vec![0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0]
    )
    .unwrap()
});

pub static JOB_TOTAL_SECONDS: Lazy<Histogram> = Lazy::new(|| {
    register_histogram!(
        "omv_job_total_seconds",
        "Enqueue-to-outcome time incl. queue wait and retries (60s SLO)",
        vec![1.0, 5.0, 15.0, 30.0, 45.0, 60.0, 90.0, 120.0, 300.0, 600.0]
    )
    .unwrap()
});

pub static QUEUE_DEPTH: Lazy<IntGauge> = Lazy::new(|| {
    register_int_gauge!("omv_queue_depth", "Total entries in the job stream").unwrap()
});

pub static QUEUE_PENDING: Lazy<IntGauge> = Lazy::new(|| {
    register_int_gauge!(
        "omv_queue_pending",
        "Delivered-but-unacked jobs (in flight or awaiting retry)"
    )
    .unwrap()
});

pub static WEBHOOKS: Lazy<IntCounterVec> = Lazy::new(|| {
    register_int_counter_vec!(
        "omv_webhook_deliveries_total",
        "Webhook delivery outcomes",
        &["outcome"] // delivered | gave_up
    )
    .unwrap()
});

/// Seconds since the XADD that produced this stream entry id ("<ms>-<seq>").
pub fn age_of_entry(entry_id: &str) -> Option<f64> {
    let ms: i64 = entry_id.split('-').next()?.parse().ok()?;
    Some(((chrono::Utc::now().timestamp_millis() - ms) as f64 / 1000.0).max(0.0))
}

async fn render() -> impl axum::response::IntoResponse {
    let mut buf = Vec::new();
    TextEncoder::new()
        .encode(&prometheus::gather(), &mut buf)
        .expect("encode metrics");
    ([(axum::http::header::CONTENT_TYPE, prometheus::TEXT_FORMAT)], buf)
}

/// Serves GET /metrics on the internal metrics port; runs for the process
/// lifetime alongside the main loop.
pub async fn serve(addr: &str) -> anyhow::Result<()> {
    let app = axum::Router::new().route("/metrics", axum::routing::get(render));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
