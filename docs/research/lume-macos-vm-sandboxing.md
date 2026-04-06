# Research: Lume macOS VM Sandboxing for Orch Agent Runners

> Status: Research / Proposal  
> Issue: #2041  
> Date: 2026-04-06

---

## Summary

This document evaluates [Lume](https://github.com/trycua/cua) — a macOS VM runtime built on Apple's native Virtualization Framework — as a sandboxing layer for Orch agent runners. The goal is to assess whether Lume is a good fit for providing strong, VM-level isolation for agents (Claude, Codex, OpenCode) that currently run directly on the host machine.

**Verdict: Strong fit. Lume is a practical and architecturally clean match for Orch's runner pipeline.** The integration path is well-defined, the security improvement is significant, and the operational overhead is manageable with a golden-image + clone workflow.

---

## Current Isolation Model

Orch agents run directly on the host machine inside a tmux session. Isolation today is layered but shallow:

| Layer | Mechanism | Coverage |
|---|---|---|
| Git isolation | Worktree per task at `~/.orch/worktrees/{project}/{branch}/` | Source code only |
| Tool blocklist | `--disallowedTools 'Bash(rm *), Bash(rm -*), Bash(git push*)'` (Claude) | Best-effort, bypassable |
| Path restriction | `--disallowedTools` blocking Edit/Write/Bash on main project dir | Claude only |
| Codex sandbox | `--full-auto` (workspace-write filesystem sandbox, network open) | Codex only |
| Secret isolation | GH_TOKEN injected via tmux env, never written to disk | Secrets only |
| Token budget | Pre-flight + post-run check on `max_tokens_per_task` | Cost control only |

**What is not isolated today:**
- Host filesystem outside the worktree (accessible to all agents)
- Host environment variables and secrets (outside GH_TOKEN)
- Host network stack (agents can make arbitrary outbound connections)
- Host processes and system resources
- The `SandboxLevel::None` enum value in `agents/mod.rs` explicitly documents: *"No sandboxing (orch manages isolation externally)"* — this is the hook point for VM-level sandboxing.

---

## What Is Lume?

Lume is an open-source (MIT) CLI and HTTP API for creating and managing macOS and Linux VMs on Apple Silicon. It wraps Apple's [Virtualization Framework](https://developer.apple.com/documentation/virtualization) — the same technology that powers Claude Cowork (Anthropic's own sandboxed environment for Claude Code).

Key properties:
- **Near-native performance** — CPU instructions execute via hardware virtualization, not emulation
- **Sparse disk images** — VM disk only consumes actual usage, not allocated size (50 GB disk ≈ a few GB on disk for a fresh VM)
- **Clone in seconds** — `lume clone` creates a full VM copy via copy-on-write; resetting a sandbox is instant
- **Headless operation** — `lume run my-vm --no-display` runs fully unattended
- **HTTP API** — `lume serve` exposes a REST API on `localhost:7777` for programmatic VM lifecycle management
- **Shared folders** — `lume run my-vm --shared-dir /path` mounts a host directory at `/Volumes/My Shared Files` inside the VM (read-write by default, can be read-only)
- **SSH access** — VMs get a local IP, accessible via SSH from the host
- **Unattended setup** — Fully automated macOS Setup Assistant via YAML config (VNC + OCR)
- **macOS only** — Requires Apple Silicon; does not work on Intel Macs

---

## Proposed Architecture

### How Lume Would Plug Into Orch

The core insight is that Orch's runner already separates **what to run** (agent invocation) from **where to run it** (tmux session on host). Lume adds a third option: **run inside a VM via SSH**.

The runner's lifecycle would change only in the execution phase:

```
Current:
  prepare_task() → spawn tmux session on host → monitor → collect output

With Lume:
  prepare_task() → claim VM from pool → rsync worktree into VM →
  spawn tmux session (SSH into VM) → monitor → rsync output back → release VM
```

Everything else — task routing, worktree setup, prompt building, output parsing, PR creation — stays exactly the same.

### Architecture Diagram

```
┌──────────────────────────────────────────────────────────────┐
│                     Host Mac (Orch)                          │
│                                                              │
│  ┌─────────────┐    ┌──────────────┐    ┌────────────────┐  │
│  │  Task Queue │ →  │ Runner       │ →  │  VM Pool       │  │
│  │  (SQLite)   │    │ (Rust)       │    │  (Lume)        │  │
│  └─────────────┘    └──────┬───────┘    └───────┬────────┘  │
│                            │ SSH                │            │
│                    ┌───────▼────────────────────▼────────┐  │
│                    │        macOS VM (Sandbox)            │  │
│                    │                                      │  │
│                    │  Agent runs here (claude/codex/...)  │  │
│                    │  Worktree mounted at /sandbox/work   │  │
│                    │  No access to host FS or secrets     │  │
│                    │  Network: configurable               │  │
│                    └──────────────────────────────────────┘  │
│                                                              │
│  ~/.orch/worktrees/{project}/{branch}/   ← git worktrees    │
│  ~/.lume/                                ← VM images        │
└──────────────────────────────────────────────────────────────┘
```

### VM Pool Design

For low-latency task dispatch, VMs should be pre-warmed:

1. **Golden image** — A pre-configured VM with all agent CLIs installed (claude, codex, opencode, gh, git, etc.) and SSH enabled. Built once with `lume create + unattended setup`.
2. **Warm pool** — `N` VMs cloned from the golden image, sitting idle and ready to accept work (e.g., `orch-sandbox-0`, `orch-sandbox-1`). Clone via `lume clone`.
3. **Claim/release** — Runner claims a free VM from the pool, runs the task, then resets by cloning from the golden image (or using a snapshot) and returning it to the pool.
4. **Pool size** — Start with 2-3 VMs; each requires ~4 CPU cores + 8 GB RAM for comfortable agent operation.

Clone time after initial setup is very fast (seconds) due to copy-on-write sparse images.

---

## Integration Points in Orch

### 1. `SandboxLevel::None` — The Hook Already Exists

In `src/engine/runner/agents/mod.rs`:

```rust
pub enum SandboxLevel {
    WorkspaceWrite,
    FullAccess,
    /// No sandboxing (orch manages isolation externally).
    None,
}
```

The `None` variant is the explicit integration point. When `SandboxLevel::None` is set, the agent runner itself applies no sandboxing — orch is expected to provide it externally. A new `SandboxLevel::VmIsolated` variant (or simply using `None` when a VM executor is active) would signal the runner to use the Lume-based execution path.

### 2. `agent.rs` — `spawn_in_tmux()` — SSH Variant Needed

Currently, `spawn_in_tmux()` creates a local tmux session running `bash runner.sh`. A Lume-aware variant would:

1. Get VM IP: `lume get {vm_name} --format json | jq -r '.ip'`
2. Sync worktree into VM: `rsync -a {worktree}/ lume@{vm_ip}:/sandbox/work/`
3. Copy runner script: `scp runner.sh lume@{vm_ip}:/tmp/`
4. Create tmux session that SSHs into VM: `tmux new-session ... "ssh lume@{vm_ip} bash /tmp/runner.sh"`
5. Or alternatively: open an SSH tunnel and run the session directly inside the VM via `ssh -t lume@{vm_ip} tmux new-session ...`

The monitoring path (`wait_for_completion`, exit code collection) can stay unchanged — it polls the local tmux session which happens to be an SSH connection into the VM.

### 3. `worktree.rs` — Shared Folder vs. rsync

Two options for getting the worktree into the VM:

**Option A: Shared folder (simpler)**
```
lume run {vm} --no-display --shared-dir ~/.orch/worktrees/{project}/{branch}/
```
Inside VM: `/Volumes/My Shared Files/` — read-write. Agent edits files there directly. No sync needed; changes are immediately visible on the host.

*Pros:* No rsync step; output is immediately available.  
*Cons:* The shared folder is accessible from both host and VM simultaneously; a compromised agent could still read host files that happen to be in the shared directory.

**Option B: rsync (stronger isolation)**
```
rsync -a ~/.orch/worktrees/{project}/{branch}/ lume@{vm_ip}:/sandbox/work/
# ... agent runs ...
rsync -a lume@{vm_ip}:/sandbox/work/ ~/.orch/worktrees/{project}/{branch}/
```

*Pros:* VM has no direct access to host filesystem at any point. Stronger isolation.  
*Cons:* Adds rsync steps before and after; output collection requires explicit sync.

**Recommendation:** Start with Option A (shared folder) for simplicity. Move to Option B if stricter isolation is needed.

### 4. Secret Injection — SSH Environment

Currently, `GH_TOKEN` is injected via `tmux set-environment` — never written to disk. This model continues cleanly with SSH: inject secrets into the SSH session environment, not into the runner script:

```bash
# Pass GH_TOKEN only via SSH environment (not in runner.sh or disk)
ssh -o SendEnv=GH_TOKEN lume@{vm_ip} "bash /tmp/runner.sh"
```

The VM's `sshd_config` needs `AcceptEnv GH_TOKEN` — configurable during golden image setup.

Alternatively, use `ssh -t` + tmux inside the VM and inject via `tmux set-environment` inside the VM session, mirroring the current host-side mechanism exactly.

### 5. Config — New `sandbox` Mode

A new config option to enable VM sandboxing:

```yaml
# In .orch.yml or config.yml
workflow:
  sandbox: vm   # "none" | "workspace-write" | "vm"

agents:
  lume:
    pool_size: 3
    golden_image: orch-sandbox-golden
    vm_prefix: orch-sandbox
    vm_cpu: 4
    vm_memory: 8GB
    ssh_user: lume
    ssh_password: lume   # or key-based auth
```

---

## Golden Image Setup

A one-time setup procedure to build the base VM. This would be documented as an operational runbook:

```bash
# 1. Create base VM (15-20 min, fully automated)
lume create orch-sandbox-golden \
  --os macos \
  --ipsw latest \
  --unattended tahoe \
  --cpu 4 \
  --memory 8GB \
  --disk-size 80GB

# 2. SSH in and install agent CLIs
lume run orch-sandbox-golden --no-display
VM_IP=$(lume get orch-sandbox-golden --format json | jq -r '.ip')

ssh lume@$VM_IP << 'EOF'
  # Install Homebrew
  /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
  
  # Install agent CLIs
  brew install gh git node
  npm install -g @anthropic-ai/claude-code   # claude
  npm install -g opencode                     # opencode
  npm install -g @openai/codex               # codex (if npm-distributed)
  
  # Configure SSH to accept GH_TOKEN injection
  echo "AcceptEnv GH_TOKEN GITHUB_TOKEN" | sudo tee -a /etc/ssh/sshd_config
  sudo launchctl kickstart -k system/com.openssh.sshd
EOF

# 3. Stop and snapshot
lume stop orch-sandbox-golden

# 4. Pre-warm pool
lume clone orch-sandbox-golden orch-sandbox-0
lume clone orch-sandbox-golden orch-sandbox-1
lume clone orch-sandbox-golden orch-sandbox-2
```

Resetting a VM to clean state after task completion:

```bash
lume stop orch-sandbox-0
lume delete orch-sandbox-0
lume clone orch-sandbox-golden orch-sandbox-0
lume run orch-sandbox-0 --no-display
```

---

## Security Improvement Analysis

| Attack Vector | Current Risk | With Lume VM |
|---|---|---|
| Agent reads arbitrary host files | High — only tool blocklist prevents it | Eliminated — VM has no access to host FS |
| Agent exfiltrates host secrets | Medium — env is filtered, but shell is open | Eliminated — VM env is isolated |
| Agent installs malware on host | Medium — can run arbitrary bash | Eliminated — changes stay in VM |
| Agent modifies host git repos | Medium — tool blocklist only | Eliminated — host git not accessible |
| Agent makes unauthorized network calls | Not mitigated | Not mitigated (network is open by default) |
| Runaway resource usage | Not mitigated | Mitigated — VM has fixed CPU/RAM limits |
| Cross-task contamination | Low — separate worktrees | Eliminated — VM reset between tasks |

Network access inside the VM is open by default (required for agent operation: package installs, API calls). Network restriction would require additional macOS firewall configuration inside the VM, which is possible but complex.

---

## Tradeoffs and Limitations

### Pros

- **True hardware-level isolation** — Apple Silicon hardware virtualization, not software sandboxing
- **Near-native performance** — No emulation overhead
- **Clean reset** — Clone from golden image = guaranteed clean state, no state leakage between tasks
- **macOS environment** — Agents work identically to host; no porting or compatibility concerns
- **Anthropic uses this** — Claude Cowork runs on Apple's Virtualization Framework for the same purpose
- **MIT license** — Open source, auditable, no vendor lock-in
- **HTTP API** — Scriptable from Rust via simple HTTP calls to `localhost:7777`

### Cons / Risks

- **Apple Silicon only** — Breaks on Intel Macs. Orch currently runs on Apple Silicon in production, but this is a hard constraint.
- **macOS licensing** — macOS VMs must run on Apple hardware, limited to 2 concurrent macOS VM instances per Mac per Apple's SLA. **Linux VMs have no such limit.**
- **Resource overhead** — Each VM needs ~4 CPU + 8 GB RAM. A 3-VM pool requires 12 CPU + 24 GB RAM in addition to the host. On an M4 Max (128 GB, 16 cores) this is fine; on a base M2 (16 GB) it is limiting.
- **Cold start latency** — Booting a stopped VM takes 30-60 seconds. Pre-warming the pool mitigates this; tasks wait only if the pool is exhausted.
- **Operational complexity** — Requires maintaining the golden image (re-baking when agent CLIs update), pool management, and SSH key distribution.
- **Network is still open** — VM isolation protects the host but does not prevent the agent from making outbound network calls. Acceptable for current use case.

---

## Linux VM Alternative

Apple limits macOS VMs to 2 concurrent instances per Mac. For higher parallelism, **Linux VMs are the correct path**:

- No instance limit
- Lighter resource footprint
- Agents (claude, codex, opencode) are all cross-platform and run on Linux
- Same Lume API: `lume create ubuntu-vm --os linux --cpu 2 --memory 4GB`

Linux VMs require a one-time manual installation (boot from ISO, run installer), but once a golden image is built, cloning is instant and identical to macOS.

**Recommendation:** Use macOS VMs for full compatibility with agent CLIs that may have macOS-specific behavior (e.g., Codex's macOS sandbox). Use Linux VMs for high-concurrency workloads.

---

## Implementation Roadmap

### Phase 1: Proof of Concept (manual, no Orch changes)

Goal: Validate that an agent runs correctly inside a Lume VM with the same output as host execution.

1. Build golden image manually (see setup steps above)
2. Start a VM with a worktree shared via `--shared-dir`
3. SSH in and run a `runner.sh` script manually
4. Verify output collection works

Estimated effort: 1-2 days.

### Phase 2: Runner Integration (Orch changes)

Goal: Add `SandboxMode::Vm` to the runner, backed by a simple VM pool.

Key changes:
- `src/engine/runner/agents/mod.rs` — Add `SandboxLevel::VmIsolated` variant
- `src/engine/runner/agent.rs` — Add `spawn_in_vm_tmux()` as a sibling to `spawn_in_tmux()`
- `src/engine/runner/vm_pool.rs` (new) — VM pool: claim, release, reset operations via Lume HTTP API
- `src/config.rs` — Add `agents.lume` config section
- `src/engine/runner/mod.rs` — Branch on `sandbox == VmIsolated` to use VM path

Estimated effort: 3-5 days.

### Phase 3: Hardening

- SSH key-based auth instead of password
- Read-only shared folder option (rsync-based output collection)
- VM pool auto-scaling (clone on demand, delete when idle)
- Metrics: track VM claim latency, reset time, pool utilization

---

## Conclusion

Lume is an excellent match for Orch's sandboxing needs. The architecture is compatible with Orch's existing runner pipeline with minimal changes. The security improvement — moving from tool-level blocklists to hardware VM isolation — is substantial and addresses the most significant remaining risks in the current model.

The main constraint is Apple Silicon requirement and the 2-concurrent-macOS-VM limit. For production use with higher parallelism, the path forward is Linux VMs, which have no instance limit and lower resource overhead.

**Recommended next step:** Build the Phase 1 proof of concept to validate the golden image + shared-folder approach end-to-end before committing to the Phase 2 implementation work.

---

## References

- [Lume — What is it?](https://cua.ai/docs/lume/guide/getting-started/introduction)
- [Lume — Quickstart](https://cua.ai/docs/lume/guide/getting-started/quickstart)
- [Lume — VM Management](https://cua.ai/docs/lume/guide/fundamentals/vm-management)
- [Lume — Unattended Setup](https://cua.ai/docs/lume/guide/fundamentals/unattended-setup)
- [Lume — HTTP API](https://cua.ai/docs/lume/guide/advanced/http-server)
- [Lume — Claude Code Sandbox Example](https://cua.ai/docs/lume/examples/claude-code/sandbox)
- [Lume — CLI Reference](https://cua.ai/docs/lume/reference/cli-reference)
- [Apple Virtualization Framework](https://developer.apple.com/documentation/virtualization)
- [trycua/cua on GitHub](https://github.com/trycua/cua)
- Orch source: `src/engine/runner/` — agent lifecycle
- Orch source: `src/engine/runner/agents/mod.rs` — `SandboxLevel` enum
