import type { TemplateResult } from "lit";
import { html, nothing } from "lit";
import type { Card, Inbox, Landed, Project } from "./api";
import { api } from "./api";
import { ago, initials, statusLabel } from "./format";
import { Page } from "./page";
import { addDays, isWorking, menuButton, shortRepo, stageBar, startOfDay, taskHref, taskLine, taskRef } from "./shared";

const LIST_MAX = 6;

function count(p: Project, s: string): number {
  return p.counts.find(([k]) => k === s)?.[1] ?? 0;
}

function openCount(p: Project): number {
  return p.counts.filter(([s]) => s !== "done").reduce((n, [, c]) => n + c, 0);
}

// The first screen: every project, who is working, what needs the owner and what landed today
export class OrchHome extends Page {
  static override properties = {
    ...Page.properties,
    projects: { state: true },
    running: { state: true },
    inbox: { state: true },
    landed: { state: true },
  };

  declare projects: Project[];
  declare running: Card[];
  declare inbox: Inbox;
  declare landed: Landed[];

  constructor() {
    super();
    this.projects = [];
    this.running = [];
    this.inbox = { blocked: [], human: [] };
    this.landed = [];
  }

  protected async load(): Promise<void> {
    const today = startOfDay(new Date());
    [this.projects, this.running, this.inbox, this.landed] = await Promise.all([
      api.projects(),
      api.running(),
      api.inbox(),
      api.day(today, addDays(today, 1)).then((d) => d.landed),
    ]);
  }

  override render(): TemplateResult {
    const needs = this.inbox.blocked.length + this.inbox.human.length;
    const open = this.projects.reduce((n, p) => n + openCount(p), 0);
    return html`
      <header class="pane-head">${menuButton()}<h1>Home</h1>
        <span class="muted">${new Date().toLocaleDateString(undefined, { weekday: "long", month: "long", day: "numeric" })}</span>
      </header>
      <div class="page">
        ${this.error ? html`<div class="error-bar">${this.error}</div>` : nothing}
        <div class="tiles">
          <div class="tile">
            <span class="tile-n ${this.running.length > 0 ? "tile-working" : ""}">${this.running.length}</span>
            <span class="tile-l">${this.running.length === 1 ? "agent working" : "agents working"}</span>
          </div>
          <a class="tile ${needs ? "tile-alert" : ""}" href="#/inbox">
            <span class="tile-n">${needs}</span>
            <span class="tile-l">need you</span>
          </a>
          <a class="tile" href="#/day">
            <span class="tile-n">${this.landed.length}</span>
            <span class="tile-l">merged today</span>
          </a>
          <div class="tile">
            <span class="tile-n">${open}</span>
            <span class="tile-l">open tasks in ${this.projects.length} ${this.projects.length === 1 ? "project" : "projects"}</span>
          </div>
        </div>

        <div class="home-grid">
          <section class="block">
            <header class="block-head"><h2>Working now</h2><span class="count">${this.running.length}</span></header>
            ${
              this.running.length > 0
                ? html`<div class="working-list">${this.running.map((c) => this.renderWorking(c))}</div>`
                : html`<p class="block-empty">No agent is working. Tasks start as soon as an agent is free.</p>`
            }
          </section>

          <section class="block">
            <header class="block-head"><h2>Needs you</h2><span class="count">${needs}</span>
              <span class="topbar-spacer"></span><a class="block-link" href="#/inbox">Inbox →</a></header>
            ${
              needs
                ? html`${[...this.inbox.blocked, ...this.inbox.human].slice(0, LIST_MAX).map((c) => taskLine(c, { repo: true, reason: true, at: c.since ?? undefined }))}`
                : html`<p class="block-empty">Nothing waits on you.</p>`
            }
          </section>
        </div>

        <section class="block">
          <header class="block-head"><h2>Projects</h2><span class="count">${this.projects.length}</span></header>
          <div class="project-grid">${this.projects.map((p) => this.renderProject(p))}</div>
        </section>

        <section class="block">
          <header class="block-head"><h2>Merged today</h2><span class="count">${this.landed.length}</span>
            <span class="topbar-spacer"></span><a class="block-link" href="#/day">Day report →</a></header>
          ${
            this.landed.length > 0
              ? this.landed.slice(0, LIST_MAX).map((l) => taskLine(l.card, { repo: true, at: l.at }))
              : html`<p class="block-empty">Nothing merged yet today.</p>`
          }
        </section>
      </div>
    `;
  }

  private renderWorking(c: Card): TemplateResult {
    return html`
      <a class="working-card" href=${taskHref(c)}>
        <div class="working-top">
          <span class="spinner"></span>
          <span class="working-agent">${c.agent ?? "agent"}${c.model ? html`<span class="muted"> · ${c.model}</span>` : nothing}</span>
          <span class="topbar-spacer"></span>
          <span class="muted" title="Time in this status">${ago(c.since ?? c.updated_at)}</span>
        </div>
        <div class="working-title">${c.title}</div>
        <div class="muted working-meta">${shortRepo(c.repo)} ${taskRef(c)} · ${isWorking(c) && c.status === "in_review" ? "reviewing" : statusLabel(c.status).toLowerCase()}</div>
      </a>
    `;
  }

  private renderProject(p: Project): TemplateResult {
    const blocked = count(p, "blocked");
    const working = count(p, "in_progress") + count(p, "in_review");
    return html`
      <a class="project-card" href="#/${p.repo}">
        <div class="project-card-head">
          <span class="project-icon">${initials(p.repo)}</span>
          <span class="project-card-name">${shortRepo(p.repo)}</span>
          <span class="muted">${p.repo.split("/")[0]}</span>
        </div>
        ${stageBar(p.counts)}
        <div class="project-card-stats">
          <span><b>${openCount(p)}</b> open</span>
          <span class=${working ? "is-working" : ""}><b>${working}</b> working</span>
          <span class=${blocked ? "is-blocked" : ""}><b>${blocked}</b> blocked</span>
          <span><b>${count(p, "done")}</b> done</span>
        </div>
      </a>
    `;
  }
}

customElements.define("orch-home", OrchHome);
