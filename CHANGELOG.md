# Changelog

What each version brought, for the people running `bp`. How it works, and its limits, live in the [README](README.md).

Each version's lines go under New (what didn't exist), Changed (what works otherwise) and Fixed (bugs), in that order, only those that have lines.

Numbers: a minor version (0.2.0) for something new to use, a command, an option, a router or a resource; a patch (0.2.1) for a fix alone.

## Unreleased
Written as the work goes, under the same New, Changed and Fixed; it becomes the next version's section when it is released.

### New
- `bp watch`, a screen following the main stack and the bays as they run: each one's state, branch, uncommitted changes and commits ahead of or behind its upstream; for the selected one, its containers with their healthcheck, its URLs, opened in the browser by their number (`1`–`9`), the pull request of its branch opened with `p` (with `gh`), what it has of each resource, its own share or the main stack's, and the logs of the selected container, read again every 2 seconds. `c` measures the CPU and memory of the main stack, of each bay and of all of them. `i` shows what the selected bay is made of: its groups and their services, `from`, `branch` and worktree. Started inside a bay's worktree, it selects that bay. `?` lists the keys.
- From `bp watch`: `u`, `U`, `d` and `x` run `bp up`, `bp up --build`, `bp down` and `bp rm` on the selected bay, `rm` once confirmed, and `u` and `U` run `docker compose up -d` and `up -d --build` on the main stack; `r` restarts the selected container; the command's output shows in a window over the screen. `e` opens the selected worktree in `$EDITOR`: with the command of `$BAYPLAN_EDIT`, in a new tab of any terminal, or in a new Rex or tmux window; `y` copies its path. When Docker is not reachable, the screen says so.
- The logs in `bp watch` go on the whole screen with `z` and scroll back with `PgUp`/`PgDn` or `Ctrl-u`/`Ctrl-d`.

## v0.2.1 · November 24, 2026

### Fixed
- `bp update` installed the new version in `~/.local` instead of `~/.local/bin`, next to the old one, which stayed first in the PATH. It now updates `bp` where it is, and repairs an install the earlier update moved: run the installer again once, then remove `~/.local/bp`.

## v0.2.0 · November 24, 2026

### New
- `bp up <bay> --branch <name>` (`-b`), or `branch` in the bay config, checks the worktree out on a branch instead of a detached HEAD, created from the start ref when missing, so that the work done in a bay can be committed and pushed. A branch held by another worktree, the main tree included, is refused.
- `bp ls` shows the branch next to HEAD: `branch@sha`.

### Changed
- `bp rm` keeps the worktree's branch, and says so; a failed `bp up` deletes a branch only when it created it.

## v0.1.0 · November 20, 2026
A copy of part of a Compose stack for each git worktree, next to the main stack.

### New
- `bp up`, `bp down`, `bp rm` and `bp ls`: a bay, its git worktree and the Compose services picked for it, renamed, rerouted and started as their own project, joining the main stack's networks.
- `bayplan.toml`, the descriptor of a repository: groups of services, untracked files to link or copy, dependency directories, files to rewrite, registrations.
- Untracked files linked or copied from the main tree, dependency directories cloned copy-on-write, and reinstalled when their lockfile differs.
- Traefik as the router: each bay on its own hosts under the domain, `<name>-<bay>.<domain>`.
- MySQL/MariaDB, RabbitMQ and Redis as resources: a database shared or cloned, a vhost, cache indexes, each bay getting its own share.
- A failed `bp up` undoes what it created, and only that; `bp rm` asks before removing uncommitted or unreferenced work.
- An installer for macOS and Linux; `bp update` installs the latest version, `bp uninstall` removes `bp`.
- An agent skill, `skills/bayplan/SKILL.md`, to teach an AI agent to use `bp`.
