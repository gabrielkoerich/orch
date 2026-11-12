import type { Status } from "./api";

export const BOARD_COLUMNS: Status[] = [
  "new",
  "routed",
  "in_progress",
  "needs_review",
  "in_review",
  "approved",
  "done",
];

// Not stages of the flow, so they sit above the board
export const ATTENTION: Status[] = ["blocked", "waiting"];

export const LIST_ORDER: Status[] = [...ATTENTION, ...BOARD_COLUMNS];

const LABELS: Record<Status, string> = {
  new: "New",
  routed: "Routed",
  in_progress: "In progress",
  needs_review: "Needs review",
  in_review: "In review",
  approved: "Approved",
  done: "Done",
  blocked: "Blocked",
  waiting: "Waiting",
};

export function statusLabel(s: string): string {
  return LABELS[s as Status] ?? s;
}

// SQLite writes UTC without a zone, so mark it as UTC before parsing
function parse(ts: string): number {
  if (!ts) {
    return Number.NaN;
  }

  const iso = ts.includes("T") ? ts : ts.replace(" ", "T");
  return Date.parse(/[zZ]|[+-]\d\d:?\d\d$/u.test(iso) ? iso : `${iso}Z`);
}

export function ago(ts: string): string {
  const t = parse(ts);
  if (Number.isNaN(t)) {
    return "";
  }

  const s = Math.max(0, (Date.now() - t) / 1000);
  if (s < 60) {
    return "now";
  }

  if (s < 3600) {
    return `${Math.floor(s / 60)}m`;
  }

  if (s < 86_400) {
    return `${Math.floor(s / 3600)}h`;
  }

  return `${Math.floor(s / 86_400)}d`;
}

export function clock(ts: string): string {
  const t = parse(ts);
  if (Number.isNaN(t)) {
    return ts;
  }

  return new Date(t).toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

// A terminal colour code starts with ESC, built from its code so the pattern holds no control character
const ANSI = new RegExp(`${String.fromCharCode(27)}\\[[0-9;]*[A-Za-z]`, "gu");

export function stripAnsi(s: string): string {
  return s.replace(ANSI, "");
}

export function initials(repo: string): string {
  const name = repo.split("/").pop() ?? repo;
  return name.slice(0, 2).toUpperCase();
}

export function parseTs(ts: string): number {
  return parse(ts);
}

export function duration(secs: number): string {
  if (!Number.isFinite(secs) || secs < 0) {
    return "";
  }

  if (secs < 60) {
    return `${Math.round(secs)}s`;
  }

  if (secs < 3600) {
    return `${Math.round(secs / 60)}m`;
  }

  if (secs < 86_400) {
    const h = Math.floor(secs / 3600);
    const m = Math.round((secs % 3600) / 60);
    return m ? `${h}h ${m}m` : `${h}h`;
  }

  const d = Math.floor(secs / 86_400);
  const h = Math.round((secs % 86_400) / 3600);
  return h ? `${d}d ${h}h` : `${d}d`;
}

export function quantile(sorted: number[], q: number): number {
  if (sorted.length === 0) {
    return Number.NaN;
  }

  const i = Math.min(sorted.length - 1, Math.floor(q * sorted.length));
  return sorted[i] ?? Number.NaN;
}

export function pct(n: number, of: number): string {
  return of ? `${Math.round((n / of) * 100)}%` : "–";
}
