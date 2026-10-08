+++
title = "Copy-on-write worktrees"
description = "How task worktrees share files with a per-project template"
weight = 4
+++

## What it is

Orch keeps one template checkout per project at `~/.orch/state/templates/<project>`, with dependencies installed. A new task worktree gets its files from a copy-on-write clone of the template. Clones share disk blocks with the template until one side writes. The worktree stays at `~/.orch/worktrees/<project>/<branch>/` and is a normal linked git worktree.

Implementation: `src/engine/runner/cow.rs`, called from `setup_worktree()`.

## How it works

1. Probe once per process whether the template dir and the worktrees dir can clone files. If not, use `git worktree add`.
2. Under a per-template lock, move the template to `origin/<default>` (`git checkout --detach --force`). Create it with `git worktree add --detach` if missing or broken.
3. If a lockfile changed since the last attempt, run its install command in the template. The outcome and lockfile hash are stored in `<template>.deps.<lockfile>` as `ok:<hash>` or `fail:<hash>`. A failed hash is not retried until the lockfile changes. A timed out install is killed.
4. `git worktree add --no-checkout <dir> <branch>` creates the gitdir entry and the `.git` file.
5. Clone every top-level template entry except `.git`, `.venv` and `venv` into `<dir>`. macOS runs `cp -cRp` (`clonefile`). Linux runs `cp -a --reflink=always` (`FICLONE`).
6. Copy the template index into the new gitdir, run `git update-index --refresh`, then `git reset --hard`. Only files where the task branch differs from the template commit are rewritten.

Any failure removes the half-built worktree and falls back to `git worktree add`. Resume, saved branch, parent branch inheritance, corrupted index recovery and cleanup are unchanged. They only see an existing directory with a valid gitdir.

## Decisions

| Decision | Rejected alternative |
|----------|---------------------|
| Call `cp`, one code path per OS | Direct `clonefile` and `ioctl` calls need `libc` and unsafe code for no gain |
| Template is a linked worktree of the project repo | A separate clone doubles the object store and needs its own fetch |
| Clone files, keep the gitdir per worktree | Cloning `.git` would share index and HEAD state |
| Probe by cloning a small file | Parsing filesystem names misses mount options like XFS without reflink |
| `target/` and `node_modules/` are cloned | A reinstall or rebuild per task defeats the purpose. Cargo fingerprints workspace crates relative to the workspace root, so only those rebuild |
| One lock per template | A global lock lets one slow install stall every project |
| Record failed installs by lockfile hash | Retrying each task repeats a 30 minute timeout. A time based backoff is arbitrary |
| Python gets no template | `.venv` holds absolute paths. Run `uv sync` in the task worktree, it hardlinks from the shared uv cache. Not measured here |
| `.venv` and `venv` are skipped | They hold absolute paths in scripts and shebangs |

Install commands, the first JS lockfile found wins: `bun.lock`/`bun.lockb` runs `bun install --frozen-lockfile`, `pnpm-lock.yaml` runs `pnpm install --frozen-lockfile`, `yarn.lock` runs `yarn install --frozen-lockfile`, `package-lock.json` runs `npm ci`. `Cargo.lock` also runs `cargo build --locked --all-targets`, so `target/` exists in the template. Go and Python get no preinstall.

## Costs

- The first task per project pays for the template checkout and install, up to 30 minutes per install. Later tasks for the same project wait on the lock while a refresh or install runs. Other projects are not blocked.
- The template is one more full checkout on disk, and it appears in `git worktree list`.
- Writes to a cloned file copy its blocks. A full reinstall or rebuild in a task stops sharing.
- `git update-index --refresh` re-hashes the cloned files, because the clones have new inodes.
- Projects with submodules always use the fallback.
- Cloned `target/` and `node_modules/` can hold absolute paths from the template. Tools that embed them rebuild.
- ext4, HFS+ and XFS without reflink always use the fallback, after one failed probe per process.

## Measurements

Apple Silicon, APFS, `cp` of the 1.9 GB `target/` of this repo (Rust, 518 tracked files):

| Method | Time | Extra disk |
|--------|------|-----------|
| `cp -cRp` (clone) | 0.7 s | about 0 (free space unchanged) |
| `cp -Rp` (plain copy) | 5.6 s | about 2.2 GB |

`git update-index --refresh` on 518 files took under 10 ms. The cost grows with tracked file count, since every cloned file has a new inode and gets re-hashed.

Not measured: a large JS repo, Linux, `uv sync` speed, and whether a cloned `target/` reuses dependency artifacts in a task worktree. Linux reflink (XFS, Btrfs) has no CI coverage. Unit tests on Ubuntu take the fallback branch, and the `test-cow-macos` job runs the clone path with a failure when the probe finds no support.

## Operating

Set `ORCH_COW_WORKTREES=0` to disable the clone path. To reset a template, delete `~/.orch/state/templates/<project>` and `<project>.deps.*`. To retry a failed install without a lockfile change, delete the matching `<project>.deps.<lockfile>`. The next task recreates it.
