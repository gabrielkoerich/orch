Why this check exists

This repository serves content for the Zola static site generator under docs/content/. Merged markdown files must have valid front-matter (TOML or YAML) for Zola to build the site. A lightweight CI check validates front-matter structure on every PR so that merges don't break the docs build.

What the check does

- Scans docs/content/**/*.md
- Ensures the first non-empty line is a front-matter delimiter: +++ (TOML) or --- (YAML)
- Ensures a matching closing delimiter exists later in the file

Why a script (not zola build)

Running `zola build` is authoritative but can be slower and depends on the full build environment. This fast script is deterministic, quick, and focused on catching the most common class of human errors (missing or unclosed front-matter blocks) before CI runs heavier checks.

If you encounter a failure

1. Inspect the failure message in the job log — it includes the offending file path and a short description.
2. Fix the front-matter (add +++/--- and a closing delimiter) and push a new commit.
