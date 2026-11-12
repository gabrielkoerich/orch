import type { TemplateResult } from "lit";
import { html, nothing } from "lit";
import type { DirectiveResult } from "lit/directive.js";
import type { RefDirective } from "lit/directives/ref.js";
import { ref } from "lit/directives/ref.js";
import type { Card } from "./api";
import { ago, BOARD_COLUMNS, clock, statusLabel } from "./format";

// Pages refetch on this, fired once per burst of task events
export const REFRESH = "orch:refresh";

export function shortRepo(repo: string): string {
  return repo.split("/")[1] ?? repo;
}

export function taskHref(c: Card): string {
  return `#/${c.repo}/${c.id}`;
}

export function taskRef(c: Card): string {
  return c.number ? `#${c.number}` : `T-${c.id}`;
}

export function isWorking(c: Card): boolean {
  return c.status === "in_progress" || c.status === "in_review";
}

export function startOfDay(d: Date): Date {
  return new Date(d.getFullYear(), d.getMonth(), d.getDate());
}

export function addDays(d: Date, n: number): Date {
  return new Date(d.getFullYear(), d.getMonth(), d.getDate() + n);
}

// One segment per stage, sized by its share of the open work
export function stageBar(counts: [string, number][]): TemplateResult {
  const of = (s: string): number => counts.find(([k]) => k === s)?.[1] ?? 0;
  const stages = ["blocked", ...BOARD_COLUMNS.filter((s) => s !== "done")];
  const total = stages.reduce((n, s) => n + of(s), 0);
  if (!total) {
    return html`<div class="stagebar stagebar-empty"></div>`;
  }

  return html`<div class="stagebar" title=${stages
    .filter((s) => of(s))
    .map((s) => `${statusLabel(s)} ${of(s)}`)
    .join(" · ")}>
    ${stages.filter((s) => of(s)).map((s) => html`<span class="seg-${s}" ${setStyle({ flex: String(of(s)) })}></span>`)}
  </div>`;
}

export function stageLegend(counts: [string, number][]): TemplateResult {
  const of = (s: string): number => counts.find(([k]) => k === s)?.[1] ?? 0;
  const stages = ["blocked", ...BOARD_COLUMNS];
  return html`<div class="legend">
    ${stages.filter((s) => of(s)).map((s) => html`<span><span class="dot dot-${s}"></span>${of(s)} ${statusLabel(s).toLowerCase()}</span>`)}
  </div>`;
}

// A one-line task row used by Home, Inbox and Day report
export function taskLine(
  c: Card,
  opts: { repo?: boolean; reason?: boolean; at?: string | undefined } = {},
): TemplateResult {
  const reason = c.block_reason || c.last_error;
  return html`
    <a class="tline" href=${taskHref(c)}>
      <span class="dot dot-${c.status}" title=${statusLabel(c.status)}></span>
      ${opts.repo ? html`<span class="tline-repo muted">${shortRepo(c.repo)}</span>` : nothing}
      <span class="tline-ref mono muted">${taskRef(c)}</span>
      <span class="tline-main">
        <span class="tline-title">${c.title}</span>
        ${opts.reason && reason ? html`<span class="tline-reason">${reason}</span>` : nothing}
      </span>
      ${c.agent ? html`<span class="chip">${c.agent}</span>` : nothing}
      ${c.pr_number ? html`<span class="chip chip-pr">PR #${c.pr_number}</span>` : nothing}
      <span class="tline-time muted" title=${clock(opts.at ?? c.updated_at)}>${ago(opts.at ?? c.updated_at)}</span>
    </a>
  `;
}

export const MENU = "orch:menu";

// Shown on phones only, where the sidebar is a drawer
export function menuButton(): TemplateResult {
  return html`<button class="icon menu-btn" title="Menu" @click=${(): boolean => window.dispatchEvent(new Event(MENU))}>☰</button>`;
}

// The page's policy refuses style attributes, so sizes are set on the element's style object
export function setStyle(props: Record<string, string>): DirectiveResult<typeof RefDirective> {
  return ref((el) => {
    if (el instanceof HTMLElement) {
      for (const [k, v] of Object.entries(props)) {
        el.style.setProperty(k, v);
      }
    }
  });
}
