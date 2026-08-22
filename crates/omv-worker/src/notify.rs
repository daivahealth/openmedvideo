//! Signed study-lifecycle webhooks (design §5.3 point 5).
//!
//! Every registered client with a webhook_url receives a JSON POST for
//! `study.ready` and `study.failed`, signed with that client's secret:
//!
//!   X-OMV-Event:     study.ready
//!   X-OMV-Signature: sha256=<hex hmac-sha256 of the raw body>
//!
//! Receivers must verify the signature against the raw bytes before parsing.
//! Delivery is best-effort with bounded retries; a failed endpoint is logged
//! and never blocks conversion (a durable outbox is a later hardening step).

use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use sqlx::{PgPool, Row};
use std::time::Duration;
use tracing::{info, warn};

const ATTEMPTS: u32 = 3;

pub struct Notifier {
    http: reqwest::Client,
}

impl Default for Notifier {
    fn default() -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("reqwest client"),
        }
    }
}

pub fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

impl Notifier {
    /// Sends `event` with `payload` to every client that registered a
    /// webhook. Payload fields are merged with {event, at}.
    pub async fn broadcast(&self, db: &PgPool, event: &str, payload: Value) {
        let hooks = match sqlx::query(
            "SELECT client_id, webhook_url, COALESCE(webhook_secret,'') AS secret
             FROM clients WHERE webhook_url IS NOT NULL",
        )
        .fetch_all(db)
        .await
        {
            Ok(rows) => rows,
            Err(e) => {
                warn!(error = %e, "webhook client lookup failed");
                return;
            }
        };

        let mut body = payload;
        body["event"] = json!(event);
        body["at"] = json!(chrono::Utc::now().to_rfc3339());
        let bytes = serde_json::to_vec(&body).expect("serialize webhook body");

        for row in hooks {
            let client_id: String = row.get("client_id");
            let url: String = row.get("webhook_url");
            let secret: String = row.get("secret");
            self.deliver(&client_id, &url, &secret, event, bytes.clone()).await;
        }
    }

    async fn deliver(&self, client_id: &str, url: &str, secret: &str, event: &str, body: Vec<u8>) {
        let signature = sign(secret, &body);
        for attempt in 1..=ATTEMPTS {
            let res = self
                .http
                .post(url)
                .header("content-type", "application/json")
                .header("x-omv-event", event)
                .header("x-omv-signature", &signature)
                .body(body.clone())
                .send()
                .await;
            match res {
                Ok(r) if r.status().is_success() => {
                    info!(client = client_id, event, "webhook delivered");
                    return;
                }
                Ok(r) => warn!(client = client_id, status = %r.status(), attempt, "webhook rejected"),
                Err(e) => warn!(client = client_id, error = %e, attempt, "webhook send failed"),
            }
            tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
        }
        warn!(client = client_id, event, "webhook delivery gave up after {ATTEMPTS} attempts");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_is_stable_and_secret_bound() {
        let body = br#"{"event":"study.ready"}"#;
        let a = sign("s1", body);
        assert!(a.starts_with("sha256="));
        assert_eq!(a, sign("s1", body), "deterministic");
        assert_ne!(a, sign("s2", body), "bound to the secret");
        assert_ne!(a, sign("s1", b"other"), "bound to the body");
    }
}
