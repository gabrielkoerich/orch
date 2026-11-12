export type Status =
  | "new"
  | "routed"
  | "in_progress"
  | "needs_review"
  | "in_review"
  | "approved"
  | "done"
  | "blocked"
  | "waiting";

export interface Project {
  repo: string;
  counts: [string, number][];
}

export interface Card {
  id: number;
  repo: string;
  number: string | null;
  title: string;
  status: Status;
  agent: string | null;
  model: string | null;
  complexity: string;
  url: string;
  pr_number: number | null;
  pr_url: string | null;
  labels: string[];
  block_reason: string | null;
  last_error: string;
  attempts: number;
  review_cycles: number;
  created_at: string;
  updated_at: string;
  since: string | null;
}

export interface Activity {
  timestamp: string;
  event_type: string;
  from_status: string | null;
  to_status: string | null;
  agent: string | null;
  model: string | null;
}

export interface Run {
  attempt: number;
  run_type: string;
  agent: string;
  model: string;
  outcome: string;
  error: string;
  duration_secs: number;
  started_at: string;
  completed_at: string | null;
  input_tokens: number;
  output_tokens: number;
}

export interface Detail {
  card: Card;
  body: string;
  summary: string;
  branch: string;
  activity: Activity[];
  runs: Run[];
}

export interface PrState {
  number: number;
  title: string;
  state: string;
  merged: boolean;
  mergeable: boolean | null;
  mergeable_state: string | null;
  html_url: string;
  head: string;
  base: string;
  checks: { name: string; status: string; conclusion: string | null }[];
  reviews: { login: string; state: string; submitted_at: string }[];
}

export interface Usage {
  windows: { agent: string; window: string; status: string; utilization: number | null; resets_at: number | null }[];
  cooldowns: { key: string; until: number; reason: string }[];
}

export interface MergedEvent {
  at: string;
  task_id: number;
  repo: string;
  agent: string | null;
  created_at: string;
  attempts: number;
  review_cycles: number;
}

export interface RunEvent {
  at: string | null;
  repo: string;
  agent: string | null;
  model: string | null;
  run_type: string | null;
  outcome: string | null;
  duration_secs: number;
}

export interface Stats {
  merged: MergedEvent[];
  blocked: { at: string; task_id: number; repo: string }[];
  runs: RunEvent[];
}

export interface Day {
  landed: Landed[];
  blocked: Landed[];
}

export interface Inbox {
  blocked: Card[];
  human: Card[];
}

export interface Landed {
  at: string;
  card: Card;
}

export interface SetupStatus {
  gh: "missing" | "logged_out" | "ready";
  agents: [string, "missing" | "logged_out" | "ready"][];
  config_exists: boolean;
  login: string | null;
  projects: number;
}

export interface LoginState {
  state: "" | "waiting" | "done" | "failed";
  code: string | null;
  url: string;
  error: string | null;
}

export interface RemoteRepo {
  repo: string;
  description: string | null;
  private: boolean;
  pushed_at: string | null;
  added: boolean;
  checkout: string | null;
}

export interface SetupRepos {
  github: RemoteRepo[];
  local: { path: string; repo: string; added: boolean }[];
}

export interface Dirs {
  path: string;
  parent: string | null;
  dirs: { name: string; path: string; repo: string | null }[];
}

export type AddProject =
  | { kind: "clone"; repo: string; path: string }
  | { kind: "existing"; path: string }
  | { kind: "create"; owner: string; name: string; description: string; private: boolean };

export interface NewTask {
  repo: string;
  title: string;
  body: string;
  // null lets the router pick, "none" keeps the task for a person
  agent: string | null;
  depends_on: number[];
}

export interface PlanState {
  messages: { role: string; content: string; created_at: string }[];
  draft: string;
  name: string;
}

export interface PlanPreview {
  path: string;
  outline: string;
  report: string;
}

export interface ConfigFile {
  repo: string;
  path: string;
  text: string;
  version: string;
}

export interface FeedItem {
  kind: "status" | "output";
  timestamp: string;
  repo: string;
  task: number | null;
  number: string | null;
  title: string;
  from: string | null;
  to: string | null;
  agent: string | null;
  text: string;
}

export interface ChatMessage {
  role: string;
  content: string;
  agent: string | null;
  model: string | null;
  created_at: string;
}

async function get<T>(path: string): Promise<T> {
  const res = await fetch(path, { headers: { Accept: "application/json" } });
  if (!res.ok) {
    throw new Error(`${res.status} ${await res.text()}`);
  }

  return (await res.json()) as T;
}

const q: typeof encodeURIComponent = encodeURIComponent;

async function send<T>(method: string, path: string, body?: unknown): Promise<T> {
  const res = await fetch(path, {
    method,
    headers: { "Content-Type": "application/json" },
    body: body === undefined ? null : JSON.stringify(body),
  });
  if (!res.ok) {
    throw new Error(await res.text());
  }

  return (await res.json()) as T;
}

export const api = {
  projects: (): Promise<Project[]> => get<Project[]>("/api/projects"),
  tasks: (repo: string): Promise<Card[]> => get<Card[]>(`/api/tasks?repo=${q(repo)}`),
  running: (): Promise<Card[]> => get<Card[]>("/api/running"),
  inbox: (repo = ""): Promise<Inbox> => get<Inbox>(`/api/inbox?repo=${q(repo)}`),
  pr: async (id: number): Promise<PrState | null> => {
    const res = await fetch(`/api/pr?id=${id}`);
    if (res.status === 204) {
      return null;
    }

    if (!res.ok) {
      throw new Error(`GitHub: ${await res.text()}`);
    }

    return (await res.json()) as PrState;
  },
  usage: (): Promise<Usage> => get<Usage>("/api/usage"),
  agents: (): Promise<string[]> => get<string[]>("/api/agents"),
  plan: (repo: string): Promise<PlanState> => get<PlanState>(`/api/plan?repo=${q(repo)}`),
  planSay: (repo: string, message: string): Promise<{ reply: string; draft: string }> =>
    send<{ reply: string; draft: string }>("POST", "/api/plan/message", { repo, message }),
  planPreview: (repo: string, name: string): Promise<PlanPreview> =>
    send<PlanPreview>("POST", "/api/plan/preview", { repo, name }),
  planImport: (repo: string, name: string): Promise<{ report: string }> =>
    send<{ report: string }>("POST", "/api/plan/import", { repo, name }),
  planClear: async (repo: string): Promise<void> => {
    const res = await fetch("/api/plan/clear", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ repo }),
    });
    if (!res.ok) {
      throw new Error(await res.text());
    }
  },
  createTask: (req: NewTask): Promise<{ id: number; number: string | null }> =>
    send<{ id: number; number: string | null }>("POST", "/api/tasks/new", req),
  setup: (): Promise<SetupStatus> => get<SetupStatus>("/api/setup"),
  loginState: (): Promise<LoginState> => get<LoginState>("/api/setup/login"),
  login: (): Promise<LoginState> => send<LoginState>("POST", "/api/setup/login"),
  setupRepos: (): Promise<SetupRepos> => get<SetupRepos>("/api/setup/repos"),
  owners: (): Promise<string[]> => get<string[]>("/api/setup/owners"),
  dirs: (path = ""): Promise<Dirs> => get<Dirs>(`/api/setup/dirs?path=${q(path)}`),
  addProject: (req: AddProject): Promise<{ repo: string }> => send<{ repo: string }>("POST", "/api/projects/add", req),
  stats: (days: number): Promise<Stats> => get<Stats>(`/api/stats?days=${days}`),
  day: (from: Date, to: Date, repo = ""): Promise<Day> =>
    get<Day>(`/api/day?from=${q(from.toISOString())}&to=${q(to.toISOString())}&repo=${q(repo)}`),
  task: (id: number): Promise<Detail> => get<Detail>(`/api/task?id=${id}`),
  config: (repo: string): Promise<ConfigFile> => get<ConfigFile>(`/api/config?repo=${q(repo)}`),
  async saveConfig(repo: string, text: string, version: string): Promise<ConfigFile> {
    const res = await fetch("/api/config", {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ repo, text, version }),
    });
    if (!res.ok) {
      throw new Error(await res.text());
    }

    return (await res.json()) as ConfigFile;
  },
  chat: (repo: string): Promise<ChatMessage[]> => get<ChatMessage[]>(`/api/chat?repo=${q(repo)}`),
  async send(repo: string, message: string): Promise<ChatMessage> {
    const res = await fetch("/api/chat", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ repo, message }),
    });
    if (!res.ok) {
      throw new Error(`${res.status} ${await res.text()}`);
    }

    return (await res.json()) as ChatMessage;
  },
  events: (repo: string): EventSource => new EventSource(`/api/events?repo=${q(repo)}`),
  output: (id: number): EventSource => new EventSource(`/api/output?id=${id}`),
  logs: (): EventSource => new EventSource("/api/logs"),
  activity: (repo: string): EventSource => new EventSource(`/api/activity?repo=${q(repo)}`),
};
