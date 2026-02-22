#!/usr/bin/env bash
set -euo pipefail

AGENTS=(claude codex opencode)

for agent in "${AGENTS[@]}"; do
  if command -v "$agent" >/dev/null 2>&1; then
    version=$("$agent" --version 2>/dev/null | head -1 || echo "unknown")
    printf '  %-10s  ✓  %s\n' "$agent" "$version"
  else
    printf '  %-10s  ✗  not installed\n' "$agent"
  fi
done
