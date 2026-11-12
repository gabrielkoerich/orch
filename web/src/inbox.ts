import type { TemplateResult } from "lit";
import { html, nothing } from "lit";
import type { Card, Inbox } from "./api";
import { api } from "./api";
import { Page } from "./page";
import { menuButton, shortRepo, taskLine } from "./shared";

// Everything that waits on a person, grouped by why
export class OrchInbox extends Page {
  static override properties = {
    ...Page.properties,
    inbox: { state: true },
    repo: { state: true },
  };

  declare inbox: Inbox;
  declare repo: string;

  constructor() {
    super();
    this.inbox = { blocked: [], human: [] };
    this.repo = "";
  }

  protected async load(): Promise<void> {
    this.inbox = await api.inbox();
  }

  private only(cards: Card[]): Card[] {
    return this.repo ? cards.filter((c) => c.repo === this.repo) : cards;
  }

  override render(): TemplateResult {
    const repos = [...new Set([...this.inbox.blocked, ...this.inbox.human].map((c) => c.repo))].sort();
    const blocked = this.only(this.inbox.blocked);
    const human = this.only(this.inbox.human);
    return html`
      <header class="pane-head">${menuButton()}<h1>Inbox</h1>
        <span class="muted">${blocked.length + human.length} waiting on you</span>
        <span class="topbar-spacer"></span>
        ${
          repos.length > 1
            ? html`<div class="seg">
              <button class=${this.repo ? "" : "on"} @click=${(): void => {
                this.repo = "";
              }}>All</button>
              ${repos.map(
                (r) =>
                  html`<button class=${this.repo === r ? "on" : ""} @click=${(): void => {
                    this.repo = r;
                  }}>${shortRepo(r)}</button>`,
              )}
            </div>`
            : nothing
        }
      </header>
      <div class="page">
        ${this.error ? html`<div class="error-bar">${this.error}</div>` : nothing}
        ${
          this.loaded
            ? blocked.length + human.length === 0
              ? html`<div class="inbox-zero"><span class="inbox-zero-mark">✓</span><p>Nothing waits on you. orch keeps working on the rest.</p></div>`
              : nothing
            : html`<p class="block-empty">Loading…</p>`
        }
        ${
          blocked.length > 0
            ? html`<section class="block">
              <header class="block-head"><span class="dot dot-blocked"></span><h2>Blocked</h2><span class="count">${blocked.length}</span></header>
              <p class="block-note muted">orch stopped on these, longest wait first. Fix what the reason names, then run <code>orch task unblock</code> or <code>orch task retry</code>.</p>
              ${blocked.map((c) => taskLine(c, { repo: true, reason: true, at: c.since ?? undefined }))}
            </section>`
            : nothing
        }
        ${
          human.length > 0
            ? html`<section class="block">
              <header class="block-head"><span class="dot dot-waiting"></span><h2>For you to do</h2><span class="count">${human.length}</span></header>
              <p class="block-note muted">Marked <code>no-agent</code> or <code>owner:human</code>, so no agent picks them up.</p>
              ${human.map((c) => taskLine(c, { repo: true, at: c.since ?? undefined }))}
            </section>`
            : nothing
        }
      </div>
    `;
  }
}

customElements.define("orch-inbox", OrchInbox);
