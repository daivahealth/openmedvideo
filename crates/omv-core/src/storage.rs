use anyhow::{bail, Context, Result};
use bytes::Bytes;
use object_store::{aws::AmazonS3Builder, local::LocalFileSystem, path::Path, ObjectStore};
use std::sync::Arc;

/// Provider-neutral object storage handle (design decision D8).
///
/// The backend is chosen by the `OMV_STORAGE_URL` scheme:
///   - `s3://bucket`   — AWS S3, MinIO, or GCS interoperability mode; endpoint
///     and credentials come from the standard AWS_* env vars
///     (AWS_ENDPOINT_URL, AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, ...).
///   - `file:///path`  — local filesystem, for development and tests.
///
/// Azure Blob (`az://`) and native GCS (`gs://`) plug in the same way via the
/// corresponding `object_store` builders when the deployment needs them.
#[derive(Clone)]
pub struct Storage {
    store: Arc<dyn ObjectStore>,
}

impl Storage {
    pub fn from_url(url: &str) -> Result<Self> {
        let store: Arc<dyn ObjectStore> = if let Some(bucket) = url.strip_prefix("s3://") {
            let bucket = bucket.trim_end_matches('/');
            let s3 = AmazonS3Builder::from_env()
                .with_bucket_name(bucket)
                .with_allow_http(true) // MinIO inside the compose network is plain HTTP
                .build()
                .context("building S3 store")?;
            Arc::new(s3)
        } else if let Some(dir) = url.strip_prefix("file://") {
            std::fs::create_dir_all(dir).ok();
            Arc::new(LocalFileSystem::new_with_prefix(dir).context("building local store")?)
        } else {
            bail!("unsupported OMV_STORAGE_URL scheme: {url}");
        };
        Ok(Self { store })
    }

    pub async fn put(&self, key: &str, data: Vec<u8>) -> Result<()> {
        self.store
            .put(&Path::from(key), data.into())
            .await
            .with_context(|| format!("uploading {key}"))?;
        Ok(())
    }

    pub async fn get(&self, key: &str) -> Result<Bytes> {
        let res = self
            .store
            .get(&Path::from(key))
            .await
            .with_context(|| format!("fetching {key}"))?;
        Ok(res.bytes().await?)
    }
}

/// Content type for a stored HLS/media object, by file extension.
pub fn content_type_for(key: &str) -> &'static str {
    match key.rsplit('.').next() {
        Some("m3u8") => "application/vnd.apple.mpegurl",
        Some("m4s") => "video/iso.segment",
        Some("mp4") => "video/mp4",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    }
}
