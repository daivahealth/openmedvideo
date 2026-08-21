# OpenMedVideo

Converts DICOM studies (CT, MRI, ultrasound, angio) into standard streaming
video and serves them to clinical apps over HLS — so a clinician can review
imaging on any phone with play/pause/scrub, no DICOM viewer required.

> **Not a diagnostic viewer.** Every output is for clinical review and
> communication; the diagnostic read happens on the DICOM in PACS.
> Videos are a regenerable cache — PACS remains the source of truth.

Full architecture and rationale: [docs/DESIGN.md](docs/DESIGN.md).

## Layout

| Path | What |
|---|---|
| `crates/omv-core` | Shared: config, provider-neutral storage (`object_store`), playback-token signing, catalog models |
| `crates/omv-api` | Streaming/Catalog API (axum): Orthanc webhook → job queue, study catalog, token-gated HLS delivery, audit trail |
| `crates/omv-worker` | Conversion worker: Redis job → Orthanc rendered frames → ffmpeg → HLS (fMP4) → object storage |
| `deploy/` | Single-box Phase 1 stack: Orthanc, Redis, Postgres, MinIO, api, worker, nginx |

## Quick start (Phase 1 dev stack)

```bash
docker compose -f deploy/docker-compose.yml up --build
```

Then:

1. **Send a study** to the Orthanc endpoint — AET `OMV`, host `localhost`, port `4242`
   (e.g. `storescu -aec OMV localhost 4242 *.dcm`, or upload via the Orthanc UI
   at http://localhost:8042, user/pass `omv`/`omv`).
2. ~30 s after the last instance arrives (Orthanc `StableAge`), the study is
   converted automatically.
3. **Browse the catalog** through nginx:
   ```bash
   curl -H "Authorization: Bearer dev-client-token" http://localhost:8000/v1/studies
   curl -H "Authorization: Bearer dev-client-token" \
        -H "X-Practitioner-Id: dr.demo" \
        http://localhost:8000/v1/studies/<StudyInstanceUID>
   ```
4. Open the `player_url` from the study response in any browser — it's the
   **embeddable web player** (design §5.3 tier 1): slice-aware scrub bar
   (`21/40`), ±1 frame stepping (also ←/→ keys), window-preset tabs for CT
   that preserve the playback position, series switcher, loop toggle. One
   tokenized URL, made for WebViews/iframes in client apps.
5. Or open any `playlist_url` directly in Safari, VLC, AVPlayer, or
   ExoPlayer — it's plain HLS with a short-lived, study-scoped token baked
   into the path.

The player is fully self-contained: hls.js (v1.6.15, Apache-2.0) is vendored
into the API binary and served at `/player-assets/hls.min.js`, so it works on
offline hospital networks with no CDN dependency.

## Phase 1 design shortcuts (deliberate)

- **Orthanc renders the frames** (`/instances/…/rendered`): it applies the
  modality/VOI LUTs and window parameters and handles every transfer syntax,
  absorbing DICOM edge cases. Native Rust pixel decoding (dicom-rs) is a
  Phase 2 throughput optimization, not a correctness need.
- **The API streams HLS objects itself** after validating the playback token;
  nginx in front caches them. Phase 2 moves segment serving to nginx directly.
- **Static bearer tokens** (`OMV_CLIENT_TOKENS`) stand in for the OAuth2/OIDC
  token exchange defined in the design's integration contract (§5.3).
- **Sorting by InstanceNumber**; geometric sort by ImagePositionPatient is
  Phase 2. Single-rendition ladder (CRF 18 "high" only). CR/DX stills and the
  export-MP4 path are skipped.
- **libx264 software encoding**; NVENC lands in Phase 2 on GPU hosts.

What already matches the design: convert-on-arrival via the stable-study
webhook, all-intra encoding for CT/MRI stacks (frame-accurate scrubbing),
native cine rates for US/XA from the DICOM tags, three hardcoded CT window
presets (soft/lung/bone) as separate renditions, prefix-scoped HMAC playback
tokens, per-view audit events, and provider-neutral storage (MinIO/S3/Azure/GCS
via `object_store`).

## Development

```bash
cargo test            # unit tests (token signing, etc.)
cargo check --workspace
```

The services read config from `OMV_*` env vars — see `deploy/docker-compose.yml`
for the full set and `crates/omv-core/src/config.rs` for defaults.
