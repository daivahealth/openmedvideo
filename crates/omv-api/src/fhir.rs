//! FHIR R4 exposure (design §5.3 point 5): studies as ImagingStudy resources
//! whose series reference a contained Endpoint carrying the tokenized HLS
//! manifest URL — so EMR/HIS integrations discover video renditions with
//! standard vocabulary instead of our proprietary catalog API.
//!
//! Shape choices:
//!   - `GET /fhir/ImagingStudy` returns a searchset Bundle of summaries
//!     (no endpoints/tokens — cheap to serve, nothing to audit).
//!   - `GET /fhir/ImagingStudy/{uid}` returns the full resource with a
//!     contained Endpoint whose address embeds a fresh short-lived playback
//!     token; the read is audited exactly like a catalog view.
//!   - The Endpoint connection type uses an OMV code system with code "hls";
//!     the manifest.json behind the address remains the contract for
//!     renditions, presets, and frame counts.

use axum::{
    extract::{Path, State},
    http::{header, HeaderMap},
    response::{IntoResponse, Response},
};
use chrono::Utc;
use omv_core::token;
use serde_json::{json, Value};
use sqlx::Row;

use crate::{authenticate, ApiError, AppState};

const FHIR_JSON: &str = "application/fhir+json; charset=utf-8";
const CONNECTION_TYPE_SYSTEM: &str = "https://openmedvideo.org/fhir/endpoint-connection-type";
const DCM: &str = "http://dicom.nema.org/resources/ontology/DCM";

fn fhir_response(body: Value) -> Response {
    ([(header::CONTENT_TYPE, FHIR_JSON)], body.to_string()).into_response()
}

fn modality_codings(csv: &str) -> Vec<Value> {
    csv.split(',')
        .filter(|m| !m.is_empty())
        .map(|m| json!({ "system": DCM, "code": m }))
        .collect()
}

/// GET /fhir/metadata — minimal CapabilityStatement so integrators can
/// discover what this server supports.
pub async fn metadata() -> Response {
    fhir_response(json!({
        "resourceType": "CapabilityStatement",
        "status": "active",
        "date": Utc::now().to_rfc3339(),
        "kind": "instance",
        "software": { "name": "OpenMedVideo" },
        "fhirVersion": "4.0.1",
        "format": ["application/fhir+json"],
        "rest": [{
            "mode": "server",
            "security": { "description":
                "OAuth 2.0 bearer token from POST /oauth/token (RFC 8693 \
                 token exchange or client_credentials); imaging.read scope." },
            "resource": [{
                "type": "ImagingStudy",
                "interaction": [{ "code": "read" }, { "code": "search-type" }]
            }]
        }]
    }))
}

/// GET /fhir/ImagingStudy — searchset Bundle of study summaries.
pub async fn search(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let id = authenticate(&st.cfg, &headers)?;
    if !id.has_scope("imaging.read") {
        return Err(ApiError::Unauthorized);
    }

    let rows = sqlx::query(
        "SELECT s.study_uid, s.description, s.modalities, s.created_at,
                COUNT(DISTINCT r.series_uid) AS n_series,
                COALESCE(SUM(r.frames) FILTER (WHERE r.preset IN ('default','soft')), 0) AS n_frames
         FROM studies s LEFT JOIN renditions r ON r.study_uid = s.study_uid
         WHERE s.status = 'ready'
         GROUP BY s.study_uid ORDER BY s.created_at DESC LIMIT 200",
    )
    .fetch_all(&st.db)
    .await?;

    let entries: Vec<Value> = rows
        .iter()
        .map(|r| {
            let uid: String = r.get("study_uid");
            json!({
                "fullUrl": format!("ImagingStudy/{uid}"),
                "resource": {
                    "resourceType": "ImagingStudy",
                    "id": uid,
                    "identifier": [{ "system": "urn:dicom:uid",
                                     "value": format!("urn:oid:{uid}") }],
                    "status": "available",
                    "modality": modality_codings(&r.get::<String, _>("modalities")),
                    "description": r.get::<String, _>("description"),
                    "started": r.get::<chrono::DateTime<Utc>, _>("created_at").to_rfc3339(),
                    "numberOfSeries": r.get::<i64, _>("n_series"),
                    "numberOfInstances": r.get::<i64, _>("n_frames"),
                }
            })
        })
        .collect();

    Ok(fhir_response(json!({
        "resourceType": "Bundle",
        "type": "searchset",
        "total": entries.len(),
        "entry": entries,
    })))
}

/// GET /fhir/ImagingStudy/{uid} — full resource with a contained Endpoint
/// carrying a tokenized HLS manifest URL. Audited as a view.
pub async fn read(
    State(st): State<AppState>,
    Path(study_uid): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let id = authenticate(&st.cfg, &headers)?;
    if !id.has_scope("imaging.read") {
        return Err(ApiError::Unauthorized);
    }

    let study = sqlx::query(
        "SELECT study_uid, description, modalities, created_at
         FROM studies WHERE study_uid = $1 AND status = 'ready'",
    )
    .bind(&study_uid)
    .fetch_optional(&st.db)
    .await?
    .ok_or(ApiError::NotFound)?;

    let renditions = sqlx::query(
        "SELECT series_uid, series_description, modality,
                MAX(frames) AS frames
         FROM renditions WHERE study_uid = $1
         GROUP BY series_uid, series_description, modality
         ORDER BY series_uid",
    )
    .bind(&study_uid)
    .fetch_all(&st.db)
    .await?;

    // Same token semantics as the catalog: prefix-scoped, short-lived.
    let prefix = format!("studies/{study_uid}");
    let playback_token = token::sign(
        &token::PlaybackClaims {
            prefix: prefix.clone(),
            exp: Utc::now().timestamp() + st.cfg.token_ttl_secs,
        },
        &st.cfg.token_secret,
    );

    sqlx::query(
        "INSERT INTO audit_events (practitioner, client_app, study_uid, action, detail)
         VALUES ($1,$2,$3,'view','fhir')",
    )
    .bind(&id.practitioner).bind(&id.client_id).bind(&study_uid)
    .execute(&st.db)
    .await?;

    let series: Vec<Value> = renditions
        .iter()
        .enumerate()
        .map(|(i, r)| {
            json!({
                "uid": r.get::<String, _>("series_uid"),
                "number": i + 1,
                "modality": { "system": DCM, "code": r.get::<String, _>("modality") },
                "description": r.get::<String, _>("series_description"),
                "numberOfInstances": r.get::<i32, _>("frames"),
                "endpoint": [{ "reference": "#hls" }],
            })
        })
        .collect();

    Ok(fhir_response(json!({
        "resourceType": "ImagingStudy",
        "id": study_uid,
        "contained": [{
            "resourceType": "Endpoint",
            "id": "hls",
            "status": "active",
            "connectionType": { "system": CONNECTION_TYPE_SYSTEM, "code": "hls" },
            "name": "OpenMedVideo HLS manifest (token expires; re-read to refresh)",
            "payloadType": [{ "coding": [{ "system": CONNECTION_TYPE_SYSTEM,
                                            "code": "omv-manifest" }] }],
            "payloadMimeType": ["application/json", "application/vnd.apple.mpegurl"],
            "address": format!("/stream/{playback_token}/{prefix}/manifest.json"),
        }],
        "identifier": [{ "system": "urn:dicom:uid",
                         "value": format!("urn:oid:{study_uid}") }],
        "status": "available",
        "modality": modality_codings(&study.get::<String, _>("modalities")),
        "description": study.get::<String, _>("description"),
        "started": study.get::<chrono::DateTime<Utc>, _>("created_at").to_rfc3339(),
        "numberOfSeries": series.len(),
        "series": series,
        "endpoint": [{ "reference": "#hls" }],
    })))
}
