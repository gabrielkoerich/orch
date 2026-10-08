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
3. `git worktree add --no-checkout <dir> <branch>` creates the gitdir entry and the `.git` file.
4. Clone every top-level template entry except `.git` into `<dir>`. macOS runs `cp -cRp` (`clonefile`). Linux runs `cp -a --reflink=always` (`FICLONE`).
5. Copy the template index into the new gitdir, run `git update-index --refresh`, then `git reset --hard`. Only files where the task branch differs from the template commit are rewritten.

Any failure removes the half-built worktree and falls back to `git worktree add`. Resume, saved branch, parent branch inheritance, corrupted index recovery and cleanup are unchanged. They only see an existing directory with a valid gitdir.

## Decisions

| Decision | Rejected alternative |
|----------|---------------------|
| Call `cp`, one code path per OS | Direct `clonefile` and `ioctl` calls need `libc` and unsafe code for no gain |
| Template is a linked worktree of the project repo | A separate clone doubles the object store and needs its own fetch |
| Clone files, keep the gitdir per worktree | Cloning `.git` would share index and HEAD state |
| Probe by cloning a small file | Parsing filesystem names misses mount options like XFS without reflink |
| One lock per template | A global lock lets one slow install stall every project |
| Template is a plain checkout, no dependency install | Orch must work with any language. Task agents install and build in their own worktree, following the project's `AGENTS.md` |

## Costs

- Clones share source files only. Each task installs its own dependencies and builds as before.
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

Not measured: end to end task setup on a large JS repo and a large Rust repo, and Linux. Linux reflink (XFS, Btrfs) is untested and has no CI coverage. Unit tests on Ubuntu take the fallback branch. The `test-cow-macos` job runs the clone path and fails if the probe finds no clone support.

## Operating

Set `ORCH_COW_WORKTREES=0` to disable the clone path. To reset a template, delete `~/.orch/state/templates/<project>`. The next task recreates it.
