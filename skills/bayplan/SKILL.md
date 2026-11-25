---
name: bayplan
description: Runs an isolated copy of part of a Docker Compose stack for a git worktree with the `bp` CLI (bayplan), next to the main stack. Use when the repository has a `bayplan.toml` at its root and the user wants to run, test or preview a branch or worktree in parallel of the main stack, or mentions a bay, `bp up`, `bp ls`, `bp init` or `.bayplan/`.
---

# bayplan

`bp` gives a git worktree its own running copy of selected Compose services, called a **bay**, while the
main stack keeps serving the main tree. A bay has its own containers, hosts and share of the stateful
services; everything it does not duplicate is reached on the main stack.

## Files

- `bayplan.toml` at the repository root: the descriptor. It lists the **groups** of services a bay can
  pick from, and how to duplicate the stack. Read it, do not change it without the user's agreement.
  It is optional: what it leaves out is inferred, and `bp up` prints each inferred line and each warning
  about shared resources; pass them on to the user. `bp init` prints the whole inferred descriptor, with
  its group names: the user decides whether to write it (`bp init --write`) and what to fix.
- `.bayplan/bays/<bay>.toml`: what a bay runs. Writing one is how a bay is created.
- `.bayplan/worktrees/<bay>/`: the bay's worktree. Code for a bay is edited **there**, never in the main
  tree.
- `.bayplan/bays/<bay>/`: generated files (`compose.json`, `state.json`). Never edit them.

## Creating and running a bay

1. Read the groups in `bayplan.toml` and pick the smallest set the task needs: a front-end change needs
   only its app; add the back-end groups only when the change touches them.
2. Write `.bayplan/bays/<bay>.toml`, with a lowercase name (letters, digits, dashes):

   ```toml
   groups = ["dashboard"]    # groups of the descriptor
   from = "main"             # git ref the worktree starts from; defaults to HEAD
   branch = "feat/demo"      # optional: branch to check out, created if missing
   ```

   Set `branch` when the work done in the bay will be committed and pushed: without it, the worktree is
   on a detached HEAD. An existing branch cannot be checked out while another worktree holds it.

   Add `[resources.mysql]` with `clone = true` only when the user asks for an own database: it copies
   the whole main database, which takes time and disk.
3. Run `bp up <bay>` from anywhere in the repository. The first run clones dependencies and may build
   images: let it finish. It prints the bay's URLs.

Run `bp up <bay>` again after changing the bay config: it adds or removes services and keeps the rest.
Pass `--build` after a Dockerfile change.

## Commands

| Command | Effect |
|---|---|
| `bp up <bay> [--from <ref>] [--branch <name>] [--build]` | creates what is missing, then starts the bay |
| `bp ls --json` | lists bays with their state (`up n/m`, `down`), branch and HEAD, uncommitted changes, URLs, containers, resources and groups |
| `bp down <bay>` | stops the bay, keeping its worktree and resources |
| `bp rm <bay> [--force]` | removes containers, resources and **the worktree**, keeping its branch |

Never run `bp rm`, and never pass `--force`, unless the user explicitly asks: it deletes the worktree,
with any uncommitted work in it.

`bp watch` is an interactive screen for the user: do not run it, use `bp ls --json` to read the bays' state.

## Hosts

A service exposed as `<name>.<domain>` on the main stack is exposed as `<name>-<bay>.<domain>` in the
bay; the bare domain becomes `bay-<bay>.<domain>`. `bp ls` prints them.

## Troubleshooting

- Containers of a bay belong to the Compose project `<repo>-bay-<bay>`:
  `docker compose -p <repo>-bay-<bay> ps` or `logs <service>-<bay>`.
- A service fails on a missing file that is untracked in git (keys, local env files): the worktree does
  not have it. Propose adding it to `copy` in `bayplan.toml` (read inside a container) or `link` (read by
  Compose on the host), then remove it from the worktree if a stale copy is there and run `bp up` again.
- A service of the bay reaches the main stack instead of its bay copy: the value naming it is not
  rewritten. Check the variable in `.bayplan/bays/<bay>/compose.json`; a file naming services must be
  listed in `rewrite_files`.
- Dependencies are reinstalled in the bay when its lockfile differs from the main tree's; a failure there
  shows in the `bp up` output.
