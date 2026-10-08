+++
title = "Copy-on-write worktrees"
description = "How task worktrees share files with a per-project template"
weight = 4
+++

## What it is

Orch keeps one template checkout per project at `~/.orch/state/templates/<project>`. A new task worktree gets its files from a copy-on-write clone of the template. Clones share disk blocks with the template until one side writes. The worktree stays at `~/.orch/worktrees/<project>/<branch>/` and is a normal linked git worktree.

Implementation: `src/engine/runner/cow.rs`, called from `setup_worktree()`.

## How it works

1. Probe once per process whether the template dir and the worktrees dir can clone files. If not, use `git worktree add`.
2. Under a per-template lock, move the template to `origin/<default>` (`git checkout --detach --force`). Create it with `git worktree add --detach` if missing or broken.
3. Warm the template. Clone each gitignored directory of the operator's checkout, at any depth (`git ls-files --others --ignored --exclude-standard --directory`), into a staging dir beside the template, then swap it into the same relative path in the template. A path with a hidden component (a name starting with `.`) is skipped. Loose ignored files such as `.env` are not copied. Bare clones have no checkout and skip this step.
4. `git worktree add --no-checkout <dir> <branch>` creates the gitdir entry and the `.git` file.
5. Clone every top-level template entry except `.git` into `<dir>`. macOS runs `cp -cRp` (`clonefile`). Linux runs `cp -a --reflink=always` (`FICLONE`).
6. Copy the template index into the new gitdir, run `git update-index --refresh`, then `git reset --hard`. Only files where the task branch differs from the template commit are rewritten.

After a task PR merges and before its worktree is removed, projects that are bare clones warm the template from that worktree the same way (`warm_from_merged()`, called from the cleanup path for `done` tasks). Failed or blocked tasks never warm the template, since they can leave broken output.

The agent then runs its usual install or build. The tool sees most output as current and rewrites only what changed, so only those blocks are copied. `git checkout --detach --force` keeps ignored files and orch never runs `git clean`.

Any failure removes the half-built worktree and falls back to `git worktree add`. Resume, saved branch, parent branch inheritance, corrupted index recovery and cleanup are unchanged. They only see an existing directory with a valid gitdir.

## Decisions

| Decision | Rejected alternative |
|----------|---------------------|
| Call `cp`, one code path per OS | Direct `clonefile` and `ioctl` calls need `libc` and unsafe code for no gain |
| Template is a linked worktree of the project repo | A separate clone doubles the object store and needs its own fetch |
| Clone files, keep the gitdir per worktree | Cloning `.git` would share index and HEAD state |
| Probe by cloning a small file | Parsing filesystem names misses mount options like XFS without reflink |
| One lock per template | A global lock lets one slow install stall every project |
| Template holds the checkout's ignored directories | Orch runs no install or build. It reuses what the project's own tooling already produced, so `cow.rs` names no language or tool |
| Skip any path with a hidden component | Hidden ignored directories hold local tool and agent config, and a settings file there could change a task agent's permissions. The rule names no tool |
| Directories only, no loose ignored files | Keeps `.env` and similar files out of agent worktrees without naming any file |
| Warm from the checkout, fall back to merged tasks | Output from failed tasks can be broken |
| One clone per task, per-worktree build dirs | A shared build dir (e.g. `CARGO_TARGET_DIR`) names a tool, serialises builds on its lock and lets branches overwrite each other |

## Costs

- Clones share source files, dependencies and build output. Blocks stay shared until the agent's build rewrites them.
- Hidden dependency directories such as virtualenvs are not shared. They do not survive a move anyway.
- Every task creation re-clones the checkout's ignored directories (copy-on-write, so no disk, but time grows with file count).
- The operator may be building while orch clones, so the template can hold partial output. The agent's tooling must detect that and rebuild. Orch does not check.
- Output that embeds absolute paths (virtualenvs, some build caches) points at the checkout or template path. Whether it recovers depends on the tool, and orch does not special-case any. The agent reinstalls if needed.
- The template only grows with the checkout's ignored directories, and each refresh replaces them whole, so stale files do not pile up. Delete the template to reset it.
- Merged-task warming overwrites a directory with the one from the last merged task.
- The first task per project pays for the template checkout. Later tasks for the same project wait on the lock while a refresh runs. Other projects are not blocked.
- The template is one more full checkout on disk, and it appears in `git worktree list`.
- Writes to a cloned file copy its blocks.
- `git update-index --refresh` re-hashes the cloned files, because the clones have new inodes.
- Projects with submodules always use the fallback.
- ext4, HFS+ and XFS without reflink always use the fallback, after one failed probe per process.

## Measurements

Apple Silicon, APFS, `cp` of a 1.9 GB directory (the `target/` of this repo):

| Method | Time | Extra disk |
|--------|------|-----------|
| `cp -cRp` (clone) | 0.7 s | about 0 (free space unchanged) |
| `cp -Rp` (plain copy) | 5.6 s | about 2.2 GB |

`git update-index --refresh` on 518 tracked files took under 10 ms. The cost grows with tracked file count, since every cloned file has a new inode and gets re-hashed.

Not measured yet (still open in #3682): the warm-up skip when the source did not change, and the free-space drop (`df`) for N worktrees before and after a first build, plain vs warmed. Whether a first `cargo build` in a warmed clone recompiles only changed crates (mtimes are kept by `cp -p`/`-a`, and `git reset --hard` only rewrites files that differ, but the package path differs from the template path). End to end task setup on a large JS repo and a large Rust repo. Linux timings.

CI coverage: `test-cow-macos` (APFS) and `test-cow-linux` (XFS with `reflink=1` on a loop volume, `TMPDIR` on the mount) run the clone path. Both set `ORCH_COW_EXPECT_CLONE=1`, so the tests fail if the probe finds no clone support. They also check that ignored directories from the checkout reach the template and the clone, that a merged task's ignored directories reach the template of a bare clone, and that `.git` and loose ignored files are never copied. Plain unit tests on ext4 take the fallback branch.

On Linux, `cp -a --reflink=always <template>/<entry> <dir>` copies the entry into the existing `<dir>` without nesting. `.git` is skipped, so the `.git` file is never overwritten. The tests check this through the resulting file contents and a clean `git status`.

## Operating

Set `ORCH_COW_WORKTREES=0` to disable the clone path. To reset a template, delete `~/.orch/state/templates/<project>`. The next task recreates it.
