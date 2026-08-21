//! Streaming / Catalog API (design §3.1 component 5, §5).
//!
//! Phase 1 scope:
//!   - receives Orthanc "stable study" webhooks and enqueues conversion jobs
//!   - serves the study catalog to client apps (static bearer auth for now;
//!     OAuth2/OIDC token exchange arrives in Phase 2)
//!   - issues prefix-scoped playback tokens and streams HLS objects
//!   - writes the append-only audit trail

use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use omv_core::{config, models::StudyStatus, storage, token, Config};
use serde::Deserialize;
use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use tower_http::trace::TraceLayer;
use tracing::{error, info};

#[derive(Clone)]
struct AppState {
    cfg: Config,
    db: PgPool,
    redis: redis::Client,
    store: storage::Storage,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tower_http=info".into()),
        )
        .init();

    let cfg = Config::from_env()?;
    let db = PgPoolOptions::new()
        .max_connections(8)
        .connect(&cfg.database_url)
        .await?;
    sqlx::migrate!("./migrations").run(&db).await?;

    let state = AppState {
        redis: redis::Client::open(cfg.redis_url.clone())?,
        store: storage::Storage::from_url(&cfg.storage_url)?,
        db,
        cfg,
    };

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/internal/orthanc-event", post(orthanc_event))
        .route("/v1/studies", get(list_studies))
        .route("/v1/studies/{study_uid}", get(get_study))
        .route("/stream/{token}/{*key}", get(stream_object))
        .layer(TraceLayer::new_for_http())
        .with_state(state.clone());

    info!(addr = %state.cfg.bind_addr, "omv-api listening");
    let listener = tokio::net::TcpListener::bind(&state.cfg.bind_addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

// ---------------------------------------------------------------- ingest --

#[derive(Deserialize)]
struct OrthancEvent {
    study_id: String,
}

/// Called by Orthanc's Lua hook when a study becomes stable (all instances
/// received). Idempotent: re-delivery just re-enqueues the job.
async fn orthanc_event(
    State(st): State<AppState>,
    Json(ev): Json<OrthancEvent>,
) -> Result<StatusCode, ApiError> {
    let mut conn = st.redis.get_multiplexed_async_connection().await?;
    let payload = serde_json::to_string(&omv_core::models::ConversionJob {
        orthanc_study_id: ev.study_id.clone(),
    })?;
    let _: String = redis::cmd("XADD")
        .arg(config::JOB_STREAM)
        .arg("*")
        .arg("job")
        .arg(&payload)
        .query_async(&mut conn)
        .await?;
    info!(study = %ev.study_id, "conversion job enqueued");
    Ok(StatusCode::ACCEPTED)
}

// --------------------------------------------------------------- catalog --

/// Phase 1 client auth: static bearer tokens from OMV_CLIENT_TOKENS.
/// Returns the client label ("client-N") used in the audit trail.
fn authorize(cfg: &Config, headers: &HeaderMap) -> Result<String, ApiError> {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    match cfg.client_tokens.iter().position(|t| t == bearer) {
        Some(i) => Ok(format!("client-{i}")),
        None => Err(ApiError::Unauthorized),
    }
}

fn practitioner(headers: &HeaderMap) -> String {
    // Phase 1: the client app asserts the practitioner id in a header.
    // Phase 2 replaces this with the identity inside the exchanged OIDC token.
    headers
        .get("x-practitioner-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string()
}

async fn list_studies(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    authorize(&st.cfg, &headers)?;
    let rows = sqlx::query(
        "SELECT study_uid, description, modalities, status, created_at
         FROM studies ORDER BY created_at DESC LIMIT 200",
    )
    .fetch_all(&st.db)
    .await?;
    let studies: Vec<_> = rows
        .iter()
        .map(|r| {
            json!({
                "study_uid": r.get::<String, _>("study_uid"),
                "description": r.get::<String, _>("description"),
                "modalities": r.get::<String, _>("modalities"),
                "status": r.get::<String, _>("status"),
                "created_at": r.get::<chrono::DateTime<Utc>, _>("created_at"),
            })
        })
        .collect();
    Ok(Json(json!({ "studies": studies })))
}

async fn get_study(
    State(st): State<AppState>,
    Path(study_uid): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let client = authorize(&st.cfg, &headers)?;
    let who = practitioner(&headers);

    let study = sqlx::query(
        "SELECT study_uid, description, modalities, status FROM studies WHERE study_uid = $1",
    )
    .bind(&study_uid)
    .fetch_optional(&st.db)
    .await?
    .ok_or(ApiError::NotFound)?;

    let status: String = study.get("status");
    let renditions = sqlx::query(
        "SELECT series_uid, series_description, modality, preset, preset_label,
                playlist, frames, fps
         FROM renditions WHERE study_uid = $1 ORDER BY series_uid, preset",
    )
    .bind(&study_uid)
    .fetch_all(&st.db)
    .await?;

    // Prefix-scoped playback token: one token covers every object of the study.
    let prefix = format!("studies/{study_uid}");
    let claims = token::PlaybackClaims {
        prefix: prefix.clone(),
        exp: Utc::now().timestamp() + st.cfg.token_ttl_secs,
    };
    let playback_token = token::sign(&claims, &st.cfg.token_secret);

    sqlx::query(
        "INSERT INTO audit_events (practitioner, client_app, study_uid, action) VALUES ($1,$2,$3,'view')",
    )
    .bind(&who)
    .bind(&client)
    .bind(&study_uid)
    .execute(&st.db)
    .await?;

    let stream_base = format!("/stream/{playback_token}/{prefix}");
    Ok(Json(json!({
        "study_uid": study_uid,
        "description": study.get::<String, _>("description"),
        "modalities": study.get::<String, _>("modalities"),
        "status": status,
        "disclaimer": omv_core::models::DISCLAIMER,
        "poster_url": (status == StudyStatus::Ready.as_str())
            .then(|| format!("{stream_base}/poster.jpg")),
        "renditions": renditions.iter().map(|r| json!({
            "series_uid": r.get::<String, _>("series_uid"),
            "series_description": r.get::<String, _>("series_description"),
            "modality": r.get::<String, _>("modality"),
            "preset": r.get::<String, _>("preset"),
            "preset_label": r.get::<String, _>("preset_label"),
            "frames": r.get::<i32, _>("frames"),
            "fps": r.get::<f64, _>("fps"),
            "playlist_url": format!("{stream_base}/{}", r.get::<String, _>("playlist")),
        })).collect::<Vec<_>>(),
        "token_expires_in_secs": st.cfg.token_ttl_secs,
    })))
}

// -------------------------------------------------------------- streaming --

/// Serves one stored object after validating the playback token and checking
/// the requested key sits under the token's study prefix.
async fn stream_object(
    State(st): State<AppState>,
    Path((tok, key)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let claims = token::verify(&tok, &st.cfg.token_secret, Utc::now().timestamp())
        .map_err(|_| ApiError::Unauthorized)?;
    if !key.starts_with(&claims.prefix) || key.contains("..") {
        return Err(ApiError::Unauthorized);
    }
    let body = st.store.get(&key).await.map_err(|_| ApiError::NotFound)?;
    Ok((
        [
            (header::CONTENT_TYPE, storage::content_type_for(&key)),
            (header::CACHE_CONTROL, "private, max-age=60"),
        ],
        body,
    )
        .into_response())
}

// ----------------------------------------------------------------- errors --

enum ApiError {
    Unauthorized,
    NotFound,
    Internal(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        Self::Internal(e.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized").into_response(),
            Self::NotFound => (StatusCode::NOT_FOUND, "not found").into_response(),
            Self::Internal(e) => {
                error!(error = %e, "internal error");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
            }
        }
    }
}
