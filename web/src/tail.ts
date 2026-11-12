import { LitElement, html } from "lit";
import { stripAnsi } from "./format";

const MAX_LINES = 3000;

// A live text stream from one EventSource, kept scrolled to the end unless the user scrolls up
export class OrchTail extends LitElement {
  static properties = {
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

  createRenderRoot() {
    return this;
  }

  disconnectedCallback() {
    super.disconnectedCallback();
    this.close();
  }

  updated(changed: Map<string, unknown>) {
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

  private close() {
    this.source?.close();
    this.source = null;
  }

  private open() {
    this.close();
    this.lines = [];
    if (!this.src) {
      return;
    }

    const source = new EventSource(this.src);
    source.onopen = () => {
      this.connected = true;
    };
    source.onerror = () => {
      this.connected = false;
    };
    source.addEventListener("attempt", () => {
      if (this.lines.length) {
        this.push("── new attempt ──");
      }
    });
    source.addEventListener("line", (e) => {
      this.push(stripAnsi((e as MessageEvent<string>).data));
    });
    this.source = source;
  }

  private push(line: string) {
    const next = this.lines.concat(line.split("\n"));
    this.lines = next.length > MAX_LINES ? next.slice(-MAX_LINES) : next;
  }

  private onScroll(e: Event) {
    const pre = e.target as HTMLElement;
    this.follow = pre.scrollHeight - pre.scrollTop - pre.clientHeight < 40;
  }

  render() {
    const needle = this.filter.toLowerCase();
    const shown = needle
      ? this.lines.filter((l) => l.toLowerCase().includes(needle))
      : this.lines;
    return html`<pre class="tail" @scroll=${this.onScroll}>${shown.length
      ? shown.join("\n")
      : this.connected
        ? "Waiting for output…"
        : "Connecting…"}</pre>`;
  }
}

customElements.define("orch-tail", OrchTail);
