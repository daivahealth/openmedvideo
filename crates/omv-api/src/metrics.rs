//! API metrics: HTTP request counts/latency per matched route, and the
//! playback error rate (design §9 Phase 2). Served on the internal metrics
//! port (9464), never proxied by nginx.

use axum::{
    extract::{MatchedPath, Request},
    middleware::Next,
    response::IntoResponse,
};
use once_cell::sync::Lazy;
use prometheus::{
    register_histogram_vec, register_int_counter_vec, Encoder, HistogramVec, IntCounterVec,
    TextEncoder,
};

static HTTP_REQUESTS: Lazy<IntCounterVec> = Lazy::new(|| {
    register_int_counter_vec!(
        "omv_http_requests_total",
        "HTTP requests by matched route and status class",
        &["route", "status"]
    )
    .unwrap()
});

static HTTP_SECONDS: Lazy<HistogramVec> = Lazy::new(|| {
    register_histogram_vec!(
        "omv_http_request_seconds",
        "HTTP request latency by matched route",
        &["route"],
        vec![0.005, 0.025, 0.1, 0.25, 1.0, 2.5, 10.0]
    )
    .unwrap()
});

/// Playback delivery outcomes: `error rate = 1 - ok / total`. Separated from
/// generic HTTP counts because playback is the doctor-facing SLI.
pub static PLAYBACK: Lazy<IntCounterVec> = Lazy::new(|| {
    register_int_counter_vec!(
        "omv_playback_requests_total",
        "Playback object requests by outcome",
        &["outcome"] // ok | unauthorized | not_found
    )
    .unwrap()
});

/// Axum middleware: records count + latency for every request against the
/// matched route template (not the raw path — no UID cardinality explosion).
pub async fn track(req: Request, next: Next) -> impl IntoResponse {
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "unmatched".to_string());
    let start = std::time::Instant::now();
    let res = next.run(req).await;
    let status = match res.status().as_u16() {
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        _ => "5xx",
    };
    HTTP_REQUESTS.with_label_values(&[&route, status]).inc();
    HTTP_SECONDS
        .with_label_values(&[&route])
        .observe(start.elapsed().as_secs_f64());
    res
}

async fn render() -> impl IntoResponse {
    let mut buf = Vec::new();
    TextEncoder::new()
        .encode(&prometheus::gather(), &mut buf)
        .expect("encode metrics");
    ([(axum::http::header::CONTENT_TYPE, prometheus::TEXT_FORMAT)], buf)
}

pub async fn serve(addr: &str) -> anyhow::Result<()> {
    let app = axum::Router::new().route("/metrics", axum::routing::get(render));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
