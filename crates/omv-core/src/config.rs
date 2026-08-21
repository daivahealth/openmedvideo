use anyhow::{Context, Result};

/// Runtime configuration, sourced entirely from environment variables so the
/// same binaries run unchanged on-prem, in CI, or on any cloud.
#[derive(Clone, Debug)]
pub struct Config {
    /// Postgres connection string.
    pub database_url: String,
    /// Redis connection string (job queue).
    pub redis_url: String,
    /// Orthanc REST base URL, e.g. http://orthanc:8042
    pub orthanc_url: String,
    pub orthanc_user: String,
    pub orthanc_password: String,
    /// Object storage: s3://bucket (MinIO/AWS/GCS-interop) or file:///path for dev.
    pub storage_url: String,
    /// HMAC secret for playback tokens. Must be long and random in production.
    pub token_secret: String,
    /// Playback token lifetime in seconds.
    pub token_ttl_secs: i64,
    /// OAuth access-token lifetime in seconds.
    pub access_token_ttl_secs: i64,
    /// Comma-separated static bearer tokens for client apps (Phase 1 auth;
    /// replaced by OAuth2/OIDC token exchange in Phase 2).
    pub client_tokens: Vec<String>,
    /// API bind address.
    pub bind_addr: String,
    /// Whether the export-MP4 download path is available at all in this
    /// deployment (per-client control is the imaging.export scope).
    pub export_enabled: bool,
}

fn var(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("missing required env var {name}"))
}

fn var_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            database_url: var("OMV_DATABASE_URL")?,
            redis_url: var_or("OMV_REDIS_URL", "redis://127.0.0.1:6379"),
            orthanc_url: var_or("OMV_ORTHANC_URL", "http://127.0.0.1:8042"),
            orthanc_user: var_or("OMV_ORTHANC_USER", "omv"),
            orthanc_password: var_or("OMV_ORTHANC_PASSWORD", "omv"),
            storage_url: var("OMV_STORAGE_URL")?,
            token_secret: var("OMV_TOKEN_SECRET")?,
            token_ttl_secs: var_or("OMV_TOKEN_TTL_SECS", "300").parse()?,
            access_token_ttl_secs: var_or("OMV_ACCESS_TOKEN_TTL_SECS", "900").parse()?,
            client_tokens: var_or("OMV_CLIENT_TOKENS", "")
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect(),
            bind_addr: var_or("OMV_BIND_ADDR", "0.0.0.0:8080"),
            export_enabled: var_or("OMV_EXPORT_ENABLED", "1") == "1",
        })
    }
}

/// Name of the Redis stream carrying conversion jobs.
pub const JOB_STREAM: &str = "omv:jobs";
/// Consumer group used by conversion workers.
pub const JOB_GROUP: &str = "omv-workers";
