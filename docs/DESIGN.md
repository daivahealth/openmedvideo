# OpenMedVideo — Design Document

**DICOM-to-video conversion and streaming platform for clinical apps — app-agnostic by design, with AADI as the first integrator**

| | |
|---|---|
| Status | v1.3 — Phases 1–2 engineering built and verified; integration contract (§5.3) fully implemented incl. FHIR; body-part CT presets shipped; see §9 |
| Date | 2026-08-23 (v1.0: 2026-08-21) |
| Author | Sajith Chandran (sajith.chandran@narayanahealth.org) |
| Audience | Engineering, client app teams (AADI first), clinical stakeholders |

---

## 1. Purpose and scope

### 1.1 Problem

Ordering clinicians need to review medical imaging — CT, MRI, angiography (XA), and ultrasound (US) — on their phones, quickly, without a DICOM viewer or a PACS workstation. Today the practical workaround is photos of screens shared over messaging apps: poor quality, no windowing, no audit trail, and a privacy liability.

### 1.2 Solution

A backend **platform** that converts DICOM studies into standard video (HLS for in-app streaming, MP4 for the rare export case) and streams them to any authorized clinical app — doctor apps, nurse apps, web portals — with authentication, authorization, and a per-view audit log.

The platform is **app-agnostic**: any client integrates through one standard contract (OAuth 2.0/OIDC, a versioned REST API, webhooks, and plain HLS playback — see §5.3). **AADI** (iOS and Android) is the first integrator, not a special case; nothing in the backend is AADI-specific.

A CT or MRI series is a stack of frames; an angio or ultrasound cine loop already *is* a video. Rendered as video, a study becomes something every phone can play natively — with pause, scrub, and frame-step — over a normal HTTPS connection.

### 1.3 Explicit non-goals

- **Not a diagnostic viewer.** Video is 8-bit with a baked-in window/level; the diagnostic read happens on the DICOM in PACS. Every output is labelled *"For clinical review and communication — not for primary diagnosis."*
- Not a PACS, VNA, or archive. Videos are a **regenerable cache**; the DICOM in PACS remains the source of truth. Videos can be expired and re-created at any time.
- Not a live-streaming system. All content is video-on-demand.
- WhatsApp/external sharing is a rare, explicit export action — not the primary path.

---

## 2. Key design decisions

| # | Decision | Rationale |
|---|----------|-----------|
| D1 | **HLS with fMP4 (CMAF) segments** as the streaming format | Apple effectively mandates HLS for streaming in iOS apps; AVPlayer (iOS) and Media3/ExoPlayer (Android) both play it natively. One encode serves both platforms with zero third-party playback SDKs. |
| D2 | **The "media server" is nginx + a small signing/auth API**, not a commercial media server | HLS VOD is static files over HTTPS. Auth, URL signing, and audit are a thin service; delivery is nginx byte-range serving from object storage. |
| D3 | **Orthanc** as the DICOM front door | Battle-tested open-source DICOM server (C-STORE SCP, Q/R, REST API, "study stable" webhooks). Writing our own SCP would reproduce a decade of edge-case handling for no benefit. |
| D4 | **Rust** for the conversion workers | Memory safety over raw pixel buffers, fearless parallelism for fan-out, single static binary deploys. Heavy lifting (decode/encode) stays in ffmpeg/GDCM regardless of orchestration language. |
| D5 | **Convert on arrival, not on request** | Conversion is triggered the moment a study lands from the modality/PACS. The doctor never waits, regardless of pipeline speed. |
| D6 | **Short GOP (~1 s) or all-intra encoding for CT/MRI** | Doctors scrub and frame-step. Long GOPs make seeking sluggish and frame-stepping impossible. This is the single most commonly missed detail in imaging-to-video pipelines. |
| D7 | **Multiple window-preset renditions per CT study** | A video bakes in one window/level. Rendering per-preset videos (e.g. lung / mediastinal / bone) restores the clinically essential views. |
| D8 | **Object storage behind a pluggable abstraction** — MinIO on-prem as default; AWS S3, Azure Blob, or GCS by configuration | Hospital data-residency favours on-prem MinIO, but nothing binds us to one provider: the Rust `object_store` crate gives one interface over S3/Azure/GCS/MinIO/local-FS, selected by config at deploy time. Encryption at rest; lifecycle rules for auto-expiry of regenerable videos. |
| D9 | **Videos carry minimal or coded identifiers** | PHI overlays are stripped at conversion. Patient context comes from the client app's UI (behind auth), not from pixels burned into the video. |
| D10 | **API-first, multi-tenant integration contract** — OAuth 2.0/OIDC token exchange, versioned OpenAPI-described REST, signed webhooks, FHIR-aligned resources | Any doctor/nurse app integrates the same standard way. AADI is the first registered client, not a special case; adding a new app is configuration, not code. |

---

## 3. Architecture

```
Modalities / PACS
      │  DICOM C-STORE (or query-retrieve)
      ▼
┌──────────────────┐   study-stable   ┌────────────────────────────┐
│  Orthanc          │─────webhook────▶│  Job queue (Redis / NATS)   │
│  (DICOM ingest)   │                 └──────────────┬─────────────┘
└──────────────────┘                                 ▼
                                      ┌────────────────────────────┐
                                      │  Conversion workers (Rust)  │
                                      │  fetch → decode → sort →    │
                                      │  window LUTs → ffmpeg/NVENC │
                                      └──────────────┬─────────────┘
                                                     ▼
                                      ┌────────────────────────────┐
                                      │  MinIO (S3) object storage  │
                                      │  .m3u8 + fMP4 segments,     │
                                      │  export MP4, poster JPEG    │
                                      └──────────────▲─────────────┘
                                                     │ range reads
┌───────────────┐  REST: catalog,    ┌───────────────┴────────────┐
│  Client apps   │◀─playback tokens─▶│  Streaming / Catalog API    │
│ AADI, nurse    │◀──HLS over HTTPS─▶│  (OAuth2/OIDC, signed URLs, │
│ apps, portals… │   + webhooks       │   webhooks, audit) + nginx  │
└───────────────┘                    └───────────────┬────────────┘
                                                     ▼
                                              PostgreSQL
                                    (catalog, jobs, audit trail)
```

### 3.1 Components

1. **Ingest (Orthanc).** Modalities or the PACS auto-forward studies. Orthanc's *stable study* event (fires only after all instances have arrived — CT slices trickle in over seconds and conversion must never start on a partial series) enqueues a conversion job.
2. **Job queue (Redis Streams or NATS JetStream).** At-least-once delivery, per-study jobs, retry with backoff, dead-letter queue for poison studies.
3. **Conversion workers (Rust).** Stateless; scale horizontally. Detailed in §4.
4. **Object storage (MinIO).** Bucket layout in §6. Encryption at rest (SSE), lifecycle expiry.
5. **Streaming / Catalog API.** OAuth 2.0/OIDC token validation, per-client registration, study-level authorization, signed-URL issuance, webhook delivery, audit logging, and the catalog endpoints client apps render from. Detailed in §5 and §7.
6. **nginx edge.** TLS termination, byte-range serving of segments from MinIO, response caching. Multi-hospital scale-out = regional nginx caches or a CDN in front; nothing else changes.
7. **PostgreSQL.** Catalog (studies, series, renditions), job state, audit trail.

---

## 4. Conversion pipeline

### 4.1 Stages

```
fetch series (Orthanc REST)
  → decode pixel data          dicom-rs; GDCM subprocess fallback for
                               exotic transfer syntaxes (JPEG-Lossless,
                               JPEG2000, RLE, old JPEGs)
  → geometric sort             by ImagePositionPatient projected onto the
                               slice normal — NEVER by filename or
                               InstanceNumber (classic corruption bug)
  → modality LUT               RescaleSlope/Intercept → real units (HU)
  → window/level per preset    → normalize to 8-bit with dithering
  → strip PHI overlays         drop overlay planes / burned-in annotation
                               regions where detectable
  → optional overlay           slice counter ("47/312"), series label,
                               "not for diagnosis" footer
  → pipe raw frames to ffmpeg  streaming: never hold the full
                               decompressed series in RAM
  → outputs                    HLS ladder + export MP4 + poster JPEG
  → register in catalog        write renditions to Postgres, emit
                               "study ready" event (push to AADI later)
```

### 4.2 Per-modality presets

| Modality | Frame rate | Renditions | Notes |
|----------|-----------|------------|-------|
| **CT** | 8–10 fps | One video per clinical window preset, selected by BodyPartExamined (chest → lung/mediastinal/bone; head → brain/subdural/bone; abdomen/pelvis → soft-tissue/bone; spine/neck → soft/bone; missing or unrecognized tag → general soft/lung/bone) | All-intra or 1 s GOP for scrubbing. Matching is contains-based over the uppercased tag — consoles emit both DICOM defined terms and free-ish text. Coronal/sagittal reformats are Phase 3. |
| **MRI** | 8–10 fps | One video per series (T1/T2/FLAIR/DWI are already separate series) | Auto window from pixel-value percentiles (2nd–98th). |
| **US** | Native cine rate from FrameTime / FrameTimeVector tags | One per cine loop | Near-lossless use case; preserve timing exactly. |
| **XA / fluoro (angio)** | Native cine rate | One per run | Same as US; runs labelled by acquisition angle where available. |
| **CR / DX (X-ray)** | — | High-quality JPEG/PNG still, no video | Player shows a zoomable image instead. |

### 4.3 Encoding parameters

- **Codec:** H.264 High profile, `yuv420p` (universal hardware decode on both platforms). H.265 revisited later only if bandwidth demands it (Android hardware decode is uneven).
- **GOP:** all-intra for CT/MRI stacks; ≤1 s keyframe interval for cine content. Emit the HLS **I-frame playlist** (`EXT-X-I-FRAME-STREAM-INF`) for instant seek previews.
- **Bitrate ladder (HLS master playlist):**
  - `high` — native resolution (CT/MRI are typically 512×512), quality-targeted (CRF ≈ 18 or NVENC CQ equivalent). Grayscale medical content needs a *higher* bitrate-per-pixel than natural video to avoid banding in smooth regions.
  - `medium` — native resolution, ~half the bitrate.
  - `low` — half resolution, for degraded 4G.
- **Segments:** fMP4 (CMAF), 2 s duration, `+faststart`-equivalent layout.
- **Export MP4:** single-file H.264/AAC-silent, sized to survive WhatsApp's ~16 MB re-compression when the rare export is used.
- **Encoder:** NVENC when a GPU is present (500+ fps at CT resolutions → a 400-slice series in ~1 s per rendition); libx264 `preset=fast` fallback. Worker auto-detects at startup.

### 4.4 Performance model

The orchestration language contributes ~5–10% of runtime; the levers that matter:

1. **Hardware encoding (NVENC):** 10–20× over software.
2. **Streaming pipeline:** decode slice N+1 while N encodes; peak RAM stays at a few frames, not the ~1 GB a decompressed 1000-slice CT would need.
3. **Rendition-level parallelism:** each window preset / bitrate tier is an independent encode — perfect fan-out across cores or GPU sessions.
4. **Convert-on-arrival (D5):** perceived latency is zero; actual conversion time only matters for the queue-depth SLO.

**Target SLO:** study ready in AADI within **60 s** of the study becoming stable in Orthanc (p95).

---

## 5. Streaming and playback

### 5.1 Delivery flow

1. The client app calls `GET /v1/studies/{id}` with its access token → catalog response includes renditions and a **playback token** (short-lived, ~5 min, scoped to the study's storage prefix).
2. App hands the master-playlist URL (token as query param or cookie) to AVPlayer / ExoPlayer.
3. nginx edge validates the token signature (shared-secret HMAC or JWKS — no API round-trip per segment), range-serves segments from MinIO, caches hot segments.
4. First playback of a study writes an audit event (§7.3).

A prefix-scoped token rather than per-file signed URLs is deliberate: one HLS playback touches hundreds of segment files.

### 5.2 Client-side playback guidelines (reference implementation: AADI)

- **Players:** AVPlayer (iOS), Media3 ExoPlayer (Android). No third-party SDK.
- **Slice-aware scrub bar:** map timeline position → slice number (`47/312`) so doctors communicate findings by slice; ±1 frame-step buttons (both players support frame stepping given the all-intra encoding, D6).
- **Start at the `high` rendition**, step down only on stall — the default "start low, adapt up" behaviour is wrong for medical grayscale.
- **Window-preset switcher** for CT (tabs: Lung / Mediastinal / Bone) — switching renditions of the same study, ideally preserving playback position.
- Poster thumbnails in the study list from the catalog.
- Screenshot/screen-recording deterrence per NH mobile policy (best-effort; FLAG_SECURE on Android).

### 5.3 Integration contract — how any app plugs in

The platform is multi-tenant and app-agnostic. Integrating a new doctor/nurse app is **configuration, not code**, and consists of five standard pieces:

1. **Client registration.** Each app (AADI, a nurse app, a web portal, a referring-physician portal) is registered as an OAuth 2.0 client with its own credentials, allowed scopes (`imaging.read`, `imaging.export`), trusted identity provider, webhook endpoint, and rate limits. Registration lives in the catalog DB; no backend release needed.
2. **Authentication: OAuth 2.0 / OIDC token exchange (RFC 8693).** The app exchanges its end-user's identity token (from whatever IdP that app uses) for a short-lived OpenMedVideo access token carrying the practitioner identity and client id. The platform never handles passwords and never trusts a client-asserted identity without a registered IdP behind it. Pure server-to-server integrations use standard client-credentials grant.
3. **Versioned REST API described by OpenAPI.** All catalog and playback-token endpoints under `/v1/…`, with a published OpenAPI spec so client teams can generate SDKs. Breaking changes only ever arrive as `/v2`.
4. **Standard playback — no SDK required.** Output is plain HLS over HTTPS: AVPlayer (iOS), Media3/ExoPlayer (Android), hls.js or Safari (web) all play it natively. For teams that don't want to build a native player UI, the platform also serves an **embeddable web player** (a single tokenized URL, suitable for a WebView or iframe) with the slice scrub bar and window-preset switcher built in.

   **Player distribution strategy — one core, thin wrappers, no parallel renderers.** Video decoding is already native everywhere, so we never ship a renderer; what we ship is the clinical UI (slice scrub bar, frame stepping, preset switcher), built exactly once:
   - *Tier 1 (default):* the embeddable player via iframe/WebView — zero integration code, works in every framework, UI fixes ship centrally.
   - *Tier 2 (on demand):* the same player packaged as a framework-agnostic **Web Component** (`<omv-player study-id token>`); published Angular/React npm packages are then ~50-line wrappers around the one codebase.
   - *Tier 3 (only if proven demand):* a native Flutter package wrapping `video_player` plus the slice UI.

   All tiers consume the same per-study `manifest.json` (renditions, presets, slice counts) — the manifest is the contract; every player is just a view over it.
5. **Signed webhooks + FHIR-aligned resources.** `study.ready`, `study.failed`, and `study.expired` events are POSTed to each client's registered endpoint, HMAC-signed. For EMR/HIS-grade integrations, studies are additionally exposed as **FHIR R4 `ImagingStudy` resources with an `Endpoint`** pointing at the HLS manifest — so healthcare systems can discover video renditions using standard vocabulary rather than a proprietary API.

**Authorization stays with the platform.** Client identity and user claims are inputs; the study-level decision ("is this practitioner allowed to view this patient's imaging?") is made by a pluggable policy hook against the care-team source of truth (§7.1). A client app can never grant itself broader access than its registration allows, and every decision — allow or deny — is audited per client (§7.3).

---

## 6. Storage layout

```
s3://medvideo/
  {study_uid}/
    manifest.json                      # renditions, presets, slice counts
    poster.jpg
    {series_uid}/{preset}/
      master.m3u8
      high/  index.m3u8  seg_00001.m4s …
      medium/ …
      low/ …
      iframe.m3u8
      export.mp4
```

- **Cloud portability:** all storage access goes through the Rust `object_store` abstraction (one trait, native backends for S3, Azure Blob, GCS, MinIO, local FS), so the provider is a deploy-time config URL (`s3://…`, `az://…`, `gs://…`, `file://…`), not a code change. Notes per provider: GCS also speaks the S3 XML API natively (interoperability mode); Azure Blob does *not* speak S3, which is exactly why the abstraction sits in code rather than relying on the S3 protocol. The delivery path is already provider-neutral: playback tokens are our own HMAC prefix tokens validated at the nginx edge, not storage-provider presigned URLs, so streaming behaves identically on-prem or on any cloud.
- **Lifecycle:** videos auto-expire after a configurable window (e.g. 90 days since last view). A request for an expired study enqueues regeneration (source DICOM still in PACS) and the API returns `202 Accepted` with a retry hint.
- **Encryption at rest** (MinIO SSE); TLS everywhere in transit.

---

## 7. Security, privacy, compliance

### 7.1 Authentication & authorization

- Clinicians authenticate in their own app; the app performs OAuth 2.0 token exchange (§5.3) and the backend validates the resulting access token (issuer/audience/expiry, JWKS) plus the client's registration and scopes.
- **Study-level authorization:** is this doctor permitted to view this patient's imaging? Enforced at the catalog API via care-team / ordering-clinician linkage from the HIS (FHIR where available). Deny by default.
- Playback tokens are short-lived, prefix-scoped, and non-renewable without re-hitting the authorized catalog endpoint.

### 7.2 PHI handling (DPDP alignment)

- Pixel-domain PHI (overlay planes, burned-in annotations) stripped at conversion where detectable; modalities known to burn in demographics get per-model crop rules.
- Videos carry at most a coded study reference in-frame; patient identity is displayed by the app from the authorized catalog, never baked into shareable pixels.
- The export-MP4 path requires an explicit user action in the client app, the `imaging.export` scope, and is logged (§7.3); it can be disabled per client or per deployment.

### 7.3 Audit

Append-only audit table: `{who (practitioner id), via (client app id), what (study/series/rendition), action (view | export | denied), when, from (app version, IP)}`. This is the compliance story — and a categorical improvement over the photos-on-WhatsApp status quo.

### 7.4 Network posture

- All services on the hospital network / private VPC; only the nginx edge is exposed, TLS 1.2+.
- Orthanc's DICOM port reachable only from modality/PACS VLANs.
- No PHI in URLs beyond opaque UIDs; no query-string patient data.

---

## 8. Technology summary

| Concern | Choice | Alternatives considered |
|---|---|---|
| DICOM ingest | Orthanc | Custom Rust SCP (rejected: edge-case burden), dcm4chee (heavier than needed) |
| Queue | Redis Streams | NATS JetStream (fine too), RabbitMQ |
| Workers | Rust (`dicom-rs`, `tokio`), ffmpeg subprocess, GDCM fallback | Go/Python orchestration (viable; Rust chosen for safety + footprint) |
| Encoding | ffmpeg: NVENC primary, libx264 fallback | GStreamer (no advantage here) |
| Storage | MinIO on-prem (default), via the `object_store` abstraction | AWS S3 / Azure Blob / GCS — same code, config-selected backend |
| Streaming format | HLS / CMAF fMP4 | MPEG-DASH (no native iOS support), raw MP4 progressive (no adaptation, slow start) |
| Edge | nginx | Caddy, CDN (added at Phase 3) |
| API | Rust (axum) or Go — small service, team's call | |
| DB | PostgreSQL | |

---

## 9. Rollout plan

### Phase 1 — MVP (single box) — ✅ built and verified, 2026-08-21

The full compose stack (Orthanc + Redis + Postgres + MinIO + Rust worker +
streaming/catalog API + nginx cache) converts and streams end-to-end. Measured
against the exit criteria with a synthetic US cine + 40-slice CT study:
conversion completed **~0.7 s** after the stable-study webhook (~35 s
doctor-visible including Orthanc's 30 s stable window — well under the ≤5 min
target), producing 4 renditions (CT soft/lung/bone at 8 fps all-intra, US at
its native 20 fps from the DICOM tags); ffprobe validated the HLS through
nginx; tampered tokens and wrong bearers were rejected; audit events recorded.

*Still open from Phase 1 scope:* pilot with a real clinician group on real
modality data.

### Phase 2 — Production hardening — engineering ✅ (2026-08-22), operational items open

Built and verified:
- **Integration contract**: client registry (`clients` table — onboarding an
  app is a row, not a release), RFC 8693 token exchange + client_credentials,
  JWKS validation for RS256 IdPs, scope enforcement at issuance and use,
  practitioner identity flowing into the audit trail.
- **Players**: embeddable web player (tier 1, one tokenized URL) and the
  `<omv-player>` Web Component (tier 2) sharing one codebase — slice scrub
  bar, ±1 frame stepping, preset/series tabs, events and theming hooks for
  host apps; hls.js vendored into the binary (offline hospital networks),
  CORS for cross-origin embeds.
- **Geometric slice ordering**: ImagePositionPatient projected onto the
  series normal, InstanceNumber fallback on missing/degenerate geometry;
  verified at the pixel level against a scrambled-InstanceNumber study.
- **Export MP4**: lossless re-mux per rendition (re-encoded only if over the
  WhatsApp-safe ~14 MB), gated by the `imaging.export` scope, audited
  (denials included), deployment-wide kill switch.
- **NVENC**: runtime smoke-test detection (auto/nvenc/x264), fail-fast on
  misconfigured GPU hosts, GPU compose overlay. Throughput validation awaits
  actual GPU hardware.
- **Signed webhooks**: `study.ready`/`study.failed` HMAC-signed per client,
  bounded retries, verified receiver-side.
- **FHIR R4 exposure** (pulled forward from Phase 3, 2026-08-23): a
  CapabilityStatement at `/fhir/metadata`, `ImagingStudy` search (searchset
  Bundle, DICOM modality codings, `urn:oid:` identifiers) and read — the full
  resource's series reference a contained `Endpoint` (connectionType `hls`)
  whose address is a fresh short-lived tokenized URL to the study's
  `manifest.json`; integrations re-read the resource to refresh the token.
  Same OAuth bearer and `imaging.read` scope; reads audited. Verified E2E
  from FHIR search to a playing manifest without touching the proprietary
  catalog API. With this, **all five integration surfaces of §5.3 are
  implemented**: client registry + OAuth, versioned REST catalog, standard
  HLS playback + both player tiers, signed webhooks, and FHIR.
- **Body-part-driven CT presets** (2026-08-23): window sets selected from
  BodyPartExamined per the §4.2 table, general-set fallback on missing or
  unrecognized tags, chosen set logged per series. Verified E2E: a HEAD
  study produced brain/subdural/bone renditions with the US series
  untouched; unit tests cover the mappings, free-text variants, and
  fallbacks.

Open (operational, mostly needing real data/infra):
- PHI-strip rules per modality model and burned-in-annotation handling —
  driven by what real NH modalities actually emit.
- AADI's real IdP registered in the client registry; AADI player integration.
- Monitoring (queue depth, conversion p95, playback error rate) and the 60 s
  SLO measured on production hardware; dead-letter retry queue.
- Regression corpus from real-world DICOM failures.

### Phase 3 — Scale-out (not started)

Multi-hospital ingest, second and third client apps onboarded via the registry (nurse app, web portal), regional edge caches/CDN, coronal/sagittal reformats for CT, storage lifecycle + regenerate-on-demand, capacity planning from measured volumes. (FHIR `ImagingStudy`/`Endpoint` exposure was originally scoped here; it was pulled forward and shipped with Phase 2 — see above.)

### Open questions (needed to size Phase 2)

1. Studies/day per hospital, and modality mix?
2. Peak concurrent viewers on AADI?
3. GPU availability in NH data centres (a single mid-range NVIDIA card likely covers full volume)?
4. Care-team authorization source of truth — HIS API, FHIR server, or each client app's backend?
5. Retention window for generated videos?
6. Which identity providers do the first client apps (AADI, nurse app) use, and do they support OIDC token exchange?

---

## 10. Risks

| Risk | Mitigation |
|---|---|
| Clinicians treat videos as diagnostic | Persistent in-frame and in-app labelling; clinical governance sign-off on wording |
| Exotic/legacy DICOM fails to decode | GDCM fallback path; dead-letter queue with alerting; keep a corpus of failing studies as regression tests |
| Banding/artifacts hide subtle findings | Quality-targeted (CRF) encoding, dithered 8-bit reduction, per-modality QA review during pilot |
| Partial-series conversion | Orthanc stable-study event + slice-count/geometry sanity checks before encode |
| Token leakage → segment access | Short TTL, prefix scoping, TLS-only, tokens never logged |
| Storage growth | Videos are regenerable cache — aggressive lifecycle expiry is safe by design |
