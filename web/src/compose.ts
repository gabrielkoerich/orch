import { LitElement, html, nothing } from "lit";
import { api } from "./api";
import type { Card } from "./api";
import { statusLabel } from "./format";
import { menuButton, shortRepo, taskRef } from "./shared";

// The router picks the agent, no agent keeps the task for a person, or a named agent skips the router
type AgentChoice = "router" | "none" | string;

const PICKER_MAX = 8;

// A new task for one project, written like a GitHub issue
export class OrchCompose extends LitElement {
  static properties = {
    repo: { type: String },
    agents: { state: true },
    tasks: { state: true },
    title: { state: true },
    body: { state: true },
    agent: { state: true },
    deps: { state: true },
    search: { state: true },
    saving: { state: true },
    error: { state: true },
  };

  declare repo: string;
  declare agents: string[];
  declare tasks: Card[];
  declare title: string;
  declare body: string;
  declare agent: AgentChoice;
  declare deps: Card[];
  declare search: string;
  declare saving: boolean;
  declare error: string;

  constructor() {
    super();
    this.repo = "";
    this.agents = [];
    this.tasks = [];
    this.title = "";
    this.body = "";
    this.agent = "router";
    this.deps = [];
    this.search = "";
    this.saving = false;
    this.error = "";
  }

  createRenderRoot() {
    return this;
  }

  updated(changed: Map<string, unknown>) {
    if (changed.has("repo") && this.repo) {
      void this.load();
    }
  }

  firstUpdated() {
    this.querySelector<HTMLInputElement>(".compose-title")?.focus();
  }

  private async load() {
    try {
      [this.agents, this.tasks] = await Promise.all([api.agents(), api.tasks(this.repo)]);
    } catch (e) {
      this.error = e instanceof Error ? e.message : String(e);
    }
  }

  // Open tasks first, since a task rarely waits on one that already merged
  private found(): Card[] {
    const q = this.search.trim().toLowerCase().replace(/^#/, "");
    if (!q) {
      return [];
    }

    const picked = new Set(this.deps.map((d) => d.id));
    return this.tasks
      .filter((t) => !picked.has(t.id))
      .filter((t) => t.title.toLowerCase().includes(q) || (t.number ?? "").startsWith(q) || String(t.id) === q)
      .sort((a, b) => Number(a.status === "done") - Number(b.status === "done"))
      .slice(0, PICKER_MAX);
  }

  private addDep(c: Card) {
    this.deps = [...this.deps, c];
    this.search = "";
    this.querySelector<HTMLInputElement>(".dep-search")?.focus();
  }

  private cancel() {
    location.hash = `#/${this.repo}`;
  }

  private async submit() {
    if (!this.title.trim() || this.saving) {
      return;
    }

    this.saving = true;
    this.error = "";
    try {
      const made = await api.createTask({
        repo: this.repo,
        title: this.title.trim(),
        body: this.body,
        agent: this.agent === "router" ? null : this.agent,
        depends_on: this.deps.map((d) => d.id),
      });
      location.hash = `#/${this.repo}/${made.id}`;
    } catch (e) {
      this.error = e instanceof Error ? e.message : String(e);
      this.saving = false;
    }
  }

  private onKey(e: KeyboardEvent) {
    if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      void this.submit();
    }
  }

  render() {
    const found = this.found();
    const waits = this.deps.some((d) => d.status !== "done");
    return html`
      <header class="pane-head">${menuButton()}<h1>New task</h1>
        <span class="muted">in ${shortRepo(this.repo)}</span></header>
      <div class="page compose" @keydown=${(e: KeyboardEvent) => this.onKey(e)}>
        <div class="compose-main">
          <input class="compose-title" placeholder="Title" .value=${this.title}
            @input=${(e: Event) => {
              this.title = (e.target as HTMLInputElement).value;
            }} />
          <textarea class="compose-body" placeholder="What should happen, and how will you know it is done? Markdown works, as on GitHub."
            .value=${this.body}
            @input=${(e: Event) => {
              this.body = (e.target as HTMLTextAreaElement).value;
            }}></textarea>
          ${this.error ? html`<div class="task-error">${this.error}</div>` : nothing}
          <div class="compose-actions">
            <span class="muted">⌘Enter to create</span>
            <span class="topbar-spacer"></span>
            <button class="tool" @click=${() => this.cancel()}>Cancel</button>
            <button class="tool primary" ?disabled=${!this.title.trim() || this.saving} @click=${() => void this.submit()}>
              ${this.saving ? "Creating…" : "Create task"}
            </button>
          </div>
        </div>

        <aside class="compose-side">
          <section>
            <h3>Agent</h3>
            <label class="choice"><input type="radio" name="agent" .checked=${this.agent === "router"} @change=${() => {
              this.agent = "router";
            }} /><span><b>Let the router pick</b><span class="muted">orch chooses the agent and the model</span></span></label>
            ${this.agents.map(
              (a) => html`<label class="choice"><input type="radio" name="agent" .checked=${this.agent === a} @change=${() => {
                this.agent = a;
              }} /><span><b>${a}</b><span class="muted">skips the router</span></span></label>`,
            )}
            <label class="choice"><input type="radio" name="agent" .checked=${this.agent === "none"} @change=${() => {
              this.agent = "none";
            }} /><span><b>No agent</b><span class="muted">waits for you, labelled <code>no-agent</code></span></span></label>
          </section>

          <section>
            <h3>Depends on</h3>
            ${this.deps.map(
              (d) => html`<div class="dep">
                <span class="dot dot-${d.status}" title=${statusLabel(d.status)}></span>
                <span class="mono muted">${taskRef(d)}</span>
                <span class="dep-title">${d.title}</span>
                <button class="icon" title="Remove" @click=${() => {
                  this.deps = this.deps.filter((x) => x.id !== d.id);
                }}>✕</button>
              </div>`,
            )}
            <input class="filter dep-search" placeholder="Search by title or #number" .value=${this.search}
              @input=${(e: Event) => {
                this.search = (e.target as HTMLInputElement).value;
              }}
              @keydown=${(e: KeyboardEvent) => {
                if (e.key === "Enter" && found.length) {
                  e.preventDefault();
                  this.addDep(found[0]);
                }
              }} />
            ${found.length
              ? html`<div class="picker">${found.map(
                  (t) => html`<button class="picker-row" @click=${() => this.addDep(t)}>
                    <span class="dot dot-${t.status}"></span><span class="mono muted">${taskRef(t)}</span><span class="dep-title">${t.title}</span>
                  </button>`,
                )}</div>`
              : nothing}
            <p class="muted compose-note">${waits
              ? "This task waits until every task above is done."
              : "A task with an open dependency waits until that task is done."}</p>
          </section>
        </aside>
      </div>
    `;
  }
}

customElements.define("orch-compose", OrchCompose);
