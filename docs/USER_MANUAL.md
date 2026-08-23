# OpenMedVideo User Manual

OpenMedVideo converts DICOM studies (CT, MRI, ultrasound, angiography) into standard streaming video and serves them to clinical apps over HLS, so a clinician can review imaging on any phone — play, pause, scrub, frame-step — without a DICOM viewer.

> **Not a diagnostic viewer.** Video output is 8-bit with a baked-in window/level. Every output is for clinical review and communication; the diagnostic read happens on the DICOM in PACS. Videos are a regenerable cache — PACS remains the source of truth.

This manual covers day-to-day use of a running deployment. For architecture and rationale, see [DESIGN.md](DESIGN.md).

**Who this is for**

| You are… | Read |
|---|---|
| An operator standing up or running the stack | §1, §2, §8–§10 |
| A client-app developer integrating imaging into your app | §3–§7 |
| A clinician-facing team evaluating the player | §5 |

---

## 1. Starting the system

The Phase 1/2 stack runs as a single Docker Compose deployment: Orthanc (DICOM ingest), Redis (job queue), PostgreSQL (catalog + audit), MinIO (video storage), the API, the conversion worker, and nginx in front.

```bash
docker compose -f deploy/docker-compose.yml up --build
```

On a host with an NVIDIA GPU, add the NVENC overlay for 10–20× faster encoding:

```bash
docker compose -f deploy/docker-compose.yml -f deploy/docker-compose.gpu.yml up --build
```

The worker smoke-tests the GPU encoder at startup: with the default `OMV_ENCODER=auto` it falls back to software x264 if no usable GPU is found; `nvenc` fails fast on a misconfigured GPU host; `x264` forces software.

### Ports (dev stack)

| Port | Service | Notes |
|---|---|---|
| **8000** | nginx → API | The only port client apps talk to. All examples below use `http://localhost:8000`. |
| 4242 | Orthanc DICOM (C-STORE) | Reachable from modality/PACS networks only |
| 8042 | Orthanc web UI / REST | Dev only (`omv`/`omv`); keep off public networks |
| 9001 | MinIO console | Dev only |

In production only the nginx edge (TLS 1.2+) is exposed; everything else stays on the hospital network / private VPC.

---

## 2. Sending studies in

Point your modality or PACS auto-forward at the Orthanc endpoint: **AET `OMV`, port `4242`**. For testing:

```bash
storescu -aec OMV localhost 4242 *.dcm
```

or upload through the Orthanc UI at `http://localhost:8042` (dev credentials `omv`/`omv`). `scripts/make_test_study.py` can generate a synthetic study if you have no DICOM at hand.

**Conversion is automatic.** Orthanc waits until the study is *stable* (no new instances for ~30 s, so conversion never starts on a partial series), then fires a webhook that enqueues a conversion job. The worker fetches rendered frames, applies PHI stripping (§8), encodes HLS renditions, and registers the study in the catalog. Typical end-to-end time for a small study is under a minute, most of it the 30 s stable window.

What gets produced per modality:

| Modality | Output |
|---|---|
| CT | One video per clinical window preset, chosen from `BodyPartExamined` — chest → lung/mediastinal/bone; head → brain/subdural/bone; abdomen/pelvis → soft/bone; spine/neck → soft/bone; unknown → soft/lung/bone. All-intra encoding for frame-accurate scrubbing. |
| MRI | One video per series, auto-windowed |
| US / XA | One video per cine loop at the native frame rate from the DICOM timing tags |
| CR / DX | Skipped in Phase 1 (stills, not video) |

---

## 3. Authenticating

All catalog, FHIR, and export calls need an OMV access token as a `Bearer` header. Two grants are supported at `POST /oauth/token`:

**Token exchange (RFC 8693)** — the normal flow for user-facing apps. Your app authenticates its user against your own identity provider, then swaps that IdP token for an OMV token carrying the practitioner identity:

```bash
IDP_TOKEN=$(python3 scripts/idp_token.py dr.asha)   # dev stand-in for your real IdP
ACCESS_TOKEN=$(curl -s -u aadi-dev:aadi-dev-secret http://localhost:8000/oauth/token \
  -d grant_type=urn:ietf:params:oauth:grant-type:token-exchange \
  -d subject_token=$IDP_TOKEN | jq -r .access_token)
```

**Client credentials** — for server-to-server integrations with no end user: `grant_type=client_credentials` with the same HTTP Basic client authentication.

Notes:

- Each app is a registered OAuth client with its own credentials, allowed scopes, and trusted IdP (see §9). The dev stack seeds `aadi-dev` / `aadi-dev-secret` against a fake HS256 IdP; production clients register a real RS256 IdP validated via JWKS.
- Scopes: `imaging.read` (catalog, playback, FHIR) and `imaging.export` (MP4 download).
- Access tokens expire after ~15 minutes (`OMV_ACCESS_TOKEN_TTL_SECS`); just request a new one.
- The static `Bearer dev-client-token` from `OMV_CLIENT_TOKENS` still works as a **deprecated dev-only fallback**.

---

## 4. Browsing the catalog

**List studies** — ready and in-flight, newest first:

```bash
curl -H "Authorization: Bearer $ACCESS_TOKEN" http://localhost:8000/v1/studies
```

Each entry carries `study_uid`, `description`, `modalities`, `status`, and `created_at`. Status moves `pending → converting → ready`. A transient conversion failure shows `retrying` (with the error) while the job is automatically retried; only after all attempts are exhausted does the study go to `failed` (see §10.3 for re-driving it).

**Get one study** — the playback entry point:

```bash
curl -H "Authorization: Bearer $ACCESS_TOKEN" \
     http://localhost:8000/v1/studies/<StudyInstanceUID>
```

A `ready` study's response includes:

- `player_url` — the embeddable web player page, one tokenized URL (§5).
- `poster_url` — a JPEG thumbnail for study lists.
- Per rendition (series × window preset): `playlist_url` — a plain HLS master playlist any player can stream — and `export_url` when export is enabled and your token has `imaging.export` (§6).

All of these embed a **playback token**: short-lived (~5 min, `OMV_TOKEN_TTL_SECS`), HMAC-signed, and scoped to that study's storage prefix. One token covers every playlist and segment of the study, so hand the URL straight to a video player. When it expires, re-fetch the study to get fresh URLs — don't cache the URLs beyond a viewing session.

---

## 5. Watching a study

### 5.1 The web player (zero integration)

Open `player_url` in any browser, WebView, or iframe. It provides the clinical UI out of the box:

- Slice-aware scrub bar showing the slice counter (`21/40`)
- ±1 frame stepping via buttons or the ←/→ keys
- Window-preset tabs for CT (e.g. Soft / Lung / Bone) that preserve the playback position when switching
- Series switcher and loop toggle

The player is fully self-contained — hls.js is served from the OMV server itself, so it works on offline hospital networks with no CDN.

### 5.2 Embedding `<omv-player>` in your own app

The same UI ships as a framework-agnostic Web Component for apps that want the player inside their own DOM (Angular, React, Vue, plain HTML, WebViews):

```html
<script src="https://your-omv-server/player-assets/omv-player.js"></script>
<omv-player server="https://your-omv-server"
            study-id="1.2.840..."
            token="<playback token from GET /v1/studies/{uid}>">
</omv-player>
```

- Size it from CSS (it fills its host); theme via `--omv-accent`, `--omv-bg`, … custom properties.
- Events: `omv-ready`, `omv-error`, `omv-frame` (`{frame, frames}`). Methods: `el.step(±1)`, `el.gotoFrame(n)`.
- Runnable example: [examples/embed-demo.html](../examples/embed-demo.html).

### 5.3 Native players (no SDK)

`playlist_url` is plain HLS: hand it to AVPlayer (iOS), Media3/ExoPlayer (Android), Safari, or VLC directly. Guidelines for a good clinical experience: start at the `high` rendition and only step down on stall (adaptive "start low" is wrong for grayscale medical content); map timeline position to slice number so doctors can communicate findings by slice; the all-intra encoding makes native frame stepping work.

---

## 6. Exporting an MP4

Each rendition exposes an `export_url` — a single MP4 file (sized to survive WhatsApp's re-compression) for the rare, explicit share-outside case.

- Requires the `imaging.export` scope on the client's registration.
- Every download **and every denial** is written to the audit trail.
- Can be disabled deployment-wide with `OMV_EXPORT_ENABLED=0`.

Export is a deliberate user action, not a default path — in-app streaming is the primary experience.

---

## 7. Machine-to-machine surfaces

### 7.1 Study-ready webhooks

Clients with a registered `webhook_url` receive `study.ready` / `study.failed` events as JSON POSTs, HMAC-signed with the client's `webhook_secret`:

- Signature header: `X-OMV-Signature: sha256=<hex>` — HMAC-SHA256 over the raw request body. **Verify before parsing.**
- Delivery retries 3 times with backoff and never blocks conversion.
- `study.failed` fires **once, on permanent failure only** — after the conversion retry budget is exhausted (§10.3). Transient errors never notify clients.

Register (currently a DB update):

```sql
UPDATE clients SET webhook_url='https://your-app/omv-hook',
                   webhook_secret='<random>' WHERE client_id='<app>';
```

### 7.2 FHIR R4

For EMR/HIS-grade integrations, ready studies are also exposed in standard vocabulary (same OAuth bearer, `imaging.read` scope, reads audited):

| Endpoint | Returns |
|---|---|
| `GET /fhir/metadata` | CapabilityStatement |
| `GET /fhir/ImagingStudy` | Searchset Bundle of ready studies |
| `GET /fhir/ImagingStudy/{StudyInstanceUID}` | Full resource with DICOM modality codings and a contained `Endpoint` (connection type `hls`) whose `address` is a tokenized URL to the study's `manifest.json` |

The Endpoint token is short-lived — integrations re-read the resource to refresh it.

---

## 8. PHI stripping

Videos must never carry patient-identifying pixels; patient context belongs in the client app's authenticated UI.

- **DICOM overlay planes** (group 60xx) never reach the video — the rendering path draws pixel data only.
- **PHI burned into the pixels** (ultrasound demographic banners, console annotations) is removed by per-model crop/mask rules in [deploy/phi-rules.json](../deploy/phi-rules.json) (mounted via `OMV_PHI_RULES`). Rules match case-insensitively on modality / `Manufacturer` / `ManufacturerModelName`; **the first matching rule wins**, so put machine-specific rules above generic ones. The shipped default masks the top 48 px of every US series:

```json
[
  {
    "match":  { "modality": "US" },
    "action": { "mask": [ { "x": 0, "y": 0, "w": 10000, "h": 48 } ] }
  }
]
```

- Filters run before encoding, and the poster is extracted from the *encoded* video, so it inherits the stripping.
- A series tagged `BurnedInAnnotation=YES` with **no matching rule** converts with a loud warning by default; set `OMV_PHI_UNMATCHED_BURNEDIN=skip` to refuse it entirely.
- Adding a rule for a newly observed machine is a config edit, not a release. Refine the rules as real modalities are observed.

---

## 9. Onboarding a new client app

Integration is configuration, not code: each app is a row in the `clients` table with its own `client_id`/secret, allowed scopes (`imaging.read`, optionally `imaging.export`), trusted identity provider (issuer + JWKS for RS256), and optional webhook endpoint. No backend release is needed.

A client can never grant itself broader access than its registration allows, and every authorization decision — allow or deny — is audited per client.

---

## 10. Operations reference

### 10.1 Configuration (`OMV_*` environment variables)

Defaults live in `crates/omv-core/src/config.rs`; the full working set is in [deploy/docker-compose.yml](../deploy/docker-compose.yml).

| Variable | Default | Purpose |
|---|---|---|
| `OMV_DATABASE_URL` | *(required)* | PostgreSQL connection string |
| `OMV_REDIS_URL` | `redis://127.0.0.1:6379` | Job queue |
| `OMV_ORTHANC_URL` / `_USER` / `_PASSWORD` | `http://127.0.0.1:8042`, `omv`/`omv` | Orthanc REST access |
| `OMV_STORAGE_URL` | *(required)* | Object store: `s3://…`, `az://…`, `gs://…`, or `file://…` |
| `OMV_TOKEN_SECRET` | *(required)* | HMAC key for playback tokens — **change from the dev value in production** |
| `OMV_TOKEN_TTL_SECS` | `300` | Playback-token lifetime |
| `OMV_ACCESS_TOKEN_TTL_SECS` | `900` | OAuth access-token lifetime |
| `OMV_EXPORT_ENABLED` | `1` | Deployment-wide MP4-export kill switch |
| `OMV_ENCODER` | `auto` | `auto` / `nvenc` / `x264` (worker) |
| `OMV_RETRY_IDLE_SECS` | `60` | Idle time before a failed job is reclaimed for retry (dev compose uses 15) (worker) |
| `OMV_MAX_ATTEMPTS` | `4` | Conversion attempts before a job is dead-lettered (worker) |
| `OMV_PHI_RULES` | — | Path to the PHI crop/mask rules JSON (worker) |
| `OMV_PHI_UNMATCHED_BURNEDIN` | warn | `skip` refuses unmatched `BurnedInAnnotation=YES` series |
| `OMV_SEED_DEV_CLIENT` | off | `1` seeds the `aadi-dev` client — dev only |
| `OMV_CLIENT_TOKENS` | empty | Deprecated static dev bearer tokens |

### 10.2 Audit trail

Every catalog view, FHIR read, first playback, and export (including denials) appends to the audit table in Postgres: who (practitioner id), via which client app, what study/series/rendition, action, when, and from where. The table is append-only — this is the compliance record.

### 10.3 Health & troubleshooting

- `GET /healthz` on the API returns `ok`.
- **Study never appears in the catalog:** check the study became *stable* in Orthanc (all instances arrived), then the worker logs.
- **Study stuck in `retrying` or gone to `failed`:** transient failures retry automatically — the job is reclaimed after `OMV_RETRY_IDLE_SECS`, with the attempt number taken from the Redis Stream's delivery counter. After `OMV_MAX_ATTEMPTS` the job lands on the `omv:dead` Redis stream with its final error, attempt count, and timestamp, the study goes to `failed`, and the `study.failed` webhook fires. Inspect dead letters with `redis-cli XRANGE omv:dead - +`. To re-drive after fixing the cause, re-POST the idempotent Orthanc event: `curl -X POST .../internal/orthanc-event -d '{"study_id": "<orthanc id>"}'`. Unparseable job payloads dead-letter immediately.
- **401/403 from `/v1/...`:** access token expired (re-exchange), or the client lacks the required scope.
- **Player loads but video won't start:** the playback token likely expired (~5 min) — re-fetch the study for fresh URLs. Tampered or expired tokens are rejected at the streaming path.
- **502s from nginx after recreating the api container:** nginx caches upstream DNS; restart or reload nginx.
- **Worker refuses to start on a GPU host:** `OMV_ENCODER=nvenc` fails fast when the GPU is misconfigured; use `auto` to fall back to software.

### 10.4 Development

```bash
cargo test              # unit tests (token signing, preset mapping, etc.)
cargo check --workspace
```
