import { LitElement, html, nothing } from "lit";
import type { TemplateResult } from "lit";
import { api } from "./api";
import type { PlanPreview, PlanState } from "./api";

// Enough markdown for a plan: headings, list items and paragraphs, built as templates so nothing
// the agent writes becomes markup
function renderMarkdown(text: string): TemplateResult[] {
  const out: TemplateResult[] = [];
  let list: string[] = [];
  let para: string[] = [];
  const flush = () => {
    if (para.length) {
      out.push(html`<p>${para.join(" ")}</p>`);
      para = [];
    }

    if (list.length) {
      const items = list;
      out.push(html`<ul>${items.map((i) => html`<li>${i}</li>`)}</ul>`);
      list = [];
    }
  };
  for (const raw of text.split("\n")) {
    const line = raw.trimEnd();
    const heading = /^(#{1,4})\s+(.*)$/.exec(line);
    const item = /^\s*[-*]\s+(.*)$/.exec(line) ?? /^\s*\d+\.\s+(.*)$/.exec(line);
    if (heading) {
      flush();
      const level = heading[1].length;
      out.push(level === 1 ? html`<h1>${heading[2]}</h1>` : level === 2 ? html`<h2>${heading[2]}</h2>` : html`<h3>${heading[2]}</h3>`);
    } else if (item) {
      if (para.length) {
        flush();
      }

      list.push(item[1]);
    } else if (!line.trim()) {
      flush();
    } else {
      if (list.length) {
        flush();
      }

      para.push(line.trim());
    }
  }

  flush();
  return out;
}

// Plan mode for one project: talk the plan through with an agent, then import it as tasks
export class OrchPlan extends LitElement {
  static properties = {
    repo: { type: String },
    state: { state: true },
    thinking: { state: true },
    error: { state: true },
    preview: { state: true },
    previewing: { state: true },
    importing: { state: true },
    imported: { state: true },
    name: { state: true },
  };

  declare repo: string;
  declare state: PlanState | null;
  declare thinking: boolean;
  declare error: string;
  declare preview: PlanPreview | null;
  declare previewing: boolean;
  declare importing: boolean;
  declare imported: string;
  declare name: string;
  private draft = "";

  constructor() {
    super();
    this.repo = "";
    this.state = null;
    this.thinking = false;
    this.error = "";
    this.preview = null;
    this.previewing = false;
    this.importing = false;
    this.imported = "";
    this.name = "";
  }

  createRenderRoot() {
    return this;
  }

  updated(changed: Map<string, unknown>) {
    if (changed.has("repo") && this.repo) {
      void this.load();
    }
  }

  private async load() {
    try {
      this.state = await api.plan(this.repo);
      this.name = this.state.name;
      this.scrollLog();
    } catch (e) {
      this.error = e instanceof Error ? e.message : String(e);
    }
  }

  private scrollLog() {
    void this.updateComplete.then(() => {
      const log = this.querySelector(".plan-log");
      if (log) {
        log.scrollTop = log.scrollHeight;
      }
    });
  }

  private async say() {
    const text = this.draft.trim();
    if (!text || this.thinking || !this.state) {
      return;
    }

    this.thinking = true;
    this.error = "";
    this.state = { ...this.state, messages: [...this.state.messages, { role: "user", content: text, created_at: "" }] };
    this.draft = "";
    const input = this.querySelector<HTMLTextAreaElement>(".plan-input");
    if (input) {
      input.value = "";
    }

    this.scrollLog();
    try {
      const r = await api.planSay(this.repo, text);
      this.state = {
        ...this.state,
        draft: r.draft,
        messages: [...this.state.messages, { role: "assistant", content: r.reply, created_at: "" }],
      };
      this.preview = null;
    } catch (e) {
      this.error = e instanceof Error ? e.message : String(e);
    }

    this.thinking = false;
    this.scrollLog();
  }

  private async showPreview() {
    this.previewing = true;
    this.error = "";
    try {
      this.preview = await api.planPreview(this.repo, this.name);
    } catch (e) {
      this.error = e instanceof Error ? e.message : String(e);
    }

    this.previewing = false;
  }

  private async runImport() {
    this.importing = true;
    this.error = "";
    try {
      const r = await api.planImport(this.repo, this.name);
      this.imported = r.report;
      this.preview = null;
      await this.load();
    } catch (e) {
      this.error = e instanceof Error ? e.message : String(e);
    }

    this.importing = false;
  }

  private async startOver() {
    if (!confirm("Start a new plan? The conversation and the draft go, imported issues stay.")) {
      return;
    }

    await api.planClear(this.repo);
    this.imported = "";
    this.preview = null;
    await this.load();
  }

  private renderPreview(p: PlanPreview) {
    return html`
      <div class="scrim" @click=${() => {
        this.preview = null;
      }}></div>
      <div class="dialog dialog-wide" role="dialog" aria-label="Import the plan">
        <header class="dialog-head"><h2>Import as tasks</h2><span class="topbar-spacer"></span>
          <button class="icon" @click=${() => {
            this.preview = null;
          }}>✕</button></header>
        <div class="dialog-body">
          <p class="muted">orch commits the plan to <code>${p.path}</code> on the default branch, then creates one issue per task below. A task waits for the tasks it depends on.</p>
          <pre class="code">${p.outline}</pre>
          <pre class="code">${p.report}</pre>
        </div>
        <footer class="dialog-foot">
          <button class="tool" @click=${() => {
            this.preview = null;
          }}>Cancel</button>
          <button class="tool primary" ?disabled=${this.importing} @click=${() => void this.runImport()}>
            ${this.importing ? "Creating issues…" : "Commit and create issues"}
          </button>
        </footer>
      </div>
    `;
  }

  render() {
    const s = this.state;
    if (!s) {
      return html`<div class="empty">${this.error || "Loading…"}</div>`;
    }

    const hasDraft = s.draft.trim().length > 0;
    return html`
      <div class="plan">
        <section class="plan-chat">
          <div class="plan-log">
            ${s.messages.length
              ? s.messages.map(
                  (m) => html`<div class="msg msg-${m.role}">
                    <div class="msg-who">${m.role === "user" ? "You" : "Planner"}</div>
                    <div class="msg-text">${m.content}</div>
                  </div>`,
                )
              : html`<div class="plan-intro">
                  <h2>What should orch build?</h2>
                  <p class="muted">Describe the goal in a few sentences. The planner asks what it needs to know, writes the plan on the right, and changes it as you talk. When it looks right, import it and orch creates the tasks in order.</p>
                </div>`}
            ${this.thinking ? html`<div class="msg msg-assistant"><div class="msg-who">Planner</div><div class="msg-text muted">Thinking…</div></div>` : nothing}
          </div>
          ${this.error ? html`<div class="task-error">${this.error}</div>` : nothing}
          <div class="chat-compose">
            <textarea class="chat-input plan-input" rows="3" placeholder=${s.messages.length ? "Answer, or ask for a change. ⌘Enter to send" : "I want to build… ⌘Enter to send"}
              @input=${(e: Event) => {
                this.draft = (e.target as HTMLTextAreaElement).value;
              }}
              @keydown=${(e: KeyboardEvent) => {
                if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
                  e.preventDefault();
                  void this.say();
                }
              }}></textarea>
            <div class="compose-actions">
              ${s.messages.length ? html`<button class="tool" @click=${() => void this.startOver()}>Start over</button>` : nothing}
              <span class="topbar-spacer"></span>
              <button class="tool primary" ?disabled=${this.thinking} @click=${() => void this.say()}>Send</button>
            </div>
          </div>
        </section>

        <section class="plan-draft">
          <header class="plan-draft-head">
            <h3>Plan</h3>
            <span class="topbar-spacer"></span>
            ${hasDraft
              ? html`<input class="filter plan-name" title="File name under docs/plans/" .value=${this.name}
                  @input=${(e: Event) => {
                    this.name = (e.target as HTMLInputElement).value;
                  }} />
                <button class="tool primary" ?disabled=${this.previewing || this.thinking} @click=${() => void this.showPreview()}>
                  ${this.previewing ? "Drafting the tasks…" : "Import as tasks"}
                </button>`
              : nothing}
          </header>
          <div class="plan-doc">
            ${this.imported ? html`<div class="imported"><b>Imported.</b><pre class="code">${this.imported}</pre></div>` : nothing}
            ${hasDraft ? renderMarkdown(s.draft) : html`<p class="block-empty">The plan appears here as the planner writes it.</p>`}
          </div>
        </section>
      </div>
      ${this.preview ? this.renderPreview(this.preview) : nothing}
    `;
  }
}

customElements.define("orch-plan", OrchPlan);
