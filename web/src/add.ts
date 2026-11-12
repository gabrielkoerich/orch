import { LitElement, html, nothing } from "lit";
import { api } from "./api";
import type { AddProject, Dirs, RemoteRepo, SetupRepos } from "./api";
import { ago } from "./format";

export type AddMode = "clone" | "create" | "existing";

// The app closes the dialog on this, and opens the new project on ADDED
export const CLOSE_ADD = "orch:close-add";
export const ADDED = "orch:added";

// Adding a project the way GitHub Desktop adds a repository: clone one, create one, or use a checkout
export class OrchAdd extends LitElement {
  static properties = {
    mode: { type: String },
    repos: { state: true },
    owners: { state: true },
    filter: { state: true },
    picked: { state: true },
    path: { state: true },
    owner: { state: true },
    name: { state: true },
    description: { state: true },
    isPrivate: { state: true },
    browsing: { state: true },
    dirs: { state: true },
    saving: { state: true },
    error: { state: true },
  };

  declare mode: AddMode;
  declare repos: SetupRepos | null;
  declare owners: string[];
  declare filter: string;
  declare picked: RemoteRepo | null;
  declare path: string;
  declare owner: string;
  declare name: string;
  declare description: string;
  declare isPrivate: boolean;
  declare browsing: boolean;
  declare dirs: Dirs | null;
  declare saving: boolean;
  declare error: string;

  constructor() {
    super();
    this.mode = "clone";
    this.repos = null;
    this.owners = [];
    this.filter = "";
    this.picked = null;
    this.path = "";
    this.owner = "";
    this.name = "";
    this.description = "";
    this.isPrivate = true;
    this.browsing = false;
    this.dirs = null;
    this.saving = false;
    this.error = "";
  }

  createRenderRoot() {
    return this;
  }

  updated(changed: Map<string, unknown>) {
    if (changed.has("mode")) {
      this.error = "";
      this.browsing = false;
      void this.load();
    }
  }

  private async load() {
    try {
      if (this.mode === "clone" && !this.repos) {
        this.repos = await api.setupRepos();
      }

      if (this.mode === "create" && !this.owners.length) {
        this.owners = await api.owners();
        this.owner = this.owners[0] ?? "";
      }
    } catch (e) {
      this.error = e instanceof Error ? e.message : String(e);
    }
  }

  private close() {
    window.dispatchEvent(new Event(CLOSE_ADD));
  }

  private request(): AddProject | null {
    switch (this.mode) {
      case "clone":
        return this.picked ? { kind: "clone", repo: this.picked.repo, path: this.path } : null;
      case "existing":
        return this.path.trim() ? { kind: "existing", path: this.path } : null;
      case "create":
        return this.owner && this.name.trim()
          ? { kind: "create", owner: this.owner, name: this.name.trim(), description: this.description, private: this.isPrivate }
          : null;
    }
  }

  private async submit() {
    const req = this.request();
    if (!req || this.saving) {
      return;
    }

    this.saving = true;
    this.error = "";
    try {
      const made = await api.addProject(req);
      window.dispatchEvent(new CustomEvent(ADDED, { detail: made.repo }));
    } catch (e) {
      this.error = e instanceof Error ? e.message : String(e);
    }

    this.saving = false;
  }

  private async browse(path = "") {
    this.browsing = true;
    try {
      this.dirs = await api.dirs(path || this.path);
    } catch {
      this.dirs = await api.dirs("");
    }
  }

  private pick(r: RemoteRepo) {
    this.picked = r;
    // A checkout found on this computer is the natural place, empty means orch keeps its own copy
    this.path = r.checkout ?? "";
  }

  private renderClone() {
    const needle = this.filter.trim().toLowerCase();
    const list = (this.repos?.github ?? []).filter((r) => !needle || r.repo.toLowerCase().includes(needle));
    const owners = [...new Set(list.map((r) => r.repo.split("/")[0]))];
    return html`
      <input class="filter" placeholder="Filter your repositories" .value=${this.filter}
        @input=${(e: Event) => {
          this.filter = (e.target as HTMLInputElement).value;
        }} />
      <div class="repo-list">
        ${!this.repos
          ? html`<p class="block-empty">Loading your repositories…</p>`
          : list.length
            ? owners.map(
                (o) => html`<div class="repo-owner muted">${o}</div>
                  ${list.filter((r) => r.repo.startsWith(`${o}/`)).map(
                    (r) => html`<button class="repo-row ${this.picked?.repo === r.repo ? "on" : ""}" ?disabled=${r.added}
                      @click=${() => this.pick(r)} @dblclick=${() => void this.submit()}>
                      <span class="repo-name">${r.repo.split("/")[1]}</span>
                      ${r.private ? html`<span class="chip">private</span>` : nothing}
                      ${r.added ? html`<span class="chip chip-ok">added</span>` : r.checkout ? html`<span class="chip" title=${r.checkout}>on this computer</span>` : nothing}
                      <span class="topbar-spacer"></span>
                      <span class="muted">${r.pushed_at ? ago(r.pushed_at) : ""}</span>
                    </button>`,
                  )}`,
              )
            : html`<p class="block-empty">No repository matches.</p>`}
      </div>
      ${this.renderPath("Local path", "Leave empty and orch keeps its own copy")}
    `;
  }

  private renderPath(label: string, placeholder: string) {
    return html`
      <label class="field-label">${label}</label>
      <div class="path-row">
        <input class="filter" placeholder=${placeholder} .value=${this.path}
          @input=${(e: Event) => {
            this.path = (e.target as HTMLInputElement).value;
          }} />
        <button class="tool" @click=${() => void this.browse()}>Choose…</button>
      </div>
      ${this.browsing ? this.renderBrowser() : nothing}
    `;
  }

  private renderBrowser() {
    const d = this.dirs;
    if (!d) {
      return html`<div class="browser"><p class="block-empty">Loading…</p></div>`;
    }

    return html`<div class="browser">
      <div class="browser-head">
        <button class="icon" title="Up" ?disabled=${!d.parent} @click=${() => void this.browse(d.parent ?? "")}>↑</button>
        <span class="mono browser-path" title=${d.path}>${d.path}</span>
        <span class="topbar-spacer"></span>
        <button class="tool primary" @click=${() => {
          this.path = d.path;
          this.browsing = false;
        }}>Use this folder</button>
      </div>
      <div class="browser-list">
        ${d.dirs.map(
          (x) => html`<button class="browser-row" @click=${() => void this.browse(x.path)}>
            <span>📁</span><span>${x.name}</span>
            ${x.repo ? html`<span class="chip">${x.repo}</span>` : nothing}
          </button>`,
        )}
        ${d.dirs.length ? nothing : html`<p class="block-empty">No folders here.</p>`}
      </div>
    </div>`;
  }

  private renderCreate() {
    return html`
      <label class="field-label">Owner</label>
      <select class="filter" .value=${this.owner} @change=${(e: Event) => {
        this.owner = (e.target as HTMLSelectElement).value;
      }}>
        ${this.owners.map((o) => html`<option value=${o} ?selected=${o === this.owner}>${o}</option>`)}
      </select>
      <label class="field-label">Name</label>
      <input class="filter" placeholder="my-app" .value=${this.name} @input=${(e: Event) => {
        this.name = (e.target as HTMLInputElement).value.replace(/\s+/g, "-");
      }} />
      <label class="field-label">Description</label>
      <input class="filter" placeholder="Optional" .value=${this.description} @input=${(e: Event) => {
        this.description = (e.target as HTMLInputElement).value;
      }} />
      <label class="choice"><input type="checkbox" .checked=${this.isPrivate} @change=${(e: Event) => {
        this.isPrivate = (e.target as HTMLInputElement).checked;
      }} /><span><b>Private</b><span class="muted">only you and people you add can see it</span></span></label>
      <p class="muted">orch creates the repository on GitHub with a README and keeps its own copy. Then write a plan or add a first task.</p>
    `;
  }

  render() {
    const titles: Record<AddMode, [string, string]> = {
      clone: ["Clone a repository", "Clone"],
      create: ["Create a new repository", "Create repository"],
      existing: ["Add a local repository", "Add repository"],
    };
    const [title, action] = titles[this.mode];
    return html`
      <div class="scrim" @click=${() => this.close()}></div>
      <div class="dialog" role="dialog" aria-label=${title}>
        <header class="dialog-head"><h2>${title}</h2><span class="topbar-spacer"></span>
          <button class="icon" title="Close" @click=${() => this.close()}>✕</button></header>
        <div class="dialog-body">
          ${this.mode === "clone" ? this.renderClone() : nothing}
          ${this.mode === "create" ? this.renderCreate() : nothing}
          ${this.mode === "existing" ? this.renderPath("Local path", "A folder that is a git checkout of a GitHub repository") : nothing}
          ${this.error ? html`<div class="task-error">${this.error}</div>` : nothing}
        </div>
        <footer class="dialog-foot">
          <button class="tool" @click=${() => this.close()}>Cancel</button>
          <button class="tool primary" ?disabled=${!this.request() || this.saving} @click=${() => void this.submit()}>
            ${this.saving ? "Working…" : action}
          </button>
        </footer>
      </div>
    `;
  }
}

customElements.define("orch-add", OrchAdd);
