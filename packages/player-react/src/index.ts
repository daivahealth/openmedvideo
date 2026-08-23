/**
 * React wrapper for <omv-player> (design §5.3 tier 2).
 *
 * A thin shim over the one Web Component codebase: it loads
 * /player-assets/omv-player.js from the OMV server (once per page), renders
 * the element, and maps props/events to idiomatic React.
 *
 *   <OmvPlayer server="https://omv..." studyId="1.2.840..." token={token}
 *              onFrame={({frame, frames}) => ...} />
 */
import {
  createElement,
  forwardRef,
  useEffect,
  useImperativeHandle,
  useRef,
  useState,
  type CSSProperties,
} from "react";

export interface OmvFrameEvent {
  frame: number;
  frames: number;
}

export interface OmvPlayerProps {
  /** OMV API origin, e.g. https://omv.example.org */
  server: string;
  /** DICOM StudyInstanceUID. */
  studyId: string;
  /** Playback token from GET /v1/studies/{uid}. */
  token: string;
  onReady?: (e: { studyUid: string }) => void;
  onError?: (e: { message: string }) => void;
  onFrame?: (e: OmvFrameEvent) => void;
  className?: string;
  style?: CSSProperties;
}

export interface OmvPlayerHandle {
  /** Step ±n frames (pauses playback). */
  step: (delta: number) => void;
  /** Jump to a 1-based frame number (pauses playback). */
  gotoFrame: (frame: number) => void;
}

const loaded = new Map<string, Promise<void>>();

/** Loads the component script from the OMV server exactly once per origin. */
export function ensureOmvPlayer(server: string): Promise<void> {
  const origin = server.replace(/\/$/, "");
  if (typeof customElements !== "undefined" && customElements.get("omv-player")) {
    return Promise.resolve();
  }
  let p = loaded.get(origin);
  if (!p) {
    p = new Promise<void>((resolve, reject) => {
      const s = document.createElement("script");
      s.src = `${origin}/player-assets/omv-player.js`;
      s.onload = () => resolve();
      s.onerror = () => reject(new Error(`failed to load omv-player.js from ${origin}`));
      document.head.appendChild(s);
    });
    loaded.set(origin, p);
  }
  return p;
}

type OmvElement = HTMLElement & OmvPlayerHandle;

export const OmvPlayer = forwardRef<OmvPlayerHandle, OmvPlayerProps>(
  function OmvPlayer({ server, studyId, token, onReady, onError, onFrame, className, style }, ref) {
    const el = useRef<OmvElement | null>(null);
    const [defined, setDefined] = useState(false);

    useEffect(() => {
      let cancelled = false;
      ensureOmvPlayer(server)
        .then(() => !cancelled && setDefined(true))
        .catch((e) => onError?.({ message: String(e) }));
      return () => {
        cancelled = true;
      };
      // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [server]);

    useEffect(() => {
      const node = el.current;
      if (!node) return;
      const ready = (e: Event) => onReady?.((e as CustomEvent).detail);
      const error = (e: Event) => onError?.((e as CustomEvent).detail);
      const frame = (e: Event) => onFrame?.((e as CustomEvent).detail);
      node.addEventListener("omv-ready", ready);
      node.addEventListener("omv-error", error);
      node.addEventListener("omv-frame", frame);
      return () => {
        node.removeEventListener("omv-ready", ready);
        node.removeEventListener("omv-error", error);
        node.removeEventListener("omv-frame", frame);
      };
    }, [defined, onReady, onError, onFrame]);

    useImperativeHandle(ref, () => ({
      step: (d: number) => el.current?.step?.(d),
      gotoFrame: (f: number) => el.current?.gotoFrame?.(f),
    }));

    if (!defined) return null;
    return createElement("omv-player", {
      ref: el,
      server,
      "study-id": studyId,
      token,
      class: className,
      style,
    });
  }
);
