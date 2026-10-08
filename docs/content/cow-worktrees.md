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
2. Under a process-wide lock, move the template to `origin/<default>` (`git checkout --detach --force`). Create it with `git worktree add --detach` if missing or broken.
3. If a lockfile changed since the last install, run the install command in the template. The lockfile hash is stored in `<template>.deps`.
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
| `.venv` and `venv` are skipped | They hold absolute paths in scripts and shebangs |

Supported install commands: `bun install --frozen-lockfile`, `yarn install --frozen-lockfile`, `npm ci`. Other ecosystems get no preinstall.

## Costs

- The first task per project pays for the template checkout and install, up to 10 minutes for the install. Later tasks wait on the lock while a refresh or install runs.
- The template is one more full checkout on disk, and it appears in `git worktree list`.
- Writes to a cloned file copy its blocks. A full reinstall or rebuild in a task stops sharing.
- `git update-index --refresh` re-hashes the cloned files, because the clones have new inodes.
- Projects with submodules always use the fallback.
- Cloned `target/` and `node_modules/` can hold absolute paths from the template. Tools that embed them rebuild.
- ext4, HFS+ and XFS without reflink always use the fallback, after one failed probe per process.

## Operating

Set `ORCH_COW_WORKTREES=0` to disable the clone path. To reset a template, delete `~/.orch/state/templates/<project>` and `<project>.deps`. The next task recreates it.
