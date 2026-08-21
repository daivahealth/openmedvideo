/**
 * <omv-player> — framework-agnostic clinical imaging player (design §5.3 tier 2).
 *
 * Usage:
 *   <script src="https://your-omv-server/player-assets/omv-player.js"></script>
 *   <omv-player server="https://your-omv-server"
 *               study-id="1.2.840..."
 *               token="eyJ..."></omv-player>
 *
 * Attributes:
 *   server    OMV API origin. Defaults to the origin the script came from,
 *             or the page origin as a last resort.
 *   study-id  DICOM StudyInstanceUID.
 *   token     Prefix-scoped playback token from GET /v1/studies/{uid}.
 *
 * Events: "omv-ready" {studyUid}, "omv-error" {message},
 *         "omv-frame" {frame, frames} (fires when the displayed frame changes).
 *
 * The element sizes to its host — give it width/height (or flex) from CSS.
 * Angular/React/Vue wrappers are thin shims over this one element.
 */
(function () {
  "use strict";
  if (customElements.get("omv-player")) return;

  // Where this script was served from — the natural default for `server`.
  const SCRIPT_ORIGIN = (() => {
    try {
      return new URL(document.currentScript.src).origin;
    } catch {
      return "";
    }
  })();

  /** Loads hls.js once, from the OMV server's vendored copy, only when the
   *  browser has no native HLS support. */
  let hlsPromise = null;
  function ensureHls(server) {
    const probe = document.createElement("video");
    if (probe.canPlayType("application/vnd.apple.mpegurl")) {
      return Promise.resolve(null); // Safari/iOS: native
    }
    if (window.Hls) return Promise.resolve(window.Hls);
    if (!hlsPromise) {
      hlsPromise = new Promise((resolve, reject) => {
        const s = document.createElement("script");
        s.src = `${server}/player-assets/hls.min.js`;
        s.onload = () => resolve(window.Hls || null);
        s.onerror = () => reject(new Error("failed to load hls.js"));
        document.head.appendChild(s);
      });
    }
    return hlsPromise;
  }

  const TEMPLATE = `
<style>
  :host {
    /* Overridable theme hooks for host apps. */
    --omv-bg: #0c0f12; --omv-panel: #161b20; --omv-ink: #dce4ea;
    --omv-muted: #8fa0ad; --omv-accent: #4fb8c0; --omv-line: #26323c;
    display: flex; flex-direction: column; min-height: 320px;
    background: var(--omv-bg); color: var(--omv-ink);
    font: 14px/1.45 -apple-system, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
    border-radius: 8px; overflow: hidden;
    -webkit-user-select: none; user-select: none; touch-action: manipulation;
  }
  header {
    padding: 10px 14px 8px; border-bottom: 1px solid var(--omv-line);
    display: flex; flex-wrap: wrap; gap: 4px 12px; align-items: baseline;
  }
  header .title { font-weight: 600; }
  header .mod { color: var(--omv-accent); font-weight: 600; letter-spacing: .05em; }
  header .desc { color: var(--omv-muted); }
  .bar { display: flex; gap: 6px; padding: 8px 14px 0; flex-wrap: wrap; }
  .tab {
    background: var(--omv-panel); color: var(--omv-muted);
    border: 1px solid var(--omv-line); border-radius: 999px;
    padding: 5px 14px; font-size: 13px; cursor: pointer; font: inherit;
  }
  .tab.active {
    color: var(--omv-bg); background: var(--omv-accent);
    border-color: var(--omv-accent); font-weight: 600;
  }
  .stage {
    flex: 1; min-height: 0; display: flex; align-items: center;
    justify-content: center; padding: 10px 14px;
  }
  video { max-width: 100%; max-height: 100%; background: #000; border-radius: 6px; }
  .controls { padding: 6px 14px 10px; display: flex; flex-direction: column; gap: 6px; }
  .scrub-row { display: flex; align-items: center; gap: 10px; }
  input[type=range] { flex: 1; accent-color: var(--omv-accent); height: 28px; }
  .counter {
    font-variant-numeric: tabular-nums; min-width: 76px; text-align: right;
    font-size: 15px;
  }
  .buttons { display: flex; gap: 8px; justify-content: center; }
  .buttons button {
    background: var(--omv-panel); color: var(--omv-ink);
    border: 1px solid var(--omv-line); border-radius: 8px;
    padding: 8px 18px; font-size: 16px; cursor: pointer; min-width: 56px;
  }
  .buttons button:active { background: var(--omv-accent); color: var(--omv-bg); }
  .buttons button.on { color: var(--omv-accent); }
  footer {
    padding: 6px 14px 10px; color: var(--omv-muted); font-size: 11.5px;
    border-top: 1px solid var(--omv-line); text-align: center;
  }
  .error { color: #e0a93e; padding: 20px; text-align: center; }
  .hidden { display: none !important; }
</style>
<header>
  <span class="mod"></span><span class="title">Loading…</span><span class="desc"></span>
</header>
<div class="bar series-bar hidden"></div>
<div class="bar preset-bar hidden"></div>
<div class="stage"><video playsinline preload="auto"></video></div>
<div class="controls">
  <div class="scrub-row">
    <input type="range" class="scrub" min="1" max="1" step="1" value="1" aria-label="Frame">
    <span class="counter">–/–</span>
  </div>
  <div class="buttons">
    <button class="back" title="Previous frame (←)">⏮</button>
    <button class="play" title="Play/pause (space)">▶</button>
    <button class="fwd" title="Next frame (→)">⏭</button>
    <button class="loop" title="Loop">🔁</button>
  </div>
</div>
<footer></footer>`;

  class OmvPlayer extends HTMLElement {
    static get observedAttributes() {
      return ["server", "token", "study-id"];
    }

    constructor() {
      super();
      this.attachShadow({ mode: "open" });
      this._hls = null;
      this._fps = 8;
      this._frames = 1;
      this._lastFrame = 0;
      this._series = [];
      this._curSeries = 0;
      this._curPreset = 0;
      this._raf = 0;
    }

    get _server() {
      return (this.getAttribute("server") || SCRIPT_ORIGIN || location.origin)
        .replace(/\/$/, "");
    }
    get _base() {
      return `${this._server}/stream/${this.getAttribute("token")}` +
        `/studies/${this.getAttribute("study-id")}`;
    }

    connectedCallback() {
      this.shadowRoot.innerHTML = TEMPLATE;
      this._wire();
      this._load();
    }
    disconnectedCallback() {
      cancelAnimationFrame(this._raf);
      if (this._hls) this._hls.destroy();
    }
    attributeChangedCallback(_n, oldV, newV) {
      if (oldV !== null && oldV !== newV && this.isConnected) this._load();
    }

    $(sel) { return this.shadowRoot.querySelector(sel); }

    _wire() {
      const video = this.$("video");
      const scrub = this.$(".scrub");

      const frameOf = (t) =>
        Math.min(this._frames, Math.max(1, Math.round(t * this._fps) + 1));
      const timeOf = (f) => (f - 1) / this._fps + 0.001;
      this._frameOf = frameOf;
      this._timeOf = timeOf;

      const tick = () => {
        const f = frameOf(video.currentTime);
        if (f !== this._lastFrame) {
          this._lastFrame = f;
          this.dispatchEvent(new CustomEvent("omv-frame", {
            detail: { frame: f, frames: this._frames },
          }));
        }
        this.$(".counter").textContent = `${f}/${this._frames}`;
        if (this.shadowRoot.activeElement !== scrub) scrub.value = f;
        this.$(".play").textContent = video.paused ? "▶" : "⏸";
        this._raf = requestAnimationFrame(tick);
      };
      this._raf = requestAnimationFrame(tick);

      scrub.addEventListener("input", () => {
        video.pause();
        video.currentTime = timeOf(+scrub.value);
      });
      const step = (d) => {
        video.pause();
        video.currentTime = timeOf(frameOf(video.currentTime) + d);
      };
      this.$(".back").onclick = () => step(-1);
      this.$(".fwd").onclick = () => step(1);
      this.$(".play").onclick = () =>
        video.paused ? video.play() : video.pause();
      this.$(".loop").onclick = (e) => {
        video.loop = !video.loop;
        e.target.classList.toggle("on", video.loop);
      };
      video.onclick = this.$(".play").onclick;

      this.tabIndex = 0; // make the element focusable for keyboard control
      this.addEventListener("keydown", (e) => {
        if (e.key === " ") { e.preventDefault(); this.$(".play").onclick(); }
        else if (e.key === "ArrowLeft") step(-1);
        else if (e.key === "ArrowRight") step(1);
      });

      // Public API for host apps.
      this.step = step;
      this.gotoFrame = (f) => { video.pause(); video.currentTime = timeOf(f); };
    }

    async _load() {
      const token = this.getAttribute("token");
      const studyId = this.getAttribute("study-id");
      if (!token || !studyId) return;

      let manifest;
      try {
        const res = await fetch(`${this._base}/manifest.json`);
        if (!res.ok) throw new Error(`HTTP ${res.status}`);
        manifest = await res.json();
      } catch (e) {
        this._fail("Could not load the study — the link may have expired. " +
          "Reopen it from your app to get a fresh one.");
        return;
      }

      this.$(".title").textContent = manifest.description || "Imaging study";
      this.$("footer").textContent = manifest.disclaimer;
      this.$("video").poster = `${this._base}/poster.jpg`;

      this._series = [];
      for (const r of manifest.renditions) {
        let s = this._series.find((s) => s.uid === r.series_uid);
        if (!s) {
          s = { uid: r.series_uid, desc: r.series_description,
                modality: r.modality, presets: [] };
          this._series.push(s);
        }
        s.presets.push(r);
      }
      this._curSeries = 0;
      this._curPreset = 0;
      await this._show(false);
      this.dispatchEvent(new CustomEvent("omv-ready", {
        detail: { studyUid: manifest.study_uid },
      }));
    }

    _renderTabs() {
      const mkTabs = (bar, items, current, label, onpick) => {
        bar.innerHTML = "";
        bar.classList.toggle("hidden", items.length <= 1);
        items.forEach((item, i) => {
          const b = document.createElement("button");
          b.className = "tab" + (i === current ? " active" : "");
          b.textContent = label(item, i);
          b.onclick = () => onpick(i);
          bar.appendChild(b);
        });
      };
      mkTabs(this.$(".series-bar"), this._series, this._curSeries,
        (s, i) => `${s.modality} · ${s.desc || "Series " + (i + 1)}`,
        (i) => { this._curSeries = i; this._curPreset = 0; this._show(false); });
      mkTabs(this.$(".preset-bar"), this._series[this._curSeries].presets,
        this._curPreset, (p) => p.preset_label,
        // Same series, different window: preserve the playback position.
        (i) => { this._curPreset = i; this._show(true); });
    }

    async _show(keepPosition) {
      const video = this.$("video");
      const s = this._series[this._curSeries];
      const r = s.presets[this._curPreset];
      const keep = keepPosition ? video.currentTime : 0;
      this._fps = r.fps;
      this._frames = r.frames;
      this.$(".scrub").max = r.frames;
      this.$(".mod").textContent = s.modality;
      this.$(".desc").textContent = s.desc;
      this._renderTabs();

      const src = `${this._base}/${r.playlist}`;
      if (this._hls) { this._hls.destroy(); this._hls = null; }
      let Hls = null;
      try {
        Hls = await ensureHls(this._server);
      } catch (e) {
        this._fail("Could not load the video engine (hls.js).");
        return;
      }
      if (!Hls || !Hls.isSupported()) {
        video.src = src; // native HLS
      } else {
        this._hls = new Hls({ maxBufferLength: 120 });
        this._hls.loadSource(src);
        this._hls.attachMedia(video);
      }
      video.addEventListener("loadedmetadata",
        () => { video.currentTime = keep; }, { once: true });
    }

    _fail(message) {
      this.shadowRoot.innerHTML =
        `${TEMPLATE.match(/<style>[\s\S]*<\/style>/)[0]}` +
        `<p class="error"></p>`;
      this.shadowRoot.querySelector(".error").textContent = message;
      this.dispatchEvent(new CustomEvent("omv-error", { detail: { message } }));
    }
  }

  customElements.define("omv-player", OmvPlayer);
})();
