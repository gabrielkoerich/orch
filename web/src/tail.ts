import type { TemplateResult } from "lit";
import { html, LitElement } from "lit";
import { stripAnsi } from "./format";

const MAX_LINES = 3000;

// A live text stream from one EventSource, kept scrolled to the end unless the user scrolls up
export class OrchTail extends LitElement {
  static override properties = {
    src: { type: String },
    filter: { type: String },
    lines: { state: true },
    connected: { state: true },
  };

  declare src: string;
  declare filter: string;
  declare lines: string[];
  declare connected: boolean;
  private source: EventSource | null = null;
  private follow = true;

  constructor() {
    super();
    this.src = "";
    this.filter = "";
    this.lines = [];
    this.connected = false;
  }

  override createRenderRoot(): HTMLElement {
    return this;
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    this.close();
  }

  override updated(changed: Map<string, unknown>): void {
    if (changed.has("src")) {
      this.open();
    }

    if (this.follow) {
      const pre = this.querySelector("pre");
      if (pre) {
        pre.scrollTop = pre.scrollHeight;
      }
    }
  }

  private close(): void {
    this.source?.close();
    this.source = null;
  }

  private open(): void {
    this.close();
    this.lines = [];
    if (!this.src) {
      return;
    }

    const source = new EventSource(this.src);
    source.onopen = (): void => {
      this.connected = true;
    };
    source.onerror = (): void => {
      this.connected = false;
    };
    source.addEventListener("attempt", () => {
      if (this.lines.length > 0) {
        this.push("── new attempt ──");
      }
    });
    source.addEventListener("line", (e) => {
      this.push(stripAnsi((e as MessageEvent<string>).data));
    });
    this.source = source;
  }

  private push(line: string): void {
    const next = this.lines.concat(line.split("\n"));
    this.lines = next.length > MAX_LINES ? next.slice(-MAX_LINES) : next;
  }

  private onScroll(e: Event): void {
    const pre = e.target as HTMLElement;
    this.follow = pre.scrollHeight - pre.scrollTop - pre.clientHeight < 40;
  }

  override render(): TemplateResult {
    const needle = this.filter.toLowerCase();
    const shown = needle ? this.lines.filter((l) => l.toLowerCase().includes(needle)) : this.lines;
    return html`<pre class="tail" @scroll=${this.onScroll}>${
      shown.length > 0 ? shown.join("\n") : this.connected ? "Waiting for output…" : "Connecting…"
    }</pre>`;
  }
}

customElements.define("orch-tail", OrchTail);
