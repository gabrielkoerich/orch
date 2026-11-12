import type { TemplateResult } from "lit";
import { html, nothing } from "lit";
import type { MergedEvent, Stats } from "./api";
import { api } from "./api";
import { duration, parseTs, pct, quantile } from "./format";
import { Page } from "./page";
import { menuButton, setStyle, shortRepo, startOfDay } from "./shared";

const RANGES: [number, string][] = [
  [1, "24 hours"],
  [7, "7 days"],
  [30, "30 days"],
  [90, "90 days"],
];
const DAYS = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

function cycleSecs(m: MergedEvent): number {
  return (parseTs(m.at) - parseTs(m.created_at)) / 1000;
}

// First try means one attempt and no review sent it back
function firstTry(m: MergedEvent): boolean {
  return m.attempts <= 1 && m.review_cycles === 0;
}

// What orch merged over a window and how often a person had to step in, counted in the page
export class OrchStats extends Page {
  static override properties = {
    ...Page.properties,
    days: { state: true },
    stats: { state: true },
  };

  declare days: number;
  declare stats: Stats | null;

  constructor() {
    super();
    this.days = Number(localStorage.getItem("orch.statsDays")) || 7;
    this.stats = null;
  }

  protected async load(): Promise<void> {
    this.stats = await api.stats(this.days);
  }

  private pick(days: number): void {
    this.days = days;
    localStorage.setItem("orch.statsDays", String(days));
    void this.reload();
  }

  // One bar per day, or per hour for the 24 hour range
  private series(stamps: string[]): { label: string; n: number }[] {
    const hourly = this.days === 1;
    const now = Date.now();
    const slots = hourly ? 24 : this.days;
    const step = hourly ? 3_600_000 : 86_400_000;
    const start = hourly ? now - 24 * step : startOfDay(new Date(now - (slots - 1) * step)).getTime();
    const out = Array.from({ length: slots }, (_, i) => {
      const t = new Date(start + i * step);
      return {
        label: hourly
          ? t.toLocaleTimeString(undefined, { hour: "2-digit" })
          : t.toLocaleDateString(undefined, { month: "short", day: "numeric" }),
        n: 0,
      };
    });
    for (const s of stamps) {
      const i = Math.floor((parseTs(s) - start) / step);
      const slot = out[i];
      if (i >= 0 && i < slots && slot) {
        slot.n += 1;
      }
    }

    return out;
  }

  private chart(title: string, stamps: string[], kind: string): TemplateResult {
    const series = this.series(stamps);
    const max = Math.max(1, ...series.map((s) => s.n));
    return html`<section class="block">
      <header class="block-head"><h2>${title}</h2><span class="count">${stamps.length}</span></header>
      <div class="bars bars-${kind}">
        ${series.map((s) => html`<span class="bar" ${setStyle({ height: `${(s.n / max) * 100}%` })} title="${s.label}: ${s.n}"></span>`)}
      </div>
      <div class="bars-axis muted"><span>${series[0]?.label}</span><span>${series.at(-1)?.label}</span></div>
    </section>`;
  }

  private heatmap(merged: MergedEvent[]): TemplateResult {
    const grid = Array.from({ length: 7 }, () => new Array<number>(24).fill(0));
    for (const m of merged) {
      const d = new Date(parseTs(m.at));
      const row = grid[(d.getDay() + 6) % 7];
      if (row) {
        row[d.getHours()] = (row[d.getHours()] ?? 0) + 1;
      }
    }

    const max = Math.max(1, ...grid.flat());
    let best: [number, number, number] = [0, 0, 0];
    for (const [d, row] of grid.entries()) {
      for (const [h, n] of row.entries()) {
        if (n > best[2]) {
          best = [d, h, n];
        }
      }
    }
    return html`<section class="block">
      <header class="block-head"><h2>When work lands</h2></header>
      <div class="heat">
        ${grid.map((row, d) => html`<span class="heat-day muted">${DAYS[d]}</span>${row.map((n, h) => html`<span class="heat-cell" ${setStyle({ opacity: String(n ? 0.2 + (0.8 * n) / max : 1) })} data-on=${n > 0} title="${DAYS[d]} ${h}:00 · ${n} merged"></span>`)}`)}
      </div>
      ${best[2] ? html`<p class="block-note muted">The busiest hour is ${DAYS[best[0]]} ${best[1]}:00, with ${best[2]} merged.</p>` : nothing}
    </section>`;
  }

  override render(): TemplateResult {
    const s = this.stats;
    const merged = s?.merged ?? [];
    const cycles = merged
      .map(cycleSecs)
      .filter((x) => x >= 0)
      .sort((a, b) => a - b);
    const first = merged.filter(firstTry).length;
    const rounds = merged.reduce((n, m) => n + m.review_cycles, 0);
    const agentRuns = (s?.runs ?? []).filter((r) => r.run_type === "agent");
    const failed = agentRuns.filter((r) => r.outcome && r.outcome !== "success").length;

    const agents = new Map<string, MergedEvent[]>();
    for (const m of merged) {
      agents.set(m.agent ?? "no agent", [...(agents.get(m.agent ?? "no agent") ?? []), m]);
    }

    const outcomes = new Map<string, number>();
    for (const r of agentRuns) {
      const o = r.outcome || "running";
      outcomes.set(o, (outcomes.get(o) ?? 0) + 1);
    }

    return html`
      <header class="pane-head">${menuButton()}<h1>Statistics</h1>
        <span class="topbar-spacer"></span>
        <div class="seg">
          ${RANGES.map(([d, label]) => html`<button class=${this.days === d ? "on" : ""} @click=${(): void => this.pick(d)}>${label}</button>`)}
        </div>
      </header>
      <div class="page">
        ${this.error ? html`<div class="error-bar">${this.error}</div>` : nothing}
        <div class="tiles">
          <div class="tile"><span class="tile-n">${merged.length}</span><span class="tile-l">merged</span></div>
          <div class="tile"><span class="tile-n">${duration(quantile(cycles, 0.5) || 0) || "–"}</span>
            <span class="tile-l">issue to merge, p50 · p90 ${duration(quantile(cycles, 0.9) || 0) || "–"}</span></div>
          <div class="tile"><span class="tile-n">${pct(first, merged.length)}</span>
            <span class="tile-l">first try · ${first}/${merged.length}</span></div>
          <div class="tile"><span class="tile-n">${merged.length > 0 ? (rounds / merged.length).toFixed(1) : "–"}</span>
            <span class="tile-l">review rounds per merge</span></div>
          <div class="tile ${s?.blocked.length ? "tile-alert" : ""}"><span class="tile-n">${s?.blocked.length ?? 0}</span>
            <span class="tile-l">times a task blocked</span></div>
          <div class="tile"><span class="tile-n">${pct(failed, agentRuns.length)}</span>
            <span class="tile-l">runs that failed · ${failed}/${agentRuns.length}</span></div>
        </div>

        <div class="home-grid">
          ${this.chart(
            "Merged",
            merged.map((m) => m.at),
            "merged",
          )}
          ${this.chart(
            "Blocked",
            (s?.blocked ?? []).map((b) => b.at),
            "blocked",
          )}
        </div>

        ${this.heatmap(merged)}

        <div class="home-grid">
          <section class="block">
            <header class="block-head"><h2>By agent</h2></header>
            <table class="table">
              <thead><tr><th>Agent</th><th>Merged</th><th>First try</th><th>p50 to merge</th><th>Review rounds</th></tr></thead>
              <tbody>
                ${[...agents]
                  .sort((a, b) => b[1].length - a[1].length)
                  .map(([agent, list]) => {
                    const c = list
                      .map(cycleSecs)
                      .filter((x) => x >= 0)
                      .sort((a, b) => a - b);
                    const f = list.filter(firstTry).length;
                    return html`<tr><td>${agent}</td><td>${list.length}</td><td>${pct(f, list.length)} <span class="muted">${f}/${list.length}</span></td>
                    <td>${duration(quantile(c, 0.5) || 0) || "–"}</td><td>${(list.reduce((n, m) => n + m.review_cycles, 0) / list.length).toFixed(1)}</td></tr>`;
                  })}
              </tbody>
            </table>
          </section>
          <section class="block">
            <header class="block-head"><h2>How agent runs ended</h2><span class="count">${agentRuns.length}</span></header>
            ${[...outcomes]
              .sort((a, b) => b[1] - a[1])
              .map(
                ([o, n]) => html`<div class="hbar"><span class="hbar-label">${o.replace(/_/gu, " ")}</span>
              <span class="hbar-track hbar-${o === "success" ? "ok" : "bad"}"><span ${setStyle({ width: `${(n / Math.max(1, agentRuns.length)) * 100}%` })}></span></span>
              <span class="hbar-n mono">${n}</span></div>`,
              )}
          </section>
        </div>

        <section class="block">
          <header class="block-head"><h2>By project</h2></header>
          <table class="table">
            <thead><tr><th>Project</th><th>Merged</th><th>First try</th><th>Blocked</th></tr></thead>
            <tbody>
              ${[...new Set([...merged.map((m) => m.repo), ...(s?.blocked ?? []).map((b) => b.repo)])].map((repo) => {
                const list = merged.filter((m) => m.repo === repo);
                const f = list.filter(firstTry).length;
                return html`<tr><td><a href="#/${repo}">${shortRepo(repo)}</a></td><td>${list.length}</td>
                  <td>${pct(f, list.length)} <span class="muted">${f}/${list.length}</span></td>
                  <td>${(s?.blocked ?? []).filter((b) => b.repo === repo).length}</td></tr>`;
              })}
            </tbody>
          </table>
        </section>
      </div>
    `;
  }
}

customElements.define("orch-stats", OrchStats);
