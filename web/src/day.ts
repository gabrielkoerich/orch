import type { TemplateResult } from "lit";
import { html, nothing } from "lit";
import type { Landed } from "./api";
import { api } from "./api";
import { duration, parseTs } from "./format";
import { Page } from "./page";
import { addDays, menuButton, shortRepo, startOfDay, taskLine } from "./shared";

// What merged on one local day, grouped by project, then what got stuck that day
export class OrchDay extends Page {
  static override properties = {
    ...Page.properties,
    day: { state: true },
    landed: { state: true },
    blocked: { state: true },
  };

  declare day: Date;
  declare landed: Landed[];
  declare blocked: Landed[];

  constructor() {
    super();
    this.day = startOfDay(new Date());
    this.landed = [];
    this.blocked = [];
  }

  protected async load(): Promise<void> {
    const d = await api.day(this.day, addDays(this.day, 1));
    this.landed = d.landed;
    this.blocked = d.blocked;
  }

  private go(n: number): void {
    this.day = n === 0 ? startOfDay(new Date()) : addDays(this.day, n);
    this.loaded = false;
    void this.reload();
  }

  private row(l: Landed): TemplateResult {
    const c = l.card;
    const cycle = (parseTs(l.at) - parseTs(c.created_at)) / 1000;
    return html`<div class="day-row">
      ${taskLine(c, { at: l.at })}
      <div class="day-row-meta muted">
        ${cycle > 0 ? html`<span title="From the issue to the merge">${duration(cycle)} to merge</span>` : nothing}
        <span>${c.review_cycles ? `${c.review_cycles} review ${c.review_cycles === 1 ? "round" : "rounds"}` : "approved on the first review"}</span>
        ${c.attempts > 1 ? html`<span>${c.attempts} attempts</span>` : nothing}
        ${c.model ? html`<span>${c.model}</span>` : nothing}
      </div>
    </div>`;
  }

  override render(): TemplateResult {
    const isToday = this.day.getTime() === startOfDay(new Date()).getTime();
    const groups = new Map<string, Landed[]>();
    for (const l of this.landed) {
      groups.set(l.card.repo, [...(groups.get(l.card.repo) ?? []), l]);
    }

    const agents = new Map<string, number>();
    for (const l of this.landed) {
      const a = l.card.agent ?? "no agent";
      agents.set(a, (agents.get(a) ?? 0) + 1);
    }

    return html`
      <header class="pane-head">${menuButton()}<h1>Day report</h1>
        <span class="muted">${this.day.toLocaleDateString(undefined, { weekday: "long", month: "long", day: "numeric", year: "numeric" })}</span>
        <span class="topbar-spacer"></span>
        <div class="seg">
          <button title="Previous day" @click=${(): void => this.go(-1)}>←</button>
          <button class=${isToday ? "on" : ""} @click=${(): void => this.go(0)}>Today</button>
          <button title="Next day" ?disabled=${isToday} @click=${(): void => this.go(1)}>→</button>
        </div>
      </header>
      <div class="page">
        ${this.error ? html`<div class="error-bar">${this.error}</div>` : nothing}
        <div class="day-summary">
          <span><b>${this.landed.length}</b> merged</span>
          <span><b>${groups.size}</b> ${groups.size === 1 ? "project" : "projects"}</span>
          <span><b>${this.blocked.length}</b> blocked</span>
          ${[...agents].sort((a, b) => b[1] - a[1]).map(([a, n]) => html`<span class="chip">${a} ${n}</span>`)}
        </div>
        ${
          this.loaded
            ? this.landed.length > 0
              ? [...groups].map(
                  ([repo, list]) => html`<section class="block">
                  <header class="block-head"><h2>${shortRepo(repo)}</h2><span class="count">${list.length}</span></header>
                  ${list.map((l) => this.row(l))}
                </section>`,
                )
              : html`<p class="block-empty">Nothing merged ${isToday ? "yet today" : "on this day"}.</p>`
            : html`<p class="block-empty">Loading…</p>`
        }
        ${
          this.blocked.length > 0
            ? html`<section class="block">
              <header class="block-head"><span class="dot dot-blocked"></span><h2>Got stuck</h2><span class="count">${this.blocked.length}</span></header>
              <p class="block-note muted">Tasks that blocked on this day. The ones still blocked are in the Inbox.</p>
              ${this.blocked.map((l) => taskLine(l.card, { repo: true, reason: true, at: l.at }))}
            </section>`
            : nothing
        }
      </div>
    `;
  }
}

customElements.define("orch-day", OrchDay);
