#!/usr/bin/env bash
set -euo pipefail

# Fast front-matter linter for docs/content
# Scans all .md files under the provided path (default: docs/content)
# Ensures each file has a front-matter block starting with '+++' or '---'
# and a matching closing delimiter. Exits non-zero with clear messages
# when a file is missing front-matter or has an unclosed block.

ROOT=${1:-docs/content}
RC=0

if [ ! -d "$ROOT" ]; then
  echo "Path not found: $ROOT" >&2
  exit 0
fi

while IFS= read -r -d '' file; do
  # Find first non-empty line number
  first_ln=$(awk '/[^ \t]/ {print NR; exit}' "$file" || true)
  if [ -z "$first_ln" ]; then
    echo "ERROR: Empty file (no content) -> $file"
    RC=1
    continue
  fi

  first_line=$(sed -n "${first_ln}p" "$file")

  if [ "$first_line" != "+++" ] && [ "$first_line" != "---" ]; then
    echo "ERROR: Missing front-matter start delimiter in $file (first non-empty line ${first_ln}):"
    echo "  ${first_line}"
    RC=1
    continue
  fi

  delim="$first_line"
  # Search for a matching closing delimiter after the first line
  closing_ln=$(awk -v start=$((first_ln + 1)) -v d="$delim" 'NR>=start && $0==d {print NR; exit}' "$file" || true)
  if [ -z "$closing_ln" ]; then
    echo "ERROR: Unclosed front-matter in $file (started at line ${first_ln}, delimiter: ${delim})"
    RC=1
    continue
  fi

  # All good for this file; continue
done < <(find "$ROOT" -type f -name '*.md' -print0)

if [ $RC -ne 0 ]; then
  echo "\nFront-matter linter failed. See errors above." >&2
  exit $RC
fi

echo "Front-matter linter: ok"
exit 0
