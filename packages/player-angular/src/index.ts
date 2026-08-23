/**
 * Angular wrapper for <omv-player> (design §5.3 tier 2).
 *
 * A thin standalone component over the one Web Component codebase: it loads
 * /player-assets/omv-player.js from the OMV server (once per page), creates
 * the element, and maps inputs/outputs to idiomatic Angular — consumers never
 * need CUSTOM_ELEMENTS_SCHEMA.
 *
 *   <omv-player-ng [server]="omvServer" [studyId]="uid" [token]="token"
 *                  (frame)="onFrame($event)"></omv-player-ng>
 */
import {
  Component,
  ElementRef,
  EventEmitter,
  Input,
  NgZone,
  OnChanges,
  OnDestroy,
  OnInit,
  Output,
  SimpleChanges,
} from "@angular/core";

export interface OmvFrameEvent {
  frame: number;
  frames: number;
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

type OmvElement = HTMLElement & {
  step?: (delta: number) => void;
  gotoFrame?: (frame: number) => void;
};

@Component({
  selector: "omv-player-ng",
  standalone: true,
  template: "",
  styles: [":host { display: block; }"],
})
export class OmvPlayerComponent implements OnInit, OnChanges, OnDestroy {
  /** OMV API origin, e.g. https://omv.example.org */
  @Input({ required: true }) server!: string;
  /** DICOM StudyInstanceUID. */
  @Input({ required: true }) studyId!: string;
  /** Playback token from GET /v1/studies/{uid}. */
  @Input({ required: true }) token!: string;

  @Output() ready = new EventEmitter<{ studyUid: string }>();
  @Output() error = new EventEmitter<{ message: string }>();
  @Output() frame = new EventEmitter<OmvFrameEvent>();

  private el: OmvElement | null = null;
  private listeners: Array<[string, EventListener]> = [];

  constructor(private host: ElementRef<HTMLElement>, private zone: NgZone) {}

  ngOnInit(): void {
    ensureOmvPlayer(this.server)
      .then(() => this.mount())
      .catch((e) => this.zone.run(() => this.error.emit({ message: String(e) })));
  }

  ngOnChanges(changes: SimpleChanges): void {
    if (!this.el) return;
    if (changes["server"]) this.el.setAttribute("server", this.server);
    if (changes["studyId"]) this.el.setAttribute("study-id", this.studyId);
    if (changes["token"]) this.el.setAttribute("token", this.token);
  }

  ngOnDestroy(): void {
    for (const [name, fn] of this.listeners) this.el?.removeEventListener(name, fn);
    this.el?.remove();
  }

  /** Step ±n frames (pauses playback). */
  step(delta: number): void {
    this.el?.step?.(delta);
  }

  /** Jump to a 1-based frame number (pauses playback). */
  gotoFrame(frameNumber: number): void {
    this.el?.gotoFrame?.(frameNumber);
  }

  private mount(): void {
    // The custom element runs outside Angular's zone; re-enter it so event
    // emissions trigger change detection in the consumer.
    const el = document.createElement("omv-player") as OmvElement;
    el.setAttribute("server", this.server);
    el.setAttribute("study-id", this.studyId);
    el.setAttribute("token", this.token);
    el.style.height = "100%";
    const forward = <T>(name: string, emitter: EventEmitter<T>) => {
      const fn = (e: Event) =>
        this.zone.run(() => emitter.emit((e as CustomEvent).detail as T));
      el.addEventListener(name, fn);
      this.listeners.push([name, fn]);
    };
    forward("omv-ready", this.ready);
    forward("omv-error", this.error);
    forward("omv-frame", this.frame);
    this.host.nativeElement.appendChild(el);
    this.el = el;
  }
}
