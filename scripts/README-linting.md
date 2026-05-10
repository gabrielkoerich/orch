Repository scripts for front-matter linting

- scripts/lint-frontmatter.sh: fast front-matter validator used by CI
- scripts/tests/test-lint-frontmatter.sh: simple smoke test to exercise the linter

Run tests locally:

chmod +x scripts/lint-frontmatter.sh scripts/tests/test-lint-frontmatter.sh
./scripts/tests/test-lint-frontmatter.sh
