#!/usr/bin/env bash
set -euo pipefail

# Simple unit tests for lint-frontmatter.sh
TMPDIR=$(mktemp -d)
cleanup() { rm -rf "$TMPDIR"; }
trap cleanup EXIT

mkdir -p "$TMPDIR/docs/content/sub"

# Good file with +++
echo -e "+++\n+++\nGood content" > "$TMPDIR/docs/content/good1.md"

# Good file with ---
echo -e "---\n---\nGood content" > "$TMPDIR/docs/content/good2.md"

# Bad file: missing front matter
echo "Some text without front matter" > "$TMPDIR/docs/content/bad1.md"

# Bad file: unclosed front matter
echo "+++" > "$TMPDIR/docs/content/sub/bad2.md"

set +e
"$(pwd)/scripts/lint-frontmatter.sh" "$TMPDIR/docs/content"
rc=$?
set -e

if [ $rc -eq 0 ]; then
  echo "Test failed: expected non-zero exit code" >&2
  exit 2
fi

echo "Test passed: linter detected errors as expected"
exit 0
