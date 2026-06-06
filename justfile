# Orch task runner. Run `just --list` for the full menu.
#
# External-contributor PR review lives here too — see `review-pr-*`.

set shell := ["bash", "-euo", "pipefail", "-c"]

# Default repo used by the review recipes. Override per-invocation:
#   just review-pr 3279 owner/repo
default_repo := "gabrielkoerich/orch"

# ── External-contributor PR review ────────────────────────────────────────────
# Two-container architecture so the fork's bytes never run on the host:
#
#   Stage A (review-pr-fetch):  network ON, no compilation.
#                               `git clone` the fork + `cargo fetch --locked`.
#                               build.rs / proc-macros do NOT execute here.
#   Stage B (review-pr-run):    --network=none, runs the actual `cargo` work.
#                               Untrusted code first executes here, with no
#                               network egress, no host mounts, no host creds.
#
# Volume strategy:
#   orch-review-src       — scrubbed every run (always fresh per PR)
#   orch-review-registry  — persistent crates.io cache, reused across reviews
#                           (registry content is hash-pinned via Cargo.lock,
#                           safe to share). Wipe with `just review-pr-clean`.
#   orch-review-target    — persistent target/ dir, reused across reviews
#                           (incremental compilation; same scrubbing rules).
#
# We deliberately do NOT persist `CARGO_HOME/git/` — git deps come from
# attacker-controllable URLs and must never be reused across PRs.

review-images:
    docker build -t orch-review-fetch -f scripts/review/Dockerfile.fetch scripts/review
    docker build -t orch-review-run   -f scripts/review/Dockerfile.run   scripts/review

# Pre-flight: refuse to clone anything if the diff touches Cargo manifests,
# build.rs, agent/IDE hooks, CI workflows, shell scripts, etc.
# Run this BEFORE `review-pr` and read the diff in `gh pr diff <N>` regardless.
review-pr-hooks-check pr repo=default_repo:
    bash scripts/review/hooks-check.sh {{pr}} {{repo}}

# Orchestrates the two-stage review for one PR. Cleans up volumes on exit
# (success or failure). The host shell never touches the fork's source.
review-pr pr repo=default_repo: review-images
    #!/usr/bin/env bash
    set -euo pipefail

    pr="{{pr}}"
    repo="{{repo}}"

    echo ">>> [1/4] Static-review pre-flight (hooks check)"
    set +e
    bash scripts/review/hooks-check.sh "$pr" "$repo"
    rc=$?
    set -e
    case $rc in
      0) ;;  # clean, proceed
      1)
        echo
        echo "Refusing to proceed. Read every flagged file in 'gh pr diff $pr -R $repo'."
        echo "Re-run with: just review-pr-force $pr $repo  (acknowledges the risk)"
        exit 1
        ;;
      *)
        echo
        echo "Could not fetch PR diff (host network or gh auth issue). Retry shortly."
        exit 2
        ;;
    esac

    echo ">>> [2/4] Resolving fork URL and head ref for PR #$pr"
    fork_owner=$(gh pr view "$pr" -R "$repo" --json headRepositoryOwner --jq '.headRepositoryOwner.login')
    fork_name=$(gh pr view "$pr" -R "$repo" --json headRepository --jq '.headRepository.name')
    head_ref=$(gh pr view "$pr" -R "$repo" --json headRefName --jq '.headRefName')
    fork_url="https://github.com/${fork_owner}/${fork_name}.git"
    echo "    fork: $fork_url"
    echo "    ref:  $head_ref"

    # Always scrub source + git-deps volumes. Persistent crates.io cache
    # and target/ dir survive across reviews to keep iteration fast.
    trap 'docker volume rm -f orch-review-src orch-review-git >/dev/null 2>&1 || true' EXIT
    docker volume rm -f orch-review-src orch-review-git >/dev/null 2>&1 || true
    docker volume create orch-review-src      >/dev/null
    docker volume create orch-review-git      >/dev/null
    docker volume create orch-review-registry >/dev/null
    docker volume create orch-review-target   >/dev/null

    echo ">>> [3/4] Stage A — clone + cargo fetch (network ON, no compile)"
    docker run --rm \
        -v orch-review-src:/src \
        -v orch-review-registry:/cargo-home/registry \
        -v orch-review-git:/cargo-home/git \
        -e FORK_URL="$fork_url" -e HEAD_REF="$head_ref" \
        orch-review-fetch

    echo ">>> [4/4] Stage B — cargo build + nextest run (NETWORK OFF)"
    # CARGO_BUILD_JOBS limits parallel rustc/link to fit Docker Desktop's
    # default memory. Raise it if you've bumped Docker memory.
    docker run --rm \
        --network=none \
        -v orch-review-src:/src \
        -v orch-review-registry:/cargo-home/registry \
        -v orch-review-git:/cargo-home/git \
        -v orch-review-target:/src/repo/target \
        -e CARGO_BUILD_JOBS=2 \
        orch-review-run

# Same as review-pr but bypasses the hooks-check tripwire. Use only after
# reading every flagged file in 'gh pr diff'. Logs the acknowledgement.
review-pr-force pr repo=default_repo: review-images
    @echo "ACK: bypassing hooks-check for PR #{{pr}} in {{repo}} at $(date -u +%FT%TZ)" \
        | tee -a scripts/review/.bypass.log
    @just _review-pr-no-check {{pr}} {{repo}}

_review-pr-no-check pr repo=default_repo: review-images
    #!/usr/bin/env bash
    set -euo pipefail

    pr="{{pr}}"
    repo="{{repo}}"

    fork_owner=$(gh pr view "$pr" -R "$repo" --json headRepositoryOwner --jq '.headRepositoryOwner.login')
    fork_name=$(gh pr view "$pr" -R "$repo" --json headRepository --jq '.headRepository.name')
    head_ref=$(gh pr view "$pr" -R "$repo" --json headRefName --jq '.headRefName')
    fork_url="https://github.com/${fork_owner}/${fork_name}.git"

    trap 'docker volume rm -f orch-review-src orch-review-git >/dev/null 2>&1 || true' EXIT
    docker volume rm -f orch-review-src orch-review-git >/dev/null 2>&1 || true
    docker volume create orch-review-src      >/dev/null
    docker volume create orch-review-git      >/dev/null
    docker volume create orch-review-registry >/dev/null
    docker volume create orch-review-target   >/dev/null

    docker run --rm \
        -v orch-review-src:/src \
        -v orch-review-registry:/cargo-home/registry \
        -v orch-review-git:/cargo-home/git \
        -e FORK_URL="$fork_url" -e HEAD_REF="$head_ref" \
        orch-review-fetch

    docker run --rm \
        --network=none \
        -v orch-review-src:/src \
        -v orch-review-registry:/cargo-home/registry \
        -v orch-review-git:/cargo-home/git \
        -v orch-review-target:/src/repo/target \
        -e CARGO_BUILD_JOBS=2 \
        orch-review-run

# Wipe the persistent crates.io / target caches. Run if you suspect cache
# poisoning (e.g. switching between unrelated forks with different git deps)
# or if the cache has grown large.
review-pr-clean:
    docker volume rm -f orch-review-src orch-review-git orch-review-registry orch-review-target
    @echo "Review caches removed."

# Dispatch a workflow on this repo against a fork PR's head ref via
# `gh workflow run`. Requires the target workflow to declare
# `workflow_dispatch:` — otherwise GitHub rejects the call.
#
#   just review-pr-ci 3279                       # dispatches release.yml
#   just review-pr-ci 3279 integration-tests.yml # dispatches a specific one
#
# Static review still has to happen first — this just hands you the button.
review-pr-ci pr workflow="release.yml" repo=default_repo:
    gh workflow run {{workflow}} -R {{repo}} --ref refs/pull/{{pr}}/head

# Hermetic-test detector. Runs the FULL test suite of the current branch
# (NOT a PR — for catching drift on our own tree) inside the same
# --network=none stage-B container. Any test that fails here is a test that
# silently depends on network, file-system locations outside the worktree, or
# host services. Use this on `main` periodically to catch drift, and after
# adding any new test that uses an HTTP client / external process.
hermetic-tests: review-images
    #!/usr/bin/env bash
    set -euo pipefail
    trap 'docker volume rm -f orch-hermetic-src orch-hermetic-cargo >/dev/null 2>&1 || true' EXIT
    docker volume rm -f orch-hermetic-src orch-hermetic-cargo >/dev/null 2>&1 || true
    docker volume create orch-hermetic-src   >/dev/null
    docker volume create orch-hermetic-cargo >/dev/null

    # Stage A — copy the current worktree into the volume + cargo fetch.
    # We use a tar pipe instead of `git clone .` so uncommitted changes are
    # exercised too.
    git ls-files -z | tar -cf - --null -T - \
      | docker run --rm -i \
          -v orch-hermetic-src:/src \
          -v orch-hermetic-cargo:/cargo-home \
          --entrypoint /bin/bash \
          orch-review-fetch \
          -euxc 'mkdir -p /src/repo && tar -xf - -C /src/repo && cd /src/repo && cargo fetch --locked'

    # Stage B — run tests offline. Failures here = non-hermetic tests.
    # CARGO_BUILD_JOBS=2 fits Docker Desktop's default memory ceiling; without it
    # the linker is parallelized into SIGKILL.
    docker run --rm \
        --network=none \
        -v orch-hermetic-src:/src \
        -v orch-hermetic-cargo:/cargo-home \
        -e CARGO_BUILD_JOBS=2 \
        orch-review-run \
        'cargo nextest run --offline --no-fail-fast'
