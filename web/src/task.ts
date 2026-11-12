import type { TemplateResult } from "lit";
import { html, nothing } from "lit";
import type { Activity, Card, Detail, PrState } from "./api";
import { clock, duration, parseTs, statusLabel } from "./format";
import { setStyle } from "./shared";

// orch's flow as a person reads it. Waiting and blocked are detours, not stages
const STAGES: { label: string; statuses: string[] }[] = [
  { label: "Queued", statuses: ["new"] },
  { label: "Routed", statuses: ["routed"] },
  { label: "Building", statuses: ["in_progress"] },
  { label: "Review", statuses: ["needs_review", "in_review"] },
  { label: "Approved", statuses: ["approved"] },
  { label: "Merged", statuses: ["done"] },
];

// The furthest stage the task reached, so a blocked task shows where it stopped
function reached(c: Card, activity: Activity[]): number {
  const seen = new Set([c.status, ...activity.flatMap((a) => [a.from_status ?? "", a.to_status ?? ""])]);
  if (c.status === "done") {
    return STAGES.length - 1;
  }

  const current = STAGES.findIndex((s) => s.statuses.includes(c.status));
  if (current >= 0) {
    return current;
  }

  let last = 0;
  STAGES.forEach((s, i) => {
    if (s.statuses.some((x) => seen.has(x))) {
      last = i;
    }
  });
  return last;
}

export function stepper(d: Detail): TemplateResult {
  const c = d.card;
  const at = reached(c, d.activity);
  const off = c.status === "blocked" || c.status === "waiting";
  return html`<ol class="stepper">
    ${STAGES.map(
      (
        s,
        i,
      ) => html`<li class="step ${i < at || c.status === "done" ? "step-done" : ""} ${i === at && c.status !== "done" ? (off ? "step-stuck" : "step-now") : ""}">
      <span class="step-bar"></span>
      <span class="step-label">${s.label}</span>
      ${i === at && off ? html`<span class="step-note">${statusLabel(c.status)}</span>` : nothing}
    </li>`,
    )}
  </ol>`;
}

// Seconds spent in each status, from consecutive activity rows up to now
export function timeSplit(d: Detail): { working: number; waiting: number; total: number } {
  const rows = d.activity.filter((a) => a.to_status);
  let working = 0;
  let waiting = 0;
  rows.forEach((a, i) => {
    const start = parseTs(a.timestamp);
    const next = rows[i + 1];
    const end = next ? parseTs(next.timestamp) : d.card.status === "done" ? start : Date.now();
    const secs = Math.max(0, (end - start) / 1000);
    if (a.to_status === "in_progress" || a.to_status === "in_review") {
      working += secs;
    } else if (a.to_status !== "done") {
      waiting += secs;
    }
  });
  return { working, waiting, total: working + waiting };
}

interface Row {
  ok: boolean | null;
  label: string;
  detail: string;
}

// Each row says what orch does about it next, so a person knows whether to act
export function mergeChecklist(
  c: Card,
  pr: PrState | null | undefined,
  prError: string,
): TemplateResult | typeof nothing {
  if (!c.pr_number) {
    return nothing;
  }

  if (pr === undefined) {
    return html`<div class="checklist muted">${prError || "Reading the PR from GitHub…"}</div>`;
  }

  if (pr === null) {
    return nothing;
  }

  const finished = pr.checks.filter((x) => x.status === "completed");
  const failed = finished.filter((x) => !["success", "neutral", "skipped"].includes(x.conclusion ?? ""));
  const pending = pr.checks.length - finished.length;
  const approved = pr.reviews.some((r) => r.state === "APPROVED") || c.status === "approved" || c.status === "done";
  const changes = pr.reviews.some((r) => r.state === "CHANGES_REQUESTED");
  const state = pr.mergeable_state ?? "";
  const rows: Row[] = [
    pr.checks.length === 0
      ? { ok: null, label: "CI", detail: "No checks on this PR. orch reviews after 15 minutes without one" }
      : failed.length > 0
        ? {
            ok: false,
            label: "CI",
            detail: `${failed.length} of ${pr.checks.length} failed · orch sends the task back to its agent`,
          }
        : pending
          ? {
              ok: null,
              label: "CI",
              detail: `${finished.length} of ${pr.checks.length} done · orch waits for the rest`,
            }
          : { ok: true, label: "CI", detail: `${pr.checks.length} of ${pr.checks.length} green` },
    changes
      ? {
          ok: false,
          label: "Review",
          detail: `Changes requested · round ${c.review_cycles}, the agent fixes them on this PR`,
        }
      : approved
        ? { ok: true, label: "Review", detail: "Approved" }
        : {
            ok: null,
            label: "Review",
            detail:
              c.status === "in_review"
                ? "The review agent is reading the PR"
                : "Waits for green CI, then a review agent reads the PR",
          },
    pr.mergeable === false
      ? {
          ok: false,
          label: "Mergeable",
          detail: "Conflicts with the base · orch rebases in the worktree, or the agent solves it",
        }
      : state === "behind"
        ? { ok: null, label: "Mergeable", detail: "Behind the base · orch rebases before it merges" }
        : pr.mergeable === null
          ? { ok: null, label: "Mergeable", detail: "GitHub is still working it out" }
          : { ok: true, label: "Mergeable", detail: `Merges cleanly into ${pr.base}` },
  ];
  const ready = rows.every((r) => r.ok);
  return html`<div class="checklist">
    <div class="checklist-head">${pr.merged ? "Merged" : ready ? "Ready to merge · orch merges it on the next pass" : "Before it merges"}</div>
    ${rows.map(
      (r) => html`<div class="check-row">
      <span class="check-mark ${r.ok === true ? "is-ok" : r.ok === false ? "is-bad" : "is-wait"}">${r.ok === true ? "✓" : r.ok === false ? "✕" : "…"}</span>
      <span class="check-label">${r.label}</span>
      <span class="check-detail muted">${r.detail}</span>
    </div>`,
    )}
  </div>`;
}

export function prCard(pr: PrState | null | undefined): TemplateResult | typeof nothing {
  if (!pr) {
    return nothing;
  }

  const status = pr.merged ? "merged" : pr.state;
  return html`<section class="pr-card">
    <div class="pr-head">
      <span class="chip chip-${status === "merged" ? "merged" : status === "open" ? "ok" : "bad"}">${status}</span>
      <a href=${pr.html_url} target="_blank" rel="noopener" class="pr-title">#${pr.number} ${pr.title}</a>
    </div>
    <div class="muted mono pr-branch">${pr.head} → ${pr.base}</div>
    ${
      pr.checks.length > 0
        ? html`<ul class="pr-checks">
          ${pr.checks.map((x) => {
            const ok = x.status === "completed" && ["success", "neutral", "skipped"].includes(x.conclusion ?? "");
            const bad = x.status === "completed" && !ok;
            return html`<li><span class="check-mark ${ok ? "is-ok" : bad ? "is-bad" : "is-wait"}">${ok ? "✓" : bad ? "✕" : "…"}</span>
              <span>${x.name}</span><span class="muted">${x.conclusion ?? x.status.replace(/_/gu, " ")}</span></li>`;
          })}
        </ul>`
        : nothing
    }
    ${
      pr.reviews.length > 0
        ? html`<div class="pr-reviews">${pr.reviews.map((r) => html`<span class="chip">${r.login} · ${r.state.toLowerCase().replace(/_/gu, " ")}</span>`)}</div>`
        : nothing
    }
  </section>`;
}

export function details(d: Detail): TemplateResult {
  const c = d.card;
  const t = timeSplit(d);
  const rows: [string, unknown][] = [
    ["Agent", c.agent ?? "not routed yet"],
    ["Model", c.model ?? "the agent's default"],
    ["Complexity", c.complexity || "–"],
    ["Branch", d.branch ? html`<span class="mono">${d.branch}</span>` : "–"],
    ["Attempts", String(c.attempts)],
    ["Review rounds", String(c.review_cycles)],
    ["Created", clock(c.created_at)],
  ];
  return html`<div class="details">
    ${
      t.total
        ? html`<div class="time-split">
          <div class="time-split-head"><b>${duration(t.total)}</b><span class="muted">total</span></div>
          <div class="meter meter-split"><span ${setStyle({ width: `${(t.working / t.total) * 100}%` })}></span></div>
          <div class="time-split-foot muted"><span>${duration(t.working)} working</span><span>${duration(t.waiting)} waiting</span></div>
        </div>`
        : nothing
    }
    <dl>${rows.map(([k, v]) => html`<dt>${k}</dt><dd>${v}</dd>`)}</dl>
  </div>`;
}

export function runs(d: Detail): TemplateResult | typeof nothing {
  if (d.runs.length === 0) {
    return nothing;
  }

  return html`<h3>Runs</h3>
    <table class="table runs">
      <thead><tr><th>#</th><th>Kind</th><th>Agent</th><th>Ended</th><th>Took</th></tr></thead>
      <tbody>
        ${d.runs.map(
          (r) => html`<tr title=${r.error}>
          <td>${r.attempt}</td><td>${r.run_type}</td>
          <td>${r.agent}${r.model ? html`<span class="muted"> · ${r.model}</span>` : nothing}</td>
          <td class=${r.outcome && r.outcome !== "success" ? "is-bad-text" : ""}>${r.outcome || (r.completed_at ? "–" : "running")}</td>
          <td>${duration(r.duration_secs)}</td>
        </tr>`,
        )}
      </tbody>
    </table>`;
}
