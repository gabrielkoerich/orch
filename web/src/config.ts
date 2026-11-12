import { LitElement, html, nothing } from "lit";
import { api } from "./api";
import type { ConfigFile } from "./api";

// Edits a config file as text, so its comments and layout survive the save
export class OrchConfig extends LitElement {
  static properties = {
    repo: { type: String },
    file: { state: true },
    text: { state: true },
    saving: { state: true },
    message: { state: true },
    failed: { state: true },
  };

  declare repo: string;
  declare file: ConfigFile | null;
  declare text: string;
  declare saving: boolean;
  declare message: string;
  declare failed: boolean;

  constructor() {
    super();
    this.repo = "";
    this.file = null;
    this.text = "";
    this.saving = false;
    this.message = "";
    this.failed = false;
  }

  createRenderRoot() {
    return this;
  }

  updated(changed: Map<string, unknown>) {
    if (changed.has("repo")) {
      void this.load();
    }
  }

  private async load() {
    this.message = "";
    try {
      this.file = await api.config(this.repo);
      this.text = this.file.text;
    } catch (e) {
      this.failed = true;
      this.message = e instanceof Error ? e.message : String(e);
    }
  }

  private async save() {
    if (!this.file) {
      return;
    }

    this.saving = true;
    try {
      this.file = await api.saveConfig(this.repo, this.text, this.file.version);
      this.text = this.file.text;
      this.failed = false;
      this.message = "Saved. orch reloads the file on its own, the old copy is in .bak";
    } catch (e) {
      this.failed = true;
      this.message = e instanceof Error ? e.message : String(e);
    }

    this.saving = false;
  }

  private onKey(e: KeyboardEvent) {
    if (e.key === "s" && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      void this.save();
    }
  }

  render() {
    if (!this.file) {
      return html`<div class="empty">${this.message || "Loading…"}</div>`;
    }

    const dirty = this.text !== this.file.text;
    return html`
      <div class="config">
        <div class="config-head">
          <span class="mono muted config-path" title=${this.file.path}>${this.file.path}</span>
          ${dirty ? html`<span class="chip">edited</span>` : nothing}
        </div>
        <textarea class="config-text" spellcheck="false" .value=${this.text}
          @input=${(e: Event) => {
            this.text = (e.target as HTMLTextAreaElement).value;
          }}
          @keydown=${(e: KeyboardEvent) => this.onKey(e)}></textarea>
        <div class="config-actions">
          <span class="config-message ${this.failed ? "is-error" : ""}">${this.message}</span>
          <button class="tool" ?disabled=${!dirty || this.saving} @click=${() => {
            this.text = this.file?.text ?? "";
            this.message = "";
          }}>Revert</button>
          <button class="tool primary" ?disabled=${!dirty || this.saving} @click=${() => void this.save()}>
            ${this.saving ? "Saving…" : "Save ⌘S"}
          </button>
        </div>
      </div>
    `;
  }
}

customElements.define("orch-config", OrchConfig);
