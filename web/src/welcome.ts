import { html, nothing } from "lit";
import { api } from "./api";
import type { LoginState, SetupStatus } from "./api";
import type { AddMode } from "./add";
import { Page } from "./page";
import { menuButton } from "./shared";

export const OPEN_ADD = "orch:open-add";

export function openAdd(mode: AddMode) {
  window.dispatchEvent(new CustomEvent(OPEN_ADD, { detail: mode }));
}

const AGENT_LOGIN: Record<string, string> = {
  claude: "Run claude in a terminal and type /login",
  codex: "Run codex login",
  opencode: "Run opencode auth login",
  grok: "Run grok in a terminal and log in",
};

// The first screen when orch has no project: GitHub, the agents, then the first project
export class OrchWelcome extends Page {
  static properties = {
    ...Page.properties,
    setup: { state: true },
    login: { state: true },
    copied: { state: true },
  };

  declare setup: SetupStatus | null;
  declare login: LoginState | null;
  declare copied: boolean;
  private poll = 0;

  constructor() {
    super();
    this.setup = null;
    this.login = null;
    this.copied = false;
  }

  disconnectedCallback() {
    super.disconnectedCallback();
    window.clearInterval(this.poll);
  }

  protected async load() {
    this.setup = await api.setup();
  }

  private async signIn() {
    try {
      this.login = await api.login();
    } catch (e) {
      this.error = e instanceof Error ? e.message : String(e);
      return;
    }

    window.clearInterval(this.poll);
    this.poll = window.setInterval(() => void this.checkLogin(), 2000);
  }

  private async checkLogin() {
    this.login = await api.loginState();
    if (this.login.state !== "waiting") {
      window.clearInterval(this.poll);
      await this.reload();
    }
  }

  private async copyAndOpen() {
    if (this.login?.code) {
      await navigator.clipboard.writeText(this.login.code).catch(() => undefined);
      this.copied = true;
    }

    window.open(this.login?.url ?? "https://github.com/login/device", "_blank", "noopener");
  }

  private renderGitHub(s: SetupStatus) {
    if (s.gh === "ready") {
      return html`<div class="step-row"><span class="check-mark is-ok">✓</span>
        <span>Signed in to GitHub${s.login ? html` as <b>${s.login}</b>` : nothing}</span></div>`;
    }

    if (s.gh === "missing") {
      return html`<div class="step-row"><span class="check-mark is-bad">✕</span>
        <span>orch talks to GitHub through the GitHub CLI, and it is not installed. Install it, then check again:
          <code class="copy-line">brew install gh</code></span></div>`;
    }

    const l = this.login;
    if (l?.state === "waiting" && l.code) {
      return html`<div class="device">
        <p>Enter this code on GitHub to sign in:</p>
        <div class="device-code mono">${l.code}</div>
        <button class="tool primary" @click=${() => void this.copyAndOpen()}>${this.copied ? "Copied · open GitHub again" : "Copy code and open GitHub"}</button>
        <p class="muted">This page moves on by itself once GitHub confirms.</p>
      </div>`;
    }

    return html`<div class="step-row"><span class="check-mark is-wait">…</span>
      <span>orch needs GitHub to read issues and open pull requests.</span>
      <span class="topbar-spacer"></span>
      <button class="tool primary" ?disabled=${l?.state === "waiting"} @click=${() => void this.signIn()}>
        ${l?.state === "waiting" ? "Starting…" : "Sign in with GitHub"}
      </button>
    </div>
    ${l?.state === "failed" ? html`<div class="task-error">${l.error ?? "Sign-in failed, try again"}</div>` : nothing}`;
  }

  render() {
    const s = this.setup;
    const ready = s?.agents.filter(([, st]) => st === "ready") ?? [];
    return html`
      <header class="pane-head">${menuButton()}<h1>Welcome to orch</h1></header>
      <div class="page welcome">
        <p class="welcome-lede">Write the plan, orch builds it. Three steps and the first agent can start.</p>
        ${this.error ? html`<div class="error-bar">${this.error}</div>` : nothing}
        ${!s
          ? html`<p class="block-empty">Checking GitHub and your agents…</p>`
          : html`
            <section class="block onboard ${s.gh === "ready" ? "done" : ""}">
              <header class="block-head"><span class="onboard-n">1</span><h2>GitHub</h2></header>
              ${this.renderGitHub(s)}
            </section>

            <section class="block onboard ${ready.length ? "done" : ""}">
              <header class="block-head"><span class="onboard-n">2</span><h2>Coding agents</h2>
                <span class="topbar-spacer"></span><button class="tool" @click=${() => void this.reload()}>Check again</button></header>
              <p class="block-note muted">orch runs the agent CLIs you already use. One logged-in agent is enough.</p>
              ${s.agents.map(
                ([name, st]) => html`<div class="step-row">
                  <span class="check-mark ${st === "ready" ? "is-ok" : "is-wait"}">${st === "ready" ? "✓" : "…"}</span>
                  <b class="agent-name">${name}</b>
                  <span class="muted">${st === "ready" ? "ready" : st === "missing" ? "not installed" : (AGENT_LOGIN[name] ?? `Log in to ${name}`)}</span>
                </div>`,
              )}
            </section>

            <section class="block onboard">
              <header class="block-head"><span class="onboard-n">3</span><h2>Add a project</h2></header>
              <p class="block-note muted">A project is a GitHub repository. orch works on its issues in worktrees of its own, so a copy on this computer is optional.</p>
              <div class="add-options">
                <button class="add-option" ?disabled=${s.gh !== "ready"} @click=${() => openAdd("clone")}>
                  <b>Clone a repository</b><span class="muted">Pick one of your GitHub repositories</span></button>
                <button class="add-option" ?disabled=${s.gh !== "ready"} @click=${() => openAdd("create")}>
                  <b>Create a new repository</b><span class="muted">Start from an empty repo and a plan</span></button>
                <button class="add-option" @click=${() => openAdd("existing")}>
                  <b>Add an existing repository</b><span class="muted">A checkout already on this computer</span></button>
                <button class="add-option" disabled title="Comes with multi-node">
                  <b>Watch only</b><span class="muted">Follow a project another machine runs. Comes with multi-node</span></button>
              </div>
            </section>
          `}
      </div>
    `;
  }
}

customElements.define("orch-welcome", OrchWelcome);
