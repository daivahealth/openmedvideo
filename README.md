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
3. **Authenticate and browse the catalog** through nginx. The real flow is
   OAuth 2.0 token exchange (RFC 8693): the client app swaps its user's IdP
   token for an OMV access token. The compose stack seeds a dev client
   (`aadi-dev` / `aadi-dev-secret`) registered against a fake HS256 IdP, and
   `scripts/idp_token.py` mints the IdP token a real identity provider would:
   ```bash
   IDP_TOKEN=$(python3 scripts/idp_token.py dr.asha)
   ACCESS_TOKEN=$(curl -s -u aadi-dev:aadi-dev-secret http://localhost:8000/oauth/token \
     -d grant_type=urn:ietf:params:oauth:grant-type:token-exchange \
     -d subject_token=$IDP_TOKEN | jq -r .access_token)
   curl -H "Authorization: Bearer $ACCESS_TOKEN" http://localhost:8000/v1/studies
   curl -H "Authorization: Bearer $ACCESS_TOKEN" \
        http://localhost:8000/v1/studies/<StudyInstanceUID>
   ```
   Server-to-server integrations use `grant_type=client_credentials` instead.
   (The static `Bearer dev-client-token` from `OMV_CLIENT_TOKENS` still works
   as a deprecated dev fallback.)
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

## Embedding the player in your own app (`<omv-player>`)

The player UI is a framework-agnostic **Web Component** (design §5.3 tier 2) —
the `/player/...` page above is just a thin shell over it. Any web app
(Angular, React, Vue, plain HTML, or a WebView) embeds it with one script and
one element:

```html
<script src="https://your-omv-server/player-assets/omv-player.js"></script>
<omv-player server="https://your-omv-server"
            study-id="1.2.840..."
            token="<playback token from GET /v1/studies/{uid}>">
</omv-player>
```

- Size it from CSS (it fills its host). Theme it via `--omv-accent`,
  `--omv-bg`, … custom properties.
- Events: `omv-ready`, `omv-error`, `omv-frame` (`{frame, frames}`).
  Methods: `el.step(±1)`, `el.gotoFrame(n)`.
- hls.js is lazy-loaded from the server only when the browser lacks native
  HLS. The API sends permissive CORS on `/stream` and `/player-assets`
  (access control is the playback token, not the Origin header).
- Runnable example: [examples/embed-demo.html](examples/embed-demo.html).

## Phase 1 design shortcuts (deliberate)

- **Orthanc renders the frames** (`/instances/…/rendered`): it applies the
  modality/VOI LUTs and window parameters and handles every transfer syntax,
  absorbing DICOM edge cases. Native Rust pixel decoding (dicom-rs) is a
  Phase 2 throughput optimization, not a correctness need.
- **The API streams HLS objects itself** after validating the playback token;
  nginx in front caches them. Phase 2 moves segment serving to nginx directly.
- **Single-rendition ladder** (CRF 18 "high" only). CR/DX stills are skipped.
- **libx264 software encoding**; NVENC lands in Phase 2 on GPU hosts.

What already matches the design: the OAuth2/OIDC integration contract
(client registry with per-app scopes and IdP, RFC 8693 token exchange,
client_credentials, JWKS validation for RS256 IdPs), convert-on-arrival via
the stable-study webhook, geometric slice ordering (ImagePositionPatient projected onto the
series normal, InstanceNumber fallback), all-intra encoding for CT/MRI stacks
(frame-accurate scrubbing),
native cine rates for US/XA from the DICOM tags, three hardcoded CT window
presets (soft/lung/bone) as separate renditions, prefix-scoped HMAC playback
tokens, per-view audit events, provider-neutral storage (MinIO/S3/Azure/GCS
via `object_store`), and the export-MP4 path: each rendition also produces a
single `export.mp4` (lossless re-mux of the HLS; re-encoded smaller only if it
exceeds the WhatsApp-safe ~14 MB), downloadable via each rendition's
`export_url` — requires the `imaging.export` scope, is audited per download
(denials too), and can be disabled deployment-wide with
`OMV_EXPORT_ENABLED=0`.

## Development

```bash
cargo test            # unit tests (token signing, etc.)
cargo check --workspace
```

The services read config from `OMV_*` env vars — see `deploy/docker-compose.yml`
for the full set and `crates/omv-core/src/config.rs` for defaults.
