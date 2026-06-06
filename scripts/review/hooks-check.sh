#!/usr/bin/env bash
# Pre-flight scan for an external-contributor PR: refuse to proceed if the diff
# touches any file that runs code outside the obvious source paths — Cargo/build
# config, Cargo manifests, agent-tool hooks, CI workflows, shell scripts, etc.
#
# Usage: hooks-check.sh <PR-NUMBER> [REPO]
#   REPO defaults to gabrielkoerich/orch.
#
# Exits 0 if the diff is clean. Exits 1 if any tripwire file is touched.
# Caller decides whether to require `--force` to continue.

set -euo pipefail

pr_number="${1:?PR number required}"
repo="${2:-gabrielkoerich/orch}"

# Tripwires. Anything matching these patterns means the PR can introduce
# arbitrary code execution outside the application's normal call graph.
# Patterns are matched against `gh pr diff --name-only` via grep -E.
TRIPWIRES=(
  # Cargo / build customization
  '^\.cargo/'                            # config.toml, credentials.toml — runner/build hooks
  '^Cargo\.toml$'                        # any new dependency
  '^Cargo\.lock$'                        # transitive dep drift, source-replacement
  '(^|/)build\.rs$'                      # arbitrary code at compile time

  # Agent / IDE / coding-assistant hooks (any of these can run on a maintainer's
  # machine if they open the repo in their editor / agent)
  '^\.claude/'                           # claude-code hooks, agents, slash commands, settings
  '^\.codex/'                            # codex config
  '^\.opencode/'                         # opencode config
  '^\.orch-'                             # orch agent configs
  '^\.cursor/'                           # cursor rules
  '^\.continue/'                         # continue.dev config
  '^\.aider'                             # aider config (.aider.conf.yml etc.)
  '^\.zed/'                              # zed tasks
  '^\.vscode/'                           # tasks.json, launch.json — can run on folder open
  '^\.idea/'                             # JetBrains run configs
  '^\.devcontainer/'                     # dev container config

  # Generic pre-commit / git hooks
  '^\.husky/'
  '^lefthook\.ya?ml$'
  '^\.pre-commit-config\.ya?ml$'
  '^\.git/'                              # should never be in a PR but check anyway

  # Shell / task runners
  '\.sh$'
  '^Makefile$'
  '^justfile$'
  '^Justfile$'
  '^Taskfile\.ya?ml$'

  # Toolchain shims
  '^\.tool-versions$'
  '^mise\.toml$'
  '^\.mise\.toml$'
  '^rust-toolchain(\.toml)?$'
  '^\.envrc$'                            # direnv

  # CI
  '^\.github/workflows/'                 # see "CI does not run on external PRs" in AGENTS.md
  '^\.github/actions/'

  # Containers
  '^Dockerfile'
  '^docker-compose'
  '^\.dockerignore$'

  # Node — `npm install` runs scripts from package.json
  '^package\.json$'
  '^package-lock\.json$'
  '^pnpm-lock\.yaml$'
  '^yarn\.lock$'
)

files=$(gh pr diff "$pr_number" -R "$repo" --name-only)

if [[ -z "$files" ]]; then
  echo "PR #$pr_number: empty diff?" >&2
  exit 1
fi

hits=()
for pat in "${TRIPWIRES[@]}"; do
  while IFS= read -r f; do
    [[ -n "$f" ]] && hits+=("  $pat  →  $f")
  done < <(grep -E "$pat" <<<"$files" || true)
done

if [[ ${#hits[@]} -eq 0 ]]; then
  echo "PR #$pr_number — hooks check: clean"
  exit 0
fi

cat >&2 <<EOF
PR #$pr_number — hooks check: TRIPWIRES TRIPPED

The diff touches files that can execute code outside the application's
normal call graph. Read each one in full before proceeding. Any new
dependency in Cargo.toml is a hard NO per AGENTS.md.

EOF

printf '%s\n' "${hits[@]}" >&2
exit 1
