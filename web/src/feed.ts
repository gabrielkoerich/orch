import { LitElement, html, nothing } from "lit";
import { api } from "./api";
import type { FeedItem } from "./api";
import { clock, statusLabel, stripAnsi } from "./format";

const MAX_ITEMS = 400;

// Status changes and agent output for one project, or every project when repo is empty
export class OrchFeed extends LitElement {
  static properties = {
    repo: { type: String },
    showRepo: { type: Boolean },
    items: { state: true },
  };

  declare repo: string;
  declare showRepo: boolean;
  declare items: FeedItem[];
  private source: EventSource | null = null;
  private follow = true;

  constructor() {
    super();
    this.repo = "";
    this.showRepo = false;
    this.items = [];
  }

  createRenderRoot() {
    return this;
  }

  disconnectedCallback() {
    super.disconnectedCallback();
    this.source?.close();
    this.source = null;
  }

  updated(changed: Map<string, unknown>) {
    if (changed.has("repo")) {
      this.open();
    }

    if (this.follow) {
      const log = this.querySelector(".feed");
      if (log) {
        log.scrollTop = log.scrollHeight;
      }
    }
  }

  private open() {
    this.source?.close();
    this.items = [];
    const source = api.activity(this.repo);
    source.addEventListener("item", (e) => {
      const item = JSON.parse((e as MessageEvent<string>).data) as FeedItem;
      const next = [...this.items, item];
      this.items = next.length > MAX_ITEMS ? next.slice(-MAX_ITEMS) : next;
    });
    this.source = source;
  }

  private onScroll(e: Event) {
    const el = e.target as HTMLElement;
    this.follow = el.scrollHeight - el.scrollTop - el.clientHeight < 40;
  }

  private ref(i: FeedItem) {
    const label = i.number ? `#${i.number}` : i.task !== null ? `T-${i.task}` : "";
    const name = html`${this.showRepo ? html`<span class="muted">${i.repo.split("/")[1] ?? i.repo} </span>` : nothing}${label}`;
    return i.task !== null
      ? html`<a class="feed-ref mono" href="#/${i.repo}/${i.task}" title=${i.title}>${name}</a>`
      : html`<span class="feed-ref mono">${name}</span>`;
  }

  render() {
    return html`
      <ol class="feed" @scroll=${this.onScroll}>
        ${this.items.length
          ? this.items.map((i) =>
              i.kind === "status"
                ? html`<li class="feed-item feed-status">
                    <span class="feed-time muted">${clock(i.timestamp)}</span>
                    ${this.ref(i)}
                    <span class="dot dot-${i.to ?? "new"}"></span>
                    <span class="feed-text">
                      ${i.from ? html`<span class="muted">${statusLabel(i.from)} →</span> ` : nothing}${statusLabel(i.to ?? "")}
                      <span class="muted">· ${i.title}</span>
                      ${i.text ? html`<span class="feed-error">${i.text}</span>` : nothing}
                    </span>
                  </li>`
                : html`<li class="feed-item feed-output">
                    <span class="feed-time muted">${clock(i.timestamp)}</span>
                    ${this.ref(i)}
                    <span class="feed-text mono">${stripAnsi(i.text)}</span>
                  </li>`,
            )
          : html`<li class="empty">Waiting for activity…</li>`}
      </ol>
    `;
  }
}

customElements.define("orch-feed", OrchFeed);
