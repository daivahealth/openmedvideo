# OpenMedVideo Deployment Guide

How to stand up, harden, operate, and upgrade an OpenMedVideo deployment. Day-to-day usage (API, playback, integration) is in the [User Manual](USER_MANUAL.md); architecture and rationale in [DESIGN.md](DESIGN.md).

The supported topology today is the **Phase 1/2 single box**: everything runs as one Docker Compose stack on one host. Scale-out (multi-hospital ingest, regional edge caches/CDN) is Phase 3 and not covered here.

---

## 1. What gets deployed

| Service | Image | Role | Persistent data |
|---|---|---|---|
| `orthanc` | `orthancteam/orthanc` | DICOM ingest (C-STORE SCP, AET `OMV`); fires the stable-study hook | `orthanc-db` volume |
| `redis` | `redis:7-alpine` | Job queue (Redis Streams) + dead-letter stream `omv:dead` | none (jobs are re-drivable) |
| `postgres` | `postgres:16-alpine` | Catalog, client registry, **append-only audit trail** | `pg-data` volume |
| `minio` | `minio/minio` | Object storage for HLS/MP4/posters (swappable, §5) | `minio-data` volume |
| `minio-init` | `minio/mc` | One-shot: creates the `medvideo` bucket | — |
| `api` | built from `deploy/Dockerfile` (target `api`) | OAuth, catalog, FHIR, playback tokens, HLS serving, players, audit | — (stateless) |
| `worker` | built from `deploy/Dockerfile` (target `worker`) | DICOM → ffmpeg → HLS conversion, PHI stripping, retries | — (stateless) |
| `prometheus` | `prom/prometheus` | Scrapes api + worker metrics (`deploy/prometheus.yml`) | none (dev stack; point your own Prometheus at the targets in production) |
| `nginx` | `nginx:alpine` | The only exposed edge: proxying, HLS segment cache, (production) TLS | cache only |

Traffic flow: modalities send DICOM to Orthanc on **4242**; client apps talk HTTPS to **nginx only**. Orthanc's Lua hook (`deploy/orthanc/omv-hook.lua`) POSTs the idempotent `POST /internal/orthanc-event` to the API when a study becomes stable (`StableAge: 30` in `deploy/orthanc/orthanc.json`); the worker consumes the Redis job and writes video objects to storage.

---

## 2. Prerequisites

- Docker Engine with Compose v2 on a Linux host (the stack is also fine on macOS for development).
- Sizing: conversion is CPU-bound under software x264. A modest multi-core box handles pilot volume; for production volume plan a **single mid-range NVIDIA GPU** — NVENC gives 10–20× encode throughput (a 400-slice CT in ~1 s per rendition).
- For GPU encoding: NVIDIA driver + `nvidia-container-toolkit` on the host.
- Network access from modality/PACS VLANs to port 4242, and from client-app networks to the nginx edge only.

---

## 3. Deploying the stack

### 3.1 CPU (default)

```bash
docker compose -f deploy/docker-compose.yml up --build -d
```

The compose file builds both Rust binaries from source in a multi-stage image (`deploy/Dockerfile`); the worker image additionally carries ffmpeg (libx264 included).

### 3.2 GPU (NVENC)

```bash
docker compose -f deploy/docker-compose.yml -f deploy/docker-compose.gpu.yml up --build -d
```

The overlay sets `OMV_ENCODER=nvenc` and reserves one GPU with `video` capabilities. `nvenc` makes the worker **fail fast** at startup (it smoke-tests `h264_nvenc` with a real encode) rather than silently encoding 10–20× slower on CPU. On a mixed fleet where graceful degradation is preferred, use `OMV_ENCODER=auto` instead.

### 3.3 Ports

| Port | Service | Expose to |
|---|---|---|
| 8000 (dev) / 443 (prod) | nginx | Client-app networks. **The only public port.** |
| 4242 | Orthanc DICOM | Modality/PACS VLANs only |
| 8042 | Orthanc UI/REST | **Dev only** — never on production networks |
| 9001 | MinIO console | **Dev only** |
| 9090 | Prometheus UI | **Dev only** |
| 9464 | api/worker `/metrics` (`OMV_METRICS_ADDR`) | Internal only — deliberately not proxied by nginx, so metrics never reach the public edge |

---

## 4. Production hardening checklist

The shipped compose file is a **dev stack**. Before real patient data touches it:

**Secrets — all dev defaults must change:**
- [ ] `OMV_TOKEN_SECRET` — signs every playback token. The dev value literally says `dev-only-change-me-in-production`. Use a long random value; rotating it invalidates outstanding playback tokens (clients just re-fetch the study).
- [ ] Postgres credentials (`POSTGRES_PASSWORD` + `OMV_DATABASE_URL`).
- [ ] MinIO root credentials (`MINIO_ROOT_*` + `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`).
- [ ] Orthanc `RegisteredUsers` in `deploy/orthanc/orthanc.json` (+ `OMV_ORTHANC_USER`/`_PASSWORD`).
- [ ] Remove `OMV_SEED_DEV_CLIENT=1` (it seeds the `aadi-dev` client against a fake HS256 IdP) and `OMV_CLIENT_TOKENS` (static dev bearer fallback). Register real clients instead (§6).

**Network posture (design §7.4):**
- [ ] TLS 1.2+ terminates at nginx; expose only 443. Remove the 8042 and 9001 port mappings from the compose file.
- [ ] Orthanc's DICOM port reachable only from modality/PACS VLANs (host firewall / VLAN ACLs).
- [ ] All services on the hospital network / private VPC; nothing but nginx faces clients.

**Pipeline settings:**
- [ ] `OMV_RETRY_IDLE_SECS`: the dev compose sets 15 for fast iteration; production default is **60**. `OMV_MAX_ATTEMPTS`: dev sets 3; default 4.
- [ ] PHI rules: mount a reviewed `phi-rules.json` (`OMV_PHI_RULES`) and decide the unmatched-burned-in policy — default converts with a loud warning; `OMV_PHI_UNMATCHED_BURNEDIN=skip` refuses such series. See User Manual §8.
- [ ] Decide `OMV_EXPORT_ENABLED` (MP4 export kill switch) per deployment policy.

**Monitoring:** metrics for all the design's SLIs are built in (§10) — wire the two `:9464` targets into your production Prometheus and alert on them. **Still open (design §9):** measuring the 60 s SLO on production hardware and a regression corpus from real-world DICOM. Plan these before go-live.

---

## 5. Storage backends

All storage access goes through the Rust `object_store` abstraction — the provider is the `OMV_STORAGE_URL` scheme, not a code change:

| Backend | `OMV_STORAGE_URL` | Extra env |
|---|---|---|
| MinIO (default, on-prem) | `s3://medvideo` | `AWS_ENDPOINT_URL`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION` |
| AWS S3 | `s3://bucket` | standard AWS credentials |
| Azure Blob | `az://container` | Azure credential env vars |
| GCS | `gs://bucket` | GCP credential env vars |
| Local filesystem | `file:///path` | — |

Playback is provider-neutral by design: tokens are OMV's own HMAC prefix tokens, not storage-provider presigned URLs, so streaming behaves identically on-prem or on any cloud. Enable encryption at rest on the chosen backend (MinIO SSE on-prem).

---

## 6. Registering production clients

Onboarding an app is a row in the `clients` table, not a release: client id + secret, allowed scopes (`imaging.read`, optionally `imaging.export`), the app's trusted IdP (issuer + JWKS URL for RS256 validation), and optionally a `webhook_url`/`webhook_secret` for signed `study.ready`/`study.failed` events. The API validates exchanged IdP tokens against the registered JWKS — it never trusts a client-asserted identity without a registered IdP behind it.

---

## 7. Data, backup, and retention

- **PostgreSQL (`pg-data`) is the only data that must be backed up** — it holds the catalog, the client registry, and the append-only audit trail (the compliance record). Standard `pg_dump`/WAL archiving applies.
- **Videos are a regenerable cache.** Losing `minio-data` loses no source data: the DICOM stays in PACS, and any study can be reconverted by re-POSTing its Orthanc event. Aggressive lifecycle expiry is safe by design.
- **Orthanc (`orthanc-db`)** holds received DICOM; treat retention per your PACS-forwarding policy (if PACS keeps the source, Orthanc storage is also regenerable).
- **Redis** needs no persistence guarantees: pending jobs can be re-driven, and `omv:dead` entries are recoverable by re-driving the study.

---

## 8. Upgrades

1. Pull the new code, then rebuild and recreate:
   ```bash
   docker compose -f deploy/docker-compose.yml up --build -d
   ```
2. Both Rust services are stateless; the worker finishes or abandons its current job (unacked jobs are reclaimed by the retry pass, so an upgrade mid-conversion self-heals).
3. nginx re-resolves the `api` upstream via Docker's DNS every 10 s (`resolver 127.0.0.11 valid=10s` in `deploy/nginx/nginx.conf`), so recreating the api container does **not** require an nginx restart. If you change the nginx config itself, recreate the nginx container.
4. Schema: the API applies its schema at startup; no manual migration step in the current phase.

---

## 9. Post-deploy smoke test

1. `curl http://<edge>/healthz` → `ok`.
2. Send a test study (`scripts/make_test_study.py`, then `storescu -aec OMV <host> 4242 *.dcm`).
3. Watch worker logs for the conversion; within ~60 s of the last instance the study should be `ready`:
   ```bash
   curl -H "Authorization: Bearer $ACCESS_TOKEN" http://<edge>/v1/studies
   ```
   (Token flow: User Manual §3.)
4. Open the study's `player_url` in a browser — scrub bar, frame stepping, and preset tabs should work.
5. Verify a tampered playback token is rejected (edit any character in the `/stream/...` path → 4xx) and that the view appeared in the audit table.

---

## 10. Operations

- **Health:** `GET /healthz` on the API (through nginx).
- **Queue and retries:** transient conversion failures retry automatically (study shows `status=retrying`); after `OMV_MAX_ATTEMPTS` the job lands on the `omv:dead` Redis stream and the study goes to `failed`. Inspect: `redis-cli XRANGE omv:dead - +`. Re-drive after fixing the cause:
  ```bash
  curl -X POST http://api:8080/internal/orthanc-event -d '{"study_id": "<orthanc id>"}'
  ```
  (from inside the network — `/internal/*` must never be exposed through the edge).
- **Cache:** nginx caches `/stream/` responses for 5 min (`X-Cache-Status` header shows HIT/MISS); segments are immutable once written, so the cache never serves stale media.
- **Logs:** all services log to stdout (`RUST_LOG=info` by default); use your host's log driver/shipper.
- **Metrics:** both services expose Prometheus metrics at `/metrics` on internal port 9464 (`OMV_METRICS_ADDR`); the dev stack runs Prometheus at `http://localhost:9090` scraping both. The key queries per SLI (also documented in `deploy/prometheus.yml`):

  | SLI | Query |
  |---|---|
  | Queue depth | `omv_queue_depth`, `omv_queue_pending` |
  | Conversion p95 | `histogram_quantile(0.95, rate(omv_conversion_seconds_bucket[5m]))` |
  | 60 s SLO compliance (enqueue→outcome, incl. queue wait and retries) | `sum(rate(omv_job_total_seconds_bucket{le="60"}[1h])) / sum(rate(omv_job_total_seconds_count[1h]))` |
  | Playback error rate | `1 - rate(omv_playback_requests_total{outcome="ok"}[5m]) / rate(omv_playback_requests_total[5m])` |
  | Dead-letter arrivals | `increase(omv_conversions_total{outcome="dead_letter"}[1h])` |

  Also available: `omv_webhook_deliveries_total{delivered|gave_up}` and per-route `omv_http_requests_total` / `omv_http_request_seconds` (labeled by matched route template, so study UIDs never explode cardinality). Alert at minimum on dead-letter arrivals, sustained queue depth, and playback error rate.
