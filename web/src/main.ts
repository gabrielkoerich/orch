import type { TemplateResult } from "lit";
import { html, LitElement, nothing } from "lit";
import type { AddMode } from "./add";
import { ADDED, CLOSE_ADD } from "./add";
import type { Card, ChatMessage, Detail, Project, PrState, Status } from "./api";
import { api } from "./api";
import { ATTENTION, ago, BOARD_COLUMNS, clock, initials, LIST_ORDER, statusLabel } from "./format";
import { OPEN_ADD, openAdd } from "./welcome";
import "./compose";
import "./plan";
import "./config";
import "./day";
import "./home";
import "./inbox";
import "./stats";
import "./usage";
import { MENU, menuButton, REFRESH, stageBar, stageLegend, taskLine } from "./shared";
import { details, mergeChecklist, prCard, runs, stepper } from "./task";
import "./feed";
import "./tail";

type View = "overview" | "board" | "list" | "plan";
type Side = "chat" | "activity" | "config";
type TaskTab = "overview" | "output" | "timeline";
type Theme = "system" | "light" | "dark";

type Page = "home" | "inbox" | "day" | "usage" | "stats" | "logs" | "activity" | "settings";

interface Route {
  repo: string | null;
  task: number | null;
  page: Page | null;
  compose?: boolean;
}

const PAGES: Page[] = ["home", "inbox", "day", "usage", "stats", "logs", "activity", "settings"];

// #/owner/repo, #/owner/repo/42, or a page such as #/logs
function parseRoute(): Route {
  const parts = location.hash.replace(/^#\/?/u, "").split("/").filter(Boolean);
  if (PAGES.includes(parts[0] as Page)) {
    return { repo: null, task: null, page: parts[0] as Page };
  }

  if (parts.length === 0) {
    return { repo: null, task: null, page: "home" };
  }

  const repo = parts.length >= 2 ? `${parts[0]}/${parts[1]}` : null;
  if (repo && parts[2] === "new") {
    return { repo, task: null, page: null, compose: true };
  }

  const task = parts.length >= 3 ? Number(parts[2]) : null;
  return { repo, task: task !== null && Number.isFinite(task) ? task : null, page: null };
}

function taskHref(repo: string, id: number): string {
  return `#/${repo}/${id}`;
}

function countOf(p: Project, s: string): number {
  return p.counts.find(([k]) => k === s)?.[1] ?? 0;
}

function shortRepo(repo: string): string {
  return repo.split("/")[1] ?? repo;
}

function applyTheme(theme: Theme): void {
  if (theme === "system") {
    document.documentElement.removeAttribute("data-theme");
  } else {
    document.documentElement.setAttribute("data-theme", theme);
  }
}

function matches(c: Card, needle: string): boolean {
  if (!needle) {
    return true;
  }

  const hay = [c.title, c.number ? `#${c.number}` : "", c.agent ?? "", c.model ?? "", ...c.labels]
    .join(" ")
    .toLowerCase();
  return hay.includes(needle);
}

export class OrchApp extends LitElement {
  static override properties = {
    projects: { state: true },
    running: { state: true },
    route: { state: true },
    cards: { state: true },
    view: { state: true },
    side: { state: true },
    taskTab: { state: true },
    detail: { state: true },
    pr: { state: true },
    prError: { state: true },
    needsYou: { state: true },
    addMode: { state: true },
    addMenu: { state: true },
    projectsLoaded: { state: true },
    messages: { state: true },
    sending: { state: true },
    error: { state: true },
    live: { state: true },
    filter: { state: true },
    logFilter: { state: true },
    theme: { state: true },
    navOpen: { state: true },
  };

  declare projects: Project[];
  declare running: Card[];
  declare route: Route;
  declare cards: Card[];
  declare view: View;
  declare side: Side | null;
  declare taskTab: TaskTab;
  declare detail: Detail | null;
  // undefined while it loads, null when the task has no PR
  declare pr: PrState | null | undefined;
  declare prError: string;
  declare needsYou: number;
  declare addMode: AddMode | null;
  declare addMenu: boolean;
  declare projectsLoaded: boolean;
  private prFor = 0;
  private prAt = 0;
  declare messages: ChatMessage[];
  declare sending: boolean;
  declare error: string;
  declare live: boolean;
  declare filter: string;
  declare logFilter: string;
  declare theme: Theme;
  declare navOpen: boolean;

  private events: EventSource | null = null;
  private refetch = 0;
  private ticker = 0;
  private draft = "";

  constructor() {
    super();
    this.projects = [];
    this.running = [];
    this.route = parseRoute();
    this.cards = [];
    this.view = (localStorage.getItem("orch.view") as View) || "overview";
    this.side = null;
    this.taskTab = "overview";
    this.detail = null;
    this.pr = undefined;
    this.prError = "";
    this.needsYou = 0;
    this.addMode = null;
    this.addMenu = false;
    this.projectsLoaded = false;
    this.messages = [];
    this.sending = false;
    this.error = "";
    this.live = false;
    this.filter = "";
    this.logFilter = "";
    this.theme = (localStorage.getItem("orch.theme") as Theme) || "system";
    this.navOpen = false;
    applyTheme(this.theme);
  }

  override createRenderRoot(): HTMLElement {
    return this;
  }

  override connectedCallback(): void {
    super.connectedCallback();
    window.addEventListener("hashchange", this.onRoute);
    window.addEventListener("keydown", this.onKey);
    window.addEventListener(MENU, this.onMenu);
    window.addEventListener(OPEN_ADD, this.onOpenAdd);
    window.addEventListener(CLOSE_ADD, this.onCloseAdd);
    window.addEventListener(ADDED, this.onAdded);
    this.watch();
    // Relative times on cards go stale without an event, so redraw once a minute
    this.ticker = window.setInterval(() => this.requestUpdate(), 60_000);
    void Promise.all([this.loadProjects(), this.loadRunning(), this.loadNeedsYou()]).then(() => this.onRoute());
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    window.removeEventListener("hashchange", this.onRoute);
    window.removeEventListener("keydown", this.onKey);
    window.removeEventListener(MENU, this.onMenu);
    window.removeEventListener(OPEN_ADD, this.onOpenAdd);
    window.removeEventListener(CLOSE_ADD, this.onCloseAdd);
    window.removeEventListener(ADDED, this.onAdded);
    window.clearInterval(this.ticker);
    this.events?.close();
  }

  private readonly onOpenAdd = (e: Event): void => {
    this.addMenu = false;
    this.addMode = (e as CustomEvent<AddMode>).detail;
  };

  private readonly onCloseAdd = (): void => {
    this.addMode = null;
  };

  private readonly onAdded = (e: Event): void => {
    this.addMode = null;
    const repo = (e as CustomEvent<string>).detail;
    void this.loadProjects().then(() => {
      location.hash = `#/${repo}`;
    });
  };

  private readonly onMenu = (): void => {
    this.navOpen = true;
  };

  private readonly onKey = (e: KeyboardEvent): void => {
    if (e.key !== "Escape") {
      return;
    }

    if (this.navOpen) {
      this.navOpen = false;
    } else if (this.route.task !== null) {
      this.closeTask();
    } else if (this.side) {
      this.side = null;
    }
  };

  private readonly onRoute = (): void => {
    const prev = this.route;
    this.route = parseRoute();
    this.navOpen = false;

    if (this.route.repo !== prev.repo || this.cards.length === 0) {
      this.cards = [];
      this.messages = [];
      this.filter = "";
      if (this.route.repo) {
        void this.loadTasks();
        if (this.side) {
          void this.loadSide(this.side);
        }
      }
    }

    if (this.route.task === null) {
      this.detail = null;
    } else {
      if (prev.task !== this.route.task) {
        this.taskTab = "overview";
        this.detail = null;
      }

      void this.loadDetail(this.route.task);
    }
  };

  // One stream for every project: a task event refreshes the board and the agents list
  private watch(): void {
    const source = api.events("");
    source.onopen = (): void => {
      this.live = true;
    };
    source.onerror = (): void => {
      this.live = false;
    };
    const refresh = (): void => this.scheduleRefresh();
    source.addEventListener("task", refresh);
    source.addEventListener("lagged", refresh);
    this.events = source;
  }

  private scheduleRefresh(): void {
    window.clearTimeout(this.refetch);
    this.refetch = window.setTimeout(() => {
      void this.loadProjects();
      void this.loadRunning();
      void this.loadNeedsYou();
      window.dispatchEvent(new Event(REFRESH));
      if (this.route.repo) {
        void this.loadTasks();
      }

      if (this.route.task !== null) {
        void this.loadDetail(this.route.task);
      }
    }, 300);
  }

  private async run<T>(work: () => Promise<T>): Promise<T | undefined> {
    try {
      const out = await work();
      this.error = "";
      return out;
    } catch (e) {
      this.error = e instanceof Error ? e.message : String(e);
      return undefined;
    }
  }

  private async loadProjects(): Promise<void> {
    const projects = await this.run(() => api.projects());
    if (projects) {
      this.projects = projects;
      this.projectsLoaded = true;
    }
  }

  private async loadRunning(): Promise<void> {
    const running = await this.run(() => api.running());
    if (running) {
      this.running = running;
    }
  }

  private async loadTasks(): Promise<void> {
    const repo = this.route.repo;
    if (!repo) {
      return;
    }

    const cards = await this.run(() => api.tasks(repo));
    if (cards && repo === this.route.repo) {
      this.cards = cards;
    }
  }

  private async loadDetail(id: number): Promise<void> {
    const detail = await this.run(() => api.task(id));
    if (detail && this.route.task === id) {
      this.detail = detail;
      void this.loadPr(detail);
    }
  }

  private async loadNeedsYou(): Promise<void> {
    const inbox = await this.run(() => api.inbox());
    if (inbox) {
      this.needsYou = inbox.blocked.length + inbox.human.length;
    }
  }

  // GitHub is read on open and then at most every 20 seconds, events arrive faster than that
  private async loadPr(d: Detail): Promise<void> {
    const id = d.card.id;
    if (!d.card.pr_number) {
      this.pr = null;
      return;
    }

    if (this.prFor === id && Date.now() - this.prAt < 20_000) {
      return;
    }

    if (this.prFor !== id) {
      this.pr = undefined;
    }

    this.prFor = id;
    this.prAt = Date.now();
    try {
      const pr = await api.pr(id);
      if (this.route.task === id) {
        this.pr = pr;
        this.prError = "";
      }
    } catch (e) {
      this.prError = e instanceof Error ? e.message : String(e);
    }
  }

  private async loadSide(side: Side): Promise<void> {
    const repo = this.route.repo;
    if (!repo) {
      return;
    }

    if (side === "chat") {
      this.messages = (await this.run(() => api.chat(repo))) ?? [];
      this.scrollChat();
    }
  }

  private toggleSide(side: Side): void {
    if (this.side === side && this.route.task === null) {
      this.side = null;
      return;
    }

    this.side = side;
    if (this.route.task !== null) {
      this.closeTask();
    }

    void this.loadSide(side);
  }

  private closeTask(): void {
    if (this.route.repo) {
      location.hash = `#/${this.route.repo}`;
    }
  }

  private askAbout(d: Detail): void {
    const ref = d.card.number ? `#${d.card.number}` : `internal task ${d.card.id}`;
    this.draft = `About ${ref} "${d.card.title}": `;
    this.side = "chat";
    this.closeTask();
    void this.loadSide("chat").then(() =>
      this.updateComplete.then(() => {
        const input = this.querySelector<HTMLTextAreaElement>(".chat-input");
        if (input) {
          input.value = this.draft;
          input.focus();
        }
      }),
    );
  }

  private setView(view: View): void {
    this.view = view;
    localStorage.setItem("orch.view", view);
  }

  private setTheme(theme: Theme): void {
    this.theme = theme;
    localStorage.setItem("orch.theme", theme);
    applyTheme(theme);
  }

  private scrollChat(): void {
    void this.updateComplete.then(() => {
      const log = this.querySelector(".chat-log");
      if (log) {
        log.scrollTop = log.scrollHeight;
      }
    });
  }

  private async sendChat(): Promise<void> {
    const repo = this.route.repo;
    const text = this.draft.trim();
    if (!(repo && text) || this.sending) {
      return;
    }

    this.sending = true;
    this.messages = [...this.messages, { role: "user", content: text, agent: null, model: null, created_at: "" }];
    this.draft = "";
    const input = this.querySelector<HTMLTextAreaElement>(".chat-input");
    if (input) {
      input.value = "";
    }

    this.scrollChat();
    const reply = await this.run(() => api.send(repo, text));
    this.sending = false;
    if (reply && repo === this.route.repo) {
      this.messages = [...this.messages, reply];
      this.scrollChat();
    }
  }

  private onChatKey(e: KeyboardEvent): void {
    if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      void this.sendChat();
    }
  }

  override render(): TemplateResult {
    const paneOpen = this.route.task !== null || this.side !== null;
    return html`
      <div class="shell ${this.navOpen ? "nav-open" : ""}">
        ${this.renderSidebar()}
        <div class="nav-scrim" @click=${(): void => {
          this.navOpen = false;
        }}></div>
        <div class="workspace">
          <div class="panes ${paneOpen ? "with-side" : ""} ${this.route.task === null ? "" : "side-wide"}">
            <main class="pane">${this.renderMain()}</main>
            ${paneOpen && !this.route.page ? html`<aside class="pane side">${this.renderSide()}</aside>` : nothing}
          </div>
          ${this.renderStatusbar()}
        </div>
        ${this.addMode ? html`<orch-add .mode=${this.addMode}></orch-add>` : nothing}
      </div>
    `;
  }

  private renderMain(): TemplateResult {
    switch (this.route.page) {
      case "home":
        return this.projectsLoaded && this.projects.length === 0
          ? html`<orch-welcome class="page-host"></orch-welcome>`
          : html`<orch-home class="page-host"></orch-home>`;
      case "inbox":
        return html`<orch-inbox class="page-host"></orch-inbox>`;
      case "day":
        return html`<orch-day class="page-host"></orch-day>`;
      case "usage":
        return html`<orch-usage class="page-host"></orch-usage>`;
      case "stats":
        return html`<orch-stats class="page-host"></orch-stats>`;
      case "logs":
        return this.renderLogs();
      case "activity":
        return html`
          <header class="pane-head">${menuButton()}<span class="nav-glyph">◷</span><h1>Activity</h1>
            <span class="muted">every project</span></header>
          <orch-feed class="feed-fill" .repo=${""} .showRepo=${true}></orch-feed>
        `;
      case "settings":
        return this.renderSettingsPage();
      default:
        return this.route.compose && this.route.repo
          ? html`<orch-compose class="page-host" .repo=${this.route.repo}></orch-compose>`
          : this.renderProject();
    }
  }

  private navLink(page: Page, glyph: string, label: string, badge = 0): TemplateResult {
    return html`<a class="nav-item ${this.route.page === page ? "active" : ""}" href="#/${page}">
      <span class="nav-glyph">${glyph}</span><span class="nav-name">${label}</span>
      ${badge ? html`<span class="badge badge-blocked">${badge}</span>` : nothing}
    </a>`;
  }

  private renderSidebar(): TemplateResult {
    return html`
      <nav class="sidebar">
        <div class="brand"><span class="brand-mark">◆</span> orch</div>
        ${this.navLink("home", "⌂", "Home")}
        ${this.navLink("inbox", "◉", "Inbox", this.needsYou)}
        ${this.navLink("day", "☰", "Day report")}
        ${this.navLink("activity", "◷", "Activity")}
        ${this.navLink("stats", "▤", "Statistics")}
        ${this.navLink("usage", "◔", "Usage")}

        <div class="side-section">
          <div class="side-label side-label-row">Projects
            <button class="icon add-btn" title="Add a project" @click=${(): void => {
              this.addMenu = !this.addMenu;
            }}>＋</button>
          </div>
          ${
            this.addMenu
              ? html`<div class="add-menu">
                <button @click=${(): void => openAdd("clone")}>Clone a repository…</button>
                <button @click=${(): void => openAdd("create")}>Create a new repository…</button>
                <button @click=${(): void => openAdd("existing")}>Add an existing repository…</button>
                <button disabled title="Follow a project another machine runs, with nothing on this one">
                  Watch only…<span class="add-menu-note">comes with multi-node</span></button>
              </div>`
              : nothing
          }
          ${
            this.projects.length > 0
              ? this.projects.map((p) => {
                  const blocked = countOf(p, "blocked");
                  const active = p.repo === this.route.repo;
                  return html`
                  <a class="nav-item ${active ? "active" : ""}" href="#/${p.repo}" title=${p.repo}>
                    <span class="project-icon">${initials(p.repo)}</span>
                    <span class="nav-name">${shortRepo(p.repo)}</span>
                    ${blocked ? html`<span class="badge badge-blocked" title="${blocked} blocked">${blocked}</span>` : nothing}
                  </a>
                `;
                })
              : html`<p class="side-empty">No projects yet. Use ＋ to add one.</p>`
          }
        </div>

        <div class="side-spacer"></div>
        <div class="side-section side-agents">
          <div class="side-label">${this.running.length > 0 ? html`<span class="working">Working ${this.running.length}</span>` : "Agents"}</div>
          ${
            this.running.length > 0
              ? this.running.map(
                  (
                    c,
                  ) => html`<a class="agent-row ${this.route.task === c.id ? "active" : ""}" href=${taskHref(c.repo, c.id)} title=${c.title}>
                  <span class="spinner"></span>
                  <span class="nav-name"><b>${c.agent ?? "agent"}</b> <span class="muted">· ${shortRepo(c.repo)} ${c.number ? `#${c.number}` : `T-${c.id}`}</span></span>
                  <span class="muted mono">${ago(c.since ?? c.updated_at)}</span>
                </a>`,
                )
              : html`<p class="side-empty">No agent is working.</p>`
          }
        </div>
        ${this.navLink("logs", "≡", "Service log")}
        ${this.navLink("settings", "⚙", "Settings")}
      </nav>
    `;
  }

  private renderStatusbar(): TemplateResult {
    const p = this.projects.find((x) => x.repo === this.route.repo);
    const open = p ? p.counts.filter(([s]) => s !== "done").reduce((n, [, c]) => n + c, 0) : 0;
    return html`
      <footer class="statusbar">
        <span class="status-item" title=${this.live ? "Receiving live events from the orch service" : "Not connected to the orch service"}>
          <span class="dot ${this.live ? "dot-live" : "dot-off"}"></span>${this.live ? "Live" : "Offline"}
        </span>
        <span class="status-item wide-only">This computer</span>
        <span class="status-item">${this.running.length} ${this.running.length === 1 ? "agent" : "agents"} working</span>
        ${p ? html`<span class="status-item">${shortRepo(p.repo)} · ${open} open</span>` : nothing}
        <span class="topbar-spacer"></span>
        ${this.error ? html`<span class="status-item status-error" title=${this.error}>${this.error}</span>` : nothing}
      </footer>
    `;
  }

  private renderSettingsPage(): TemplateResult {
    const options: [Theme, string][] = [
      ["system", "System"],
      ["light", "Light"],
      ["dark", "Dark"],
    ];
    return html`
      <header class="pane-head">${menuButton()}<span class="nav-glyph">⚙</span><h1>Settings</h1></header>
      <div class="settings-page">
        <section class="settings-section">
          <h2>Appearance</h2>
          <div class="pref-row">
            <span>Theme</span>
            <div class="seg">
              ${options.map(
                ([t, label]) =>
                  html`<button class=${this.theme === t ? "on" : ""} @click=${(): void => this.setTheme(t)}>${label}</button>`,
              )}
            </div>
          </div>
        </section>
        <section class="settings-section">
          <h2>orch config</h2>
          <p class="muted">The global config every project starts from. A project's own settings are under Config on its page.</p>
          <orch-config class="config-tall" .repo=${""}></orch-config>
        </section>
        <section class="settings-section">
          <h2>Nodes</h2>
          <p class="muted">This computer runs every task. Settings for more machines come here with multi-node.</p>
        </section>
      </div>
    `;
  }

  private renderProject(): TemplateResult {
    const repo = this.route.repo;
    if (!repo) {
      return html`<div class="empty">Pick a project.</div>`;
    }

    const needle = this.filter.trim().toLowerCase();
    const cards = this.cards.filter((c) => matches(c, needle));
    return html`
      <header class="pane-head project-head">
        ${menuButton()}
        <span class="project-icon">${initials(repo)}</span>
        <h1>${shortRepo(repo)}</h1>
        <span class="muted owner">${repo.split("/")[0]}</span>
        <div class="seg">
          <button class=${this.view === "overview" ? "on" : ""} @click=${(): void => this.setView("overview")}>Overview</button>
          <button class=${this.view === "board" ? "on" : ""} @click=${(): void => this.setView("board")}>Board</button>
          <button class=${this.view === "list" ? "on" : ""} @click=${(): void => this.setView("list")}>List</button>
          <button class=${this.view === "plan" ? "on" : ""} @click=${(): void => this.setView("plan")}>Plan</button>
        </div>
        <div class="topbar-spacer"></div>
        <div class="head-tools">
        <a class="tool primary" href="#/${repo}/new">New task</a>
        <button class="tool ${this.side === "chat" && this.route.task === null ? "on" : ""}" @click=${(): void => this.toggleSide("chat")}>Chat</button>
        <button class="tool ${this.side === "activity" && this.route.task === null ? "on" : ""}" @click=${(): void => this.toggleSide("activity")}>Activity</button>
        <button class="tool ${this.side === "config" && this.route.task === null ? "on" : ""}" @click=${(): void => this.toggleSide("config")}>Config</button>
        </div>
      </header>
      ${
        this.view === "plan"
          ? nothing
          : html`<div class="filter-row">
            <input class="filter" type="search" placeholder="Filter by title, #, agent, label" .value=${this.filter}
              @input=${(e: Event): void => {
                this.filter = (e.target as HTMLInputElement).value;
              }} />
          </div>`
      }
      ${
        this.view === "plan"
          ? html`<orch-plan class="plan-host" .repo=${repo}></orch-plan>`
          : this.cards.length > 0
            ? this.view === "overview"
              ? this.renderProjectOverview(repo, cards)
              : this.view === "board"
                ? this.renderBoard(repo, cards)
                : this.renderList(repo, cards)
            : html`<div class="empty">No tasks yet. <a class="link" href="#/${repo}" @click=${(): void => this.setView("plan")}>Plan what to build</a> with an agent, <a class="link" href="#/${repo}/new">create a task</a>, or label an issue on GitHub.</div>`
      }
    `;
  }

  private renderProjectOverview(repo: string, cards: Card[]): TemplateResult {
    const p = this.projects.find((x) => x.repo === repo);
    const needs = cards.filter(
      (c) =>
        c.status === "blocked" ||
        (c.status !== "done" && c.labels.some((l) => l === "no-agent" || l === "owner:human")),
    );
    const moving = cards.filter((c) => ["in_progress", "needs_review", "in_review", "approved"].includes(c.status));
    const next = cards.filter((c) => ["routed", "new", "waiting"].includes(c.status) && !needs.includes(c));
    const landed = cards.filter((c) => c.status === "done").slice(0, 8);
    const section = (
      title: string,
      list: Card[],
      empty: string,
      opts = {},
    ): TemplateResult => html`<section class="block">
      <header class="block-head"><h2>${title}</h2><span class="count">${list.length}</span></header>
      ${list.length > 0 ? list.map((c) => taskLine(c, opts)) : html`<p class="block-empty">${empty}</p>`}
    </section>`;
    return html`<div class="page">
      ${p ? html`<div class="overview-bar">${stageBar(p.counts)}${stageLegend(p.counts)}</div>` : nothing}
      ${needs.length > 0 ? section("Needs you", needs, "", { reason: true }) : nothing}
      <div class="home-grid">
        ${section("In motion", moving, "Nothing is being built or reviewed.")}
        ${section("Up next", next, "The queue is empty.")}
      </div>
      ${section("Recently landed", landed, "Nothing merged yet.")}
    </div>`;
  }

  private group(cards: Card[]): Map<Status, Card[]> {
    const groups = new Map<Status, Card[]>();
    for (const c of cards) {
      const list = groups.get(c.status) ?? [];
      list.push(c);
      groups.set(c.status, list);
    }

    return groups;
  }

  private renderBoard(repo: string, cards: Card[]): TemplateResult {
    const groups = this.group(cards);
    const attention = ATTENTION.flatMap((s) => groups.get(s) ?? []);
    return html`
      <div class="board-wrap">
        ${
          attention.length > 0
            ? html`<section class="attention">
              <div class="attention-title"><span class="dot dot-blocked"></span>Needs attention <span class="count">${attention.length}</span></div>
              <div class="attention-row">${attention.map((c) => this.renderCard(repo, c))}</div>
            </section>`
            : nothing
        }
        <div class="board">
          ${BOARD_COLUMNS.map((s) => {
            const list = groups.get(s) ?? [];
            return html`
              <section class="column ${list.length > 0 ? "" : "column-empty"}">
                <header class="column-head">
                  <span class="dot dot-${s}"></span>
                  <span class="column-name">${statusLabel(s)}</span>
                  <span class="count">${list.length}</span>
                </header>
                <div class="column-body">${list.map((c) => this.renderCard(repo, c))}</div>
              </section>
            `;
          })}
        </div>
      </div>
    `;
  }

  private renderLinks(c: Card): TemplateResult {
    return html`
      ${c.pr_url ? html`<a class="chip chip-pr" href=${c.pr_url} target="_blank" rel="noopener">PR #${c.pr_number}</a>` : nothing}
    `;
  }

  private renderTag(c: Card): TemplateResult {
    const label = c.number ? `#${c.number}` : `T-${c.id}`;
    return c.url && c.number
      ? html`<a class="tag mono tag-link" href=${c.url} target="_blank" rel="noopener" title="Open the issue on GitHub">${label}</a>`
      : html`<span class="tag mono">${label}</span>`;
  }

  private renderCard(repo: string, c: Card): TemplateResult {
    const selected = this.route.task === c.id;
    const working = c.status === "in_progress" || c.status === "in_review";
    return html`
      <article class="card ${selected ? "selected" : ""} card-${c.status}">
        <div class="card-top">
          ${this.renderTag(c)}
          ${working ? html`<span class="state-running"><span class="pulse"></span>${c.status === "in_review" ? "reviewing" : "running"}</span>` : nothing}
          <span class="card-spacer"></span>
          <span class="muted" title=${clock(c.updated_at)}>${ago(c.updated_at)}</span>
        </div>
        <a class="card-title" href=${taskHref(repo, c.id)}>${c.title}</a>
        ${
          c.status === "blocked" && (c.block_reason || c.last_error)
            ? html`<div class="card-error">${c.block_reason || c.last_error}</div>`
            : nothing
        }
        <div class="card-meta">
          ${c.agent ? html`<span class="chip">${c.agent}${c.model ? html` · ${c.model}` : nothing}</span>` : nothing}
          ${this.renderLinks(c)}
          ${c.review_cycles ? html`<span class="chip" title="Review cycles">↻ ${c.review_cycles}</span>` : nothing}
        </div>
      </article>
    `;
  }

  private renderList(repo: string, cards: Card[]): TemplateResult {
    const groups = this.group(cards);
    return html`
      <div class="list">
        ${LIST_ORDER.filter((s) => groups.has(s)).map((s) => {
          const list = groups.get(s) ?? [];
          return html`
            <section class="list-group">
              <header class="list-head">
                <span class="dot dot-${s}"></span>${statusLabel(s)}<span class="count">${list.length}</span>
              </header>
              ${list.map(
                (c) => html`
                  <div class="row ${this.route.task === c.id ? "selected" : ""}">
                    ${this.renderTag(c)}
                    ${c.status === "in_progress" || c.status === "in_review" ? html`<span class="pulse"></span>` : nothing}
                    <a class="row-title" href=${taskHref(repo, c.id)}>${c.title}</a>
                    <span class="row-meta">
                      ${c.agent ? html`<span class="chip">${c.agent}</span>` : nothing}
                      ${this.renderLinks(c)}
                      <span class="muted row-time" title=${clock(c.updated_at)}>${ago(c.updated_at)}</span>
                    </span>
                  </div>
                `,
              )}
            </section>
          `;
        })}
      </div>
    `;
  }

  private renderSide(): TemplateResult {
    if (this.route.task !== null) {
      return this.renderTask();
    }

    const tabs: [Side, string][] = [
      ["chat", "Chat"],
      ["activity", "Activity"],
      ["config", "Config"],
    ];
    return html`
      <header class="side-head">
        <div class="seg">
          ${tabs.map(
            ([t, label]) =>
              html`<button class=${this.side === t ? "on" : ""} @click=${(): void => {
                this.side = t;
                void this.loadSide(t);
              }}>${label}</button>`,
          )}
        </div>
        <span class="topbar-spacer"></span>
        <button class="icon" title="Close (Esc)" @click=${(): void => {
          this.side = null;
        }}>✕</button>
      </header>
      <div class="side-body">${this.renderSideBody()}</div>
    `;
  }

  private renderTask(): TemplateResult {
    const d = this.detail;
    if (!d || d.card.id !== this.route.task) {
      return html`<div class="empty">Loading…</div>`;
    }

    const c = d.card;
    const tabs: [TaskTab, string][] = [
      ["overview", "Overview"],
      ["output", "Agent output"],
      ["timeline", "Timeline"],
    ];
    const working = c.status === "in_progress" || c.status === "in_review";
    return html`
      <header class="side-head">
        <button class="tool" @click=${(): void => this.closeTask()}>← Back</button>
        <span class="topbar-spacer"></span>
        <button class="tool" @click=${(): void => this.askAbout(d)}>✦ Ask in chat</button>
        ${
          c.pr_url || c.url
            ? html`<a class="tool primary" href=${c.pr_url ?? c.url} target="_blank" rel="noopener">GitHub ↗</a>`
            : nothing
        }
      </header>
      <div class="task-head">
        <div class="muted">${c.repo}${c.number ? ` #${c.number}` : ` T-${c.id}`}</div>
        <h2>${c.title}</h2>
        <div class="task-status">
          <span class="dot dot-${c.status}"></span>${statusLabel(c.status)}
          ${c.agent ? html`<span class="muted">· ${c.agent}${c.model ? ` · ${c.model}` : ""}</span>` : nothing}
          <span class="muted">· attempt ${c.attempts}</span>
        </div>
      </div>
      <nav class="tabs">
        ${tabs.map(
          ([t, label]) =>
            html`<button class="tab ${this.taskTab === t ? "on" : ""}" @click=${(): void => {
              this.taskTab = t;
            }}>${label}${t === "output" && working ? html` <span class="pulse"></span>` : nothing}</button>`,
        )}
      </nav>
      <div class="side-body">
        ${this.taskTab === "overview" ? this.renderOverview(d) : nothing}
        ${this.taskTab === "output" ? html`<orch-tail class="tail-fill" .src=${`/api/output?id=${c.id}`}></orch-tail>` : nothing}
        ${this.taskTab === "timeline" ? this.renderTimeline(d) : nothing}
      </div>
    `;
  }

  private renderOverview(d: Detail): TemplateResult {
    const c = d.card;
    return html`
      <div class="section">
        ${stepper(d)}
        ${c.block_reason || c.last_error ? html`<div class="task-error">${c.block_reason || c.last_error}</div>` : nothing}
        ${mergeChecklist(c, this.pr, this.prError)}
        ${prCard(this.pr)}
        ${details(d)}
        ${runs(d)}
        <h3>Labels</h3>
        <div class="card-meta">
          ${this.renderLinks(c)}
          ${d.branch ? html`<span class="chip mono">${d.branch}</span>` : nothing}
          ${c.complexity ? html`<span class="chip">${c.complexity}</span>` : nothing}
          ${c.labels.map((l) => html`<span class="chip">${l}</span>`)}
        </div>
        ${d.summary ? html`<h3>Summary</h3><p class="prose">${d.summary}</p>` : nothing}
        ${d.body ? html`<h3>Description</h3><pre class="prose">${d.body}</pre>` : nothing}
      </div>
    `;
  }

  private renderTimeline(d: Detail): TemplateResult {
    return html`
      <ol class="timeline section">
        ${d.activity
          .slice()
          .reverse()
          .map(
            (a) => html`<li>
              <span class="muted" title=${a.timestamp}>${clock(a.timestamp)}</span>
              <span>${a.to_status ? html`${a.from_status ? `${statusLabel(a.from_status)} → ` : ""}${statusLabel(a.to_status)}` : a.event_type}</span>
              ${a.agent ? html`<span class="muted">${a.agent}</span>` : nothing}
            </li>`,
          )}
      </ol>
    `;
  }

  private renderChat(): TemplateResult {
    return html`
      <div class="chat">
        <div class="chat-log">
          ${
            this.messages.length > 0
              ? this.messages.map(
                  (m) => html`<div class="msg msg-${m.role}">
                  <div class="msg-who">${m.role === "user" ? "You" : (m.agent ?? "orch")}</div>
                  <div class="msg-text">${m.content}</div>
                </div>`,
                )
              : html`<p class="muted chat-empty">Ask about ${this.route.repo}: what is blocked, why a task failed, or what to work on next.</p>`
          }
          ${this.sending ? html`<div class="msg msg-assistant"><div class="msg-who">orch</div><div class="msg-text muted">Thinking…</div></div>` : nothing}
        </div>
        <div class="chat-compose">
          <textarea class="chat-input" rows="3" placeholder="Message about this project. ⌘Enter to send"
            @input=${(e: Event): void => {
              this.draft = (e.target as HTMLTextAreaElement).value;
            }}
            @keydown=${(e: KeyboardEvent): void => this.onChatKey(e)}></textarea>
          <button class="tool primary send" ?disabled=${this.sending} @click=${(): void => void this.sendChat()}>Send</button>
        </div>
      </div>
    `;
  }

  private renderSideBody(): TemplateResult {
    const repo = this.route.repo ?? "";
    switch (this.side) {
      case "activity":
        return html`<orch-feed class="feed-fill" .repo=${repo}></orch-feed>`;
      case "config":
        return html`<orch-config class="config-fill" .repo=${repo}></orch-config>`;
      default:
        return this.renderChat();
    }
  }

  private renderLogs(): TemplateResult {
    return html`
      <header class="pane-head">
        ${menuButton()}
        <span class="nav-glyph">≡</span>
        <h1>Service log</h1>
        <div class="topbar-spacer"></div>
        <input class="filter filter-small" type="search" placeholder="Filter" .value=${this.logFilter}
          @input=${(e: Event): void => {
            this.logFilter = (e.target as HTMLInputElement).value;
          }} />
      </header>
      <orch-tail class="tail-fill" .src=${"/api/logs"} .filter=${this.logFilter}></orch-tail>
    `;
  }
}

customElements.define("orch-app", OrchApp);
