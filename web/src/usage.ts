import type { TemplateResult } from "lit";
import { html, nothing } from "lit";
import type { Stats, Usage } from "./api";
import { api } from "./api";
import { duration } from "./format";
import { Page } from "./page";
import { menuButton, setStyle, shortRepo } from "./shared";

type WindowRow = Usage["windows"][number];

// "five_hour" is 5 hours long and "seven_day_opus" 7 days, so the name carries the length
function windowSecs(name: string): number | null {
  const m = /(\d+|one|five|seven)_(hour|day)/u.exec(name);
  if (!m) {
    return null;
  }

  const words: Record<string, number> = { one: 1, five: 5, seven: 7 };
  const word = m[1] ?? "";
  const n = words[word] ?? Number(word);
  return n * (m[2] === "hour" ? 3600 : 86_400);
}

function windowLabel(name: string): string {
  const secs = windowSecs(name);
  const rest = name.replace(/^(\d+|one|five|seven)_(hour|day)_?/u, "").replace(/_/gu, " ");
  const base = secs === 18_000 ? "5-hour" : secs === 604_800 ? "Week" : name.replace(/_/gu, " ");
  return rest && secs ? `${base} · ${rest}` : base;
}

function resetText(at: number | null): string {
  if (!at) {
    return "";
  }

  const left = at - Date.now() / 1000;
  if (left <= 0) {
    return "reset";
  }

  const when = new Date(at * 1000).toLocaleString(undefined, { weekday: "short", hour: "2-digit", minute: "2-digit" });
  return `resets in ${duration(left)}, ${when}`;
}

// Plan windows per agent with the agent's cooldowns beside them, and where the agents' time went
export class OrchUsage extends Page {
  static override properties = {
    ...Page.properties,
    usage: { state: true },
    week: { state: true },
  };

  declare usage: Usage;
  declare week: Stats | null;

  constructor() {
    super();
    this.usage = { windows: [], cooldowns: [] };
    this.week = null;
  }

  protected async load(): Promise<void> {
    [this.usage, this.week] = await Promise.all([api.usage(), api.stats(7)]);
  }

  private renderWindow(w: WindowRow): TemplateResult {
    const used = w.utilization === null ? null : Math.min(1, Math.max(0, w.utilization));
    const len = windowSecs(w.window);
    // How far into the window we are, so use above that line runs out before the reset
    const elapsed = len && w.resets_at ? 1 - Math.max(0, w.resets_at - Date.now() / 1000) / len : null;
    const ahead = used !== null && elapsed !== null && elapsed > 0.03 && used > elapsed + 0.05;
    const level = w.status === "rejected" ? "full" : used !== null && used >= 0.8 ? "high" : "ok";
    return html`
      <div class="window">
        <div class="window-head">
          <span>${windowLabel(w.window)}</span>
          ${w.status === "allowed" ? nothing : html`<span class="chip chip-${w.status === "rejected" ? "bad" : "warn"}">${w.status.replace(/_/gu, " ")}</span>`}
          <span class="topbar-spacer"></span>
          <b>${used === null ? "–" : `${Math.round(used * 100)}%`}</b>
        </div>
        <div class="meter meter-${level}">
          <span ${setStyle({ width: `${(used ?? 0) * 100}%` })}></span>
          ${elapsed !== null && elapsed > 0 && elapsed < 1 ? html`<i ${setStyle({ left: `${elapsed * 100}%` })} title="Time passed in this window"></i>` : nothing}
        </div>
        <div class="window-foot muted">
          <span>${resetText(w.resets_at)}</span>
          ${ahead ? html`<span class="warn-text">ahead of pace, may run out before the reset</span>` : nothing}
        </div>
      </div>
    `;
  }

  override render(): TemplateResult {
    const now = Date.now() / 1000;
    // Every agent that reported a window, is cooling down or ran this week
    const agents = [
      ...new Set([
        ...this.usage.windows.map((w) => w.agent),
        ...this.usage.cooldowns.map((c) => c.key.split(":")[0]),
        ...(this.week?.runs ?? []).map((r) => r.agent ?? "").filter(Boolean),
      ]),
    ].sort();
    const runs = this.week?.runs ?? [];
    const byAgent = new Map<string, number>();
    const byRepo = new Map<string, number>();
    for (const r of runs) {
      byAgent.set(r.agent ?? "unknown", (byAgent.get(r.agent ?? "unknown") ?? 0) + r.duration_secs);
      byRepo.set(r.repo, (byRepo.get(r.repo) ?? 0) + r.duration_secs);
    }

    const most = Math.max(1, ...byRepo.values(), ...byAgent.values());
    const bars = (m: Map<string, number>, label: (k: string) => string): TemplateResult[] =>
      [...m]
        .sort((a, b) => b[1] - a[1])
        .map(
          ([k, secs]) => html`<div class="hbar"><span class="hbar-label">${label(k)}</span>
          <span class="hbar-track"><span ${setStyle({ width: `${(secs / most) * 100}%` })}></span></span>
          <span class="hbar-n mono">${duration(secs)}</span></div>`,
        );

    return html`
      <header class="pane-head">${menuButton()}<h1>Usage</h1>
        <span class="muted">plan limits as each agent last reported them</span></header>
      <div class="page">
        ${this.error ? html`<div class="error-bar">${this.error}</div>` : nothing}
        ${
          agents.length > 0
            ? html`<div class="usage-grid">
              ${agents.map((agent) => {
                const windows = this.usage.windows.filter((w) => w.agent === agent);
                const cools = this.usage.cooldowns.filter((c) => c.key === agent || c.key.startsWith(`${agent}:`));
                return html`<section class="block usage-card">
                  <header class="block-head"><h2>${agent}</h2>
                    <span class="topbar-spacer"></span>
                    ${
                      cools.length > 0
                        ? html`<span class="chip chip-bad">cooling down</span>`
                        : html`<span class="chip chip-ok">routable</span>`
                    }
                  </header>
                  ${windows.length > 0 ? windows.map((w) => this.renderWindow(w)) : html`<p class="block-empty">This agent reports no plan limits to orch.</p>`}
                  ${cools.map(
                    (c) => html`<div class="cooldown">
                      <span class="mono">${c.key}</span>
                      <span class="muted">${c.reason.replace(/_/gu, " ")}</span>
                      <span class="topbar-spacer"></span>
                      <span>${duration(c.until - now)} left</span>
                    </div>`,
                  )}
                </section>`;
              })}
            </div>`
            : html`<p class="block-empty">No agent has reported a plan window, and none is cooling down.</p>`
        }

        <div class="home-grid">
          <section class="block">
            <header class="block-head"><h2>Agent time this week, by project</h2></header>
            ${byRepo.size > 0 ? bars(byRepo, shortRepo) : html`<p class="block-empty">No runs in the last 7 days.</p>`}
          </section>
          <section class="block">
            <header class="block-head"><h2>By agent</h2></header>
            ${byAgent.size > 0 ? bars(byAgent, (k) => k) : html`<p class="block-empty">No runs in the last 7 days.</p>`}
          </section>
        </div>
      </div>
    `;
  }
}

customElements.define("orch-usage", OrchUsage);
