//! OAuth 2.0 token endpoint (design §5.3 point 2).
//!
//! Grants:
//!   - `urn:ietf:params:oauth:grant-type:token-exchange` (RFC 8693): a
//!     registered client app exchanges its end-user's identity token (from
//!     the IdP registered for that client) for a short-lived OMV access
//!     token carrying the practitioner identity and client id.
//!   - `client_credentials`: pure server-to-server integrations.
//!
//! The platform never handles passwords and never trusts a client-asserted
//! identity without a registered IdP behind it. Access tokens are HS256 JWTs
//! signed with OMV_TOKEN_SECRET; the catalog endpoints validate them locally.

use anyhow::{anyhow, bail, Context, Result};
use axum::{
    extract::{Form, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use chrono::Utc;
use jsonwebtoken::{jwk::JwkSet, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{info, warn};

pub const GRANT_TOKEN_EXCHANGE: &str = "urn:ietf:params:oauth:grant-type:token-exchange";
pub const ISSUER: &str = "openmedvideo";

/// Claims inside an OMV access token.
#[derive(Debug, Serialize, Deserialize)]
pub struct AccessClaims {
    pub iss: String,
    /// Practitioner identity (or the client id itself for client_credentials).
    pub sub: String,
    /// The registered client app this token was issued to.
    pub cid: String,
    /// Space-separated granted scopes.
    pub scope: String,
    pub exp: i64,
}

/// Identity established for a request: who, via which app, with what scopes.
#[derive(Debug, Clone)]
pub struct Identity {
    pub practitioner: String,
    pub client_id: String,
    pub scopes: Vec<String>,
}

impl Identity {
    pub fn has_scope(&self, s: &str) -> bool {
        self.scopes.iter().any(|x| x == s)
    }
}

/// Cache of fetched IdP JWKS documents (kid lookup for RS256 validation).
#[derive(Clone, Default)]
pub struct JwksCache {
    inner: Arc<RwLock<HashMap<String, (Instant, JwkSet)>>>,
}

const JWKS_TTL: Duration = Duration::from_secs(600);

impl JwksCache {
    async fn get(&self, url: &str) -> Result<JwkSet> {
        if let Some((at, set)) = self.inner.read().await.get(url) {
            if at.elapsed() < JWKS_TTL {
                return Ok(set.clone());
            }
        }
        let set: JwkSet = reqwest::get(url)
            .await
            .with_context(|| format!("fetching JWKS from {url}"))?
            .error_for_status()?
            .json()
            .await?;
        self.inner.write().await.insert(url.into(), (Instant::now(), set.clone()));
        Ok(set)
    }
}

struct ClientRow {
    client_id: String,
    secret_hash: String,
    name: String,
    scopes: String,
    idp_issuer: Option<String>,
    idp_audience: Option<String>,
    idp_jwks_url: Option<String>,
    idp_hs256_secret: Option<String>,
}

fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// Mint an OMV access token.
pub fn mint(secret: &str, sub: &str, cid: &str, scope: &str, ttl_secs: i64) -> String {
    let claims = AccessClaims {
        iss: ISSUER.into(),
        sub: sub.into(),
        cid: cid.into(),
        scope: scope.into(),
        exp: Utc::now().timestamp() + ttl_secs,
    };
    jsonwebtoken::encode(&Header::default(), &claims, &EncodingKey::from_secret(secret.as_bytes()))
        .expect("HS256 encode")
}

/// Validate an OMV access token; returns the request identity.
pub fn validate(secret: &str, token: &str) -> Result<Identity> {
    let mut v = Validation::new(Algorithm::HS256);
    v.set_issuer(&[ISSUER]);
    let data = jsonwebtoken::decode::<AccessClaims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &v,
    )?;
    Ok(Identity {
        practitioner: data.claims.sub,
        client_id: data.claims.cid,
        scopes: data.claims.scope.split_whitespace().map(String::from).collect(),
    })
}

// -------------------------------------------------------- token endpoint --

fn oauth_error(status: StatusCode, code: &str, desc: &str) -> Response {
    (status, Json(json!({ "error": code, "error_description": desc }))).into_response()
}

/// Extracts client credentials from HTTP Basic auth or the form body.
fn client_credentials(
    headers: &HeaderMap,
    form: &HashMap<String, String>,
) -> Option<(String, String)> {
    if let Some(v) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        if let Some(b64) = v.strip_prefix("Basic ") {
            if let Ok(raw) = STANDARD.decode(b64) {
                if let Ok(s) = String::from_utf8(raw) {
                    if let Some((id, secret)) = s.split_once(':') {
                        return Some((id.into(), secret.into()));
                    }
                }
            }
        }
    }
    match (form.get("client_id"), form.get("client_secret")) {
        (Some(i), Some(s)) => Some((i.clone(), s.clone())),
        _ => None,
    }
}

pub async fn token_endpoint(
    State(st): State<crate::AppState>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let Some((client_id, client_secret)) = client_credentials(&headers, &form) else {
        return oauth_error(StatusCode::UNAUTHORIZED, "invalid_client", "missing client credentials");
    };

    let client = match load_client(&st, &client_id).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return oauth_error(StatusCode::UNAUTHORIZED, "invalid_client", "unknown client")
        }
        Err(e) => {
            warn!(error = %e, "client lookup failed");
            return oauth_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error", "try again");
        }
    };
    if sha256_hex(&client_secret) != client.secret_hash {
        return oauth_error(StatusCode::UNAUTHORIZED, "invalid_client", "bad client secret");
    }

    // Requested scopes must be a subset of the client's registration.
    let registered: Vec<&str> = client.scopes.split_whitespace().collect();
    let scope = match form.get("scope") {
        Some(req) => {
            let asked: Vec<&str> = req.split_whitespace().collect();
            if asked.iter().any(|s| !registered.contains(s)) {
                return oauth_error(StatusCode::BAD_REQUEST, "invalid_scope",
                    "requested scope exceeds client registration");
            }
            asked.join(" ")
        }
        None => registered.join(" "),
    };

    let grant = form.get("grant_type").map(String::as_str).unwrap_or("");
    let (sub, granted) = match grant {
        GRANT_TOKEN_EXCHANGE => {
            let Some(subject_token) = form.get("subject_token") else {
                return oauth_error(StatusCode::BAD_REQUEST, "invalid_request",
                    "subject_token is required");
            };
            match validate_subject_token(&st.jwks, &client, subject_token).await {
                Ok(practitioner) => (practitioner, scope),
                Err(e) => {
                    warn!(client = %client.client_id, error = %e, "subject token rejected");
                    return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant",
                        "subject token was rejected");
                }
            }
        }
        "client_credentials" => (format!("service:{}", client.client_id), scope),
        _ => {
            return oauth_error(StatusCode::BAD_REQUEST, "unsupported_grant_type",
                "use token-exchange (RFC 8693) or client_credentials")
        }
    };

    let ttl = st.cfg.access_token_ttl_secs;
    let token = mint(&st.cfg.token_secret, &sub, &client.client_id, &granted, ttl);
    info!(client = %client.client_id, practitioner = %sub, name = %client.name, "token issued");
    Json(json!({
        "access_token": token,
        "issued_token_type": "urn:ietf:params:oauth:token-type:access_token",
        "token_type": "Bearer",
        "expires_in": ttl,
        "scope": granted,
    }))
    .into_response()
}

async fn load_client(st: &crate::AppState, client_id: &str) -> Result<Option<ClientRow>> {
    let row = sqlx::query(
        "SELECT client_id, client_secret_hash, name, scopes,
                idp_issuer, idp_audience, idp_jwks_url, idp_hs256_secret
         FROM clients WHERE client_id = $1",
    )
    .bind(client_id)
    .fetch_optional(&st.db)
    .await?;
    Ok(row.map(|r| ClientRow {
        client_id: r.get("client_id"),
        secret_hash: r.get("client_secret_hash"),
        name: r.get("name"),
        scopes: r.get("scopes"),
        idp_issuer: r.get("idp_issuer"),
        idp_audience: r.get("idp_audience"),
        idp_jwks_url: r.get("idp_jwks_url"),
        idp_hs256_secret: r.get("idp_hs256_secret"),
    }))
}

/// Validates the end-user's identity token against the IdP registered for
/// this client, returning the practitioner identity.
async fn validate_subject_token(
    jwks: &JwksCache,
    client: &ClientRow,
    token: &str,
) -> Result<String> {
    let issuer = client
        .idp_issuer
        .as_deref()
        .ok_or_else(|| anyhow!("client has no registered IdP"))?;

    let header = jsonwebtoken::decode_header(token)?;
    let mut v = Validation::new(header.alg);
    v.set_issuer(&[issuer]);
    match &client.idp_audience {
        Some(aud) => v.set_audience(&[aud]),
        None => v.validate_aud = false,
    }

    let key = if let Some(secret) = &client.idp_hs256_secret {
        if header.alg != Algorithm::HS256 {
            bail!("client IdP is registered for HS256, token uses {:?}", header.alg);
        }
        DecodingKey::from_secret(secret.as_bytes())
    } else if let Some(url) = &client.idp_jwks_url {
        let set = jwks.get(url).await?;
        let kid = header.kid.ok_or_else(|| anyhow!("subject token has no kid"))?;
        let jwk = set
            .find(&kid)
            .ok_or_else(|| anyhow!("kid {kid} not found in IdP JWKS"))?;
        DecodingKey::from_jwk(jwk)?
    } else {
        bail!("client has neither a JWKS URL nor an HS256 secret registered");
    };

    let data = jsonwebtoken::decode::<Value>(token, &key, &v)?;
    let c = &data.claims;
    let who = c
        .get("preferred_username")
        .or_else(|| c.get("email"))
        .or_else(|| c.get("sub"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("subject token has no usable identity claim"))?;
    Ok(who.to_string())
}

/// Dev-only seed so the compose stack works out of the box. Controlled by
/// OMV_SEED_DEV_CLIENT=1; never enable in production.
pub async fn seed_dev_client(db: &sqlx::PgPool) -> Result<()> {
    sqlx::query(
        "INSERT INTO clients (client_id, client_secret_hash, name, scopes,
                              idp_issuer, idp_audience, idp_hs256_secret)
         VALUES ($1, $2, 'AADI (dev)', 'imaging.read imaging.export',
                 'https://idp.dev', 'omv', $3)
         ON CONFLICT (client_id) DO UPDATE SET client_secret_hash = $2",
    )
    .bind("aadi-dev")
    .bind(sha256_hex("aadi-dev-secret"))
    .bind("dev-idp-secret")
    .execute(db)
    .await?;
    info!("seeded dev client 'aadi-dev' (OMV_SEED_DEV_CLIENT=1)");
    Ok(())
}
