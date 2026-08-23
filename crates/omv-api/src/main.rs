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
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing::{error, info};

mod auth;
mod fhir;

#[derive(Clone)]
pub struct AppState {
    pub cfg: Config,
    pub db: PgPool,
    pub redis: redis::Client,
    pub store: storage::Storage,
    pub jwks: auth::JwksCache,
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

    if std::env::var("OMV_SEED_DEV_CLIENT").as_deref() == Ok("1") {
        auth::seed_dev_client(&db).await?;
    }

    let state = AppState {
        redis: redis::Client::open(cfg.redis_url.clone())?,
        store: storage::Storage::from_url(&cfg.storage_url)?,
        jwks: auth::JwksCache::default(),
        db,
        cfg,
    };

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/internal/orthanc-event", post(orthanc_event))
        .route("/oauth/token", post(auth::token_endpoint))
        .route("/v1/studies", get(list_studies))
        .route("/v1/studies/{study_uid}", get(get_study))
        .route("/v1/studies/{study_uid}/export", get(export_mp4))
        .route("/fhir/metadata", get(fhir::metadata))
        .route("/fhir/ImagingStudy", get(fhir::search))
        .route("/fhir/ImagingStudy/{study_uid}", get(fhir::read))
        .route("/stream/{token}/{*key}", get(stream_object))
        .route("/player/{token}/{study_uid}", get(player_page))
        .route("/player-assets/hls.min.js", get(hls_js))
        .route("/player-assets/omv-player.js", get(omv_player_js))
        .layer(TraceLayer::new_for_http())
        // Cross-origin embeds of <omv-player> need to fetch /stream and
        // /player-assets from other origins. Access control is the playback
        // token and bearer auth, not the Origin header, so permissive CORS
        // does not widen access.
        .layer(CorsLayer::permissive())
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

/// Authenticates a catalog request and returns the caller's identity.
///
/// Primary path: an OMV access token from POST /oauth/token (token exchange
/// or client_credentials). Deprecated dev fallback: the static bearer tokens
/// from OMV_CLIENT_TOKENS, with the practitioner asserted in a header.
pub(crate) fn authenticate(
    cfg: &Config,
    headers: &HeaderMap,
) -> Result<auth::Identity, ApiError> {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");

    if let Some(i) = cfg.client_tokens.iter().position(|t| t == bearer) {
        return Ok(auth::Identity {
            practitioner: headers
                .get("x-practitioner-id")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("unknown")
                .to_string(),
            client_id: format!("static-client-{i}"),
            scopes: vec!["imaging.read".into(), "imaging.export".into()],
        });
    }
    auth::validate(&cfg.token_secret, bearer).map_err(|_| ApiError::Unauthorized)
}

async fn list_studies(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let id = authenticate(&st.cfg, &headers)?;
    if !id.has_scope("imaging.read") {
        return Err(ApiError::Unauthorized);
    }
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
    let id = authenticate(&st.cfg, &headers)?;
    if !id.has_scope("imaging.read") {
        return Err(ApiError::Unauthorized);
    }

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
    .bind(&id.practitioner)
    .bind(&id.client_id)
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
        "renditions": renditions.iter().map(|r| {
            let series: String = r.get("series_uid");
            let preset: String = r.get("preset");
            json!({
                "series_uid": series,
                "series_description": r.get::<String, _>("series_description"),
                "modality": r.get::<String, _>("modality"),
                "preset": preset,
                "preset_label": r.get::<String, _>("preset_label"),
                "frames": r.get::<i32, _>("frames"),
                "fps": r.get::<f64, _>("fps"),
                "playlist_url": format!("{stream_base}/{}", r.get::<String, _>("playlist")),
                // Explicit user action, imaging.export scope, audited (§7.2).
                "export_url": (st.cfg.export_enabled && id.has_scope("imaging.export"))
                    .then(|| format!(
                        "/v1/studies/{study_uid}/export?series_uid={series}&preset={preset}")),
            })
        }).collect::<Vec<_>>(),
        "token_expires_in_secs": st.cfg.token_ttl_secs,
        // Drop-in playback for WebViews/iframes (design §5.3 tier 1): one
        // URL, no SDK. The page reads everything else from manifest.json.
        "player_url": format!("/player/{playback_token}/{study_uid}"),
    })))
}

/// The embeddable web player. Static HTML compiled into the binary; the
/// token in the path is validated by the /stream endpoints the page calls,
/// so an expired link degrades to a friendly in-page message.
async fn player_page() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        include_str!("../assets/player.html"),
    )
}

/// Vendored hls.js (v1.6.15, Apache-2.0 — see assets/vendor/hls.LICENSE),
/// compiled into the binary so the player works on offline hospital networks
/// with no CDN dependency.
async fn hls_js() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=86400, immutable"),
        ],
        include_str!("../assets/vendor/hls.min.js"),
    )
}

/// The <omv-player> Web Component (design §5.3 tier 2). Host apps include
/// this one script and drop <omv-player server token study-id> anywhere;
/// framework packages are thin wrappers over it.
async fn omv_player_js() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        include_str!("../assets/omv-player.js"),
    )
}

// ---------------------------------------------------------------- export --

#[derive(Deserialize)]
struct ExportQuery {
    series_uid: String,
    preset: String,
}

/// Downloads a rendition as a single MP4 (design §7.2). Requires the
/// imaging.export scope; every request — allowed or denied — is audited.
async fn export_mp4(
    State(st): State<AppState>,
    Path(study_uid): Path<String>,
    axum::extract::Query(q): axum::extract::Query<ExportQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let id = authenticate(&st.cfg, &headers)?;
    let detail = format!("{}:{}", q.series_uid, q.preset);

    if !st.cfg.export_enabled || !id.has_scope("imaging.export") {
        sqlx::query(
            "INSERT INTO audit_events (practitioner, client_app, study_uid, action, detail)
             VALUES ($1,$2,$3,'denied',$4)",
        )
        .bind(&id.practitioner).bind(&id.client_id).bind(&study_uid)
        .bind(format!("export {detail}"))
        .execute(&st.db)
        .await?;
        return Err(ApiError::Unauthorized);
    }

    let key = format!("studies/{study_uid}/{}/{}/export.mp4", q.series_uid, q.preset);
    if key.contains("..") {
        return Err(ApiError::Unauthorized);
    }
    let body = st.store.get(&key).await.map_err(|_| ApiError::NotFound)?;

    sqlx::query(
        "INSERT INTO audit_events (practitioner, client_app, study_uid, action, detail)
         VALUES ($1,$2,$3,'export',$4)",
    )
    .bind(&id.practitioner).bind(&id.client_id).bind(&study_uid).bind(&detail)
    .execute(&st.db)
    .await?;

    let filename = format!("study-{}-{}.mp4", &study_uid[study_uid.len().saturating_sub(8)..], q.preset);
    Ok((
        [
            (header::CONTENT_TYPE, "video/mp4".to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{filename}\"")),
            (header::CACHE_CONTROL, "private, no-store".to_string()),
        ],
        body,
    )
        .into_response())
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

pub(crate) enum ApiError {
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
