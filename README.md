# bayplan

`bp` runs an isolated copy of part of a Docker Compose stack for each git worktree, next to the main stack.

A **bay** is one such copy: a git worktree, the Compose services picked for it, renamed so they do not
collide with the main stack, and its own share of the stateful services (database, message broker, cache).
The main stack keeps running untouched on the main tree; each bay gets its own hosts.

```
main tree    dashboard.app.localhost      ──> dashboard-app ──────┐
                                                                  ├──> mysql, redis, rabbitmq, auth
bay `demo`   dashboard-demo.app.localhost ──> dashboard-app-demo ─┘    (main stack, shared)
                                                  │
                                                  └──> web-demo ──> api-demo    (bay copies)
```

## Principles

bayplan aims to infer what it can from the Compose configuration, so that it works without blocking in most
cases, while a precise description of the project remains the reliable way to handle the unusual ones.
These principles guide its design; the first and the last apply today, the others are where it is heading.

1. **Explicit wins.** What the descriptor states always takes precedence over what bayplan infers.
2. **Infer rather than block.** *Planned*: without a description, bayplan would infer from the resolved
   Compose configuration (names, aliases, labels, images, lockfiles) whatever it needs to get a bay
   running. Today the descriptor is required.
3. **Every inference is visible.** *Planned*: each decision shows where it comes from, inferred or
   declared; nothing happens silently.
4. **Nudge towards explicit.** *Planned*: inferences come with the descriptor lines that would make them
   explicit, to review and keep.
5. **Warn on ambiguity, never gamble.** What bayplan cannot carry over to a bay raises a warning, and
   nothing is removed on a guess: data it did not create is left alone.

## How it works

`bp up <bay>`:

1. creates the worktree `.bayplan/worktrees/<bay>` (detached, from `from`, `--from` or `HEAD`) if missing;
2. links and copies the untracked files the stack needs (env files, secrets, keys) from the main tree;
3. clones the dependency directories (`node_modules`, `vendor`) from the main tree, copy-on-write when the
   filesystem allows it;
4. resolves the Compose configuration of the worktree (`docker compose config --format json`);
5. provisions the bay's share of the shared resources (database clone, vhost, cache index);
6. writes a Compose file for the bay, keeping only its services, with:
   - names, `container_name`, `hostname` and `depends_on` suffixed with `-<bay>`;
   - published ports, profiles and network aliases removed;
   - environment values pointing at a service or host of the bay rewritten to the bay's copy;
   - `network_mode`, `ipc`, `pid`, `volumes_from` and `links` pointed at the bay's copy of a service, or
     at the main stack's container of a service the bay does not duplicate;
   - the networks of the main stack joined as external ones;
   - router labels (Traefik) renamed and rerouted to the bay's hosts; a router whose rule names no host
     it can rewrite is not exposed, with a warning;
   - bind-mounted files listed in `rewrite_files` replaced by a rewritten copy;
7. installs dependencies whose lockfile differs from the main tree;
8. registers the bay with shared services that need it (e.g. OAuth redirect URIs), recreating them;
9. starts the bay as its own Compose project, `<repo>-bay-<bay>`.

Services the bay does not duplicate are reached on the main stack: a bay with only a front-end app talks to
the main API, a bay that also includes the API talks to its own.

`bp up` is idempotent: run it again after changing the bay config to add or remove services. When a run
fails, it undoes what it created: containers, worktree and the shares it provisioned, and only those.

## Requirements

- Docker with Compose v2, and the main stack running: bays join its networks, whether named explicitly or
  created by Compose (`<project>_default`), and use its shared services.
- git.
- macOS (APFS clones) or Linux (`cp --reflink=auto`).
- For now, Traefik as the router, MySQL/MariaDB, RabbitMQ and Redis as resources.

## Install

On macOS or Linux, no Rust toolchain needed:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/MaelBriantin/bayplan/releases/latest/download/bayplan-installer.sh | sh
```

`bp` installs in `~/.local/bin`. `bp update` installs the latest release; `bp uninstall` removes `bp`,
leaving the bays as they are.

From source, with a Rust toolchain: `cargo install --path .`

## Usage

```sh
bp up <bay> [--from <ref>] [--build]   # create what is missing, then start
bp down <bay>                          # stop, keeping the worktree and resources
bp rm <bay> [--force]                  # remove containers, resources, registrations and worktree
bp ls                                  # list the bays, their state, HEAD and hosts
bp update                              # install the latest release
bp uninstall                           # remove bp, leaving the bays as they are
```

Run it from anywhere in the repository. `rm` asks before removing a worktree with uncommitted changes or
unreferenced commits, unless `--force`; it keeps the bay config.

## Files

```
bayplan.toml            # descriptor: how to duplicate this repository's stack
.bayplan/
├── bays/
│   ├── <bay>.toml      # bay config: what the bay runs
│   └── <bay>/          # generated: compose.json, rewritten files, state.json
└── worktrees/<bay>/    # the bay's worktree
```

`bayplan.toml` describes the repository: commit it to share it with the team, or exclude it for a personal
setup. `.bayplan/` is local to each machine: keep it out of git (`.gitignore`, or `.git/info/exclude`).

### Bay config — `.bayplan/bays/<bay>.toml`

```toml
groups = ["back", "dashboard"]   # groups of the descriptor to duplicate
from = "main"                     # git ref the worktree starts from (default: HEAD)

# Options of the bay's share of a resource, read by that resource only
[resources.mysql]
clone = true                      # own copy of the database (default: shared with the main stack)
```

### Descriptor — `bayplan.toml`

Written once per repository. Every key but `router` and `groups` is optional.

```toml
# Untracked files Compose reads on the host: symlinked, so main tree changes apply to every bay
link = [".env.local", "compose.override.yaml", "secrets/signing_key.pem"]

# Untracked files or directories read inside containers: copied, as a host symlink does not resolve there
copy = ["api/.env.local", "api/keys"]

# Bind-mounted files whose content names services or hosts, e.g. `server api:9000`
rewrite_files = ["docker/nginx/nginx.conf"]

[router]
kind = "traefik"
domain = "app.localhost"

# Named sets of Compose services a bay picks from
[groups]
back = ["api", "web"]
dashboard = ["dashboard-app"]

# Dependency directories cloned from the main tree; `install` runs in the first listed service the bay
# duplicates when the worktree's lockfile differs
[[deps]]
path = "web/node_modules"
lockfile = "web/pnpm-lock.yaml"
install = { services = ["dashboard-app"], command = ["pnpm", "install"] }

# Own database, only for bays setting `clone = true` in `[resources.mysql]`
[resources.mysql]
service = "mysql"
root_password_env = "MARIADB_ROOT_PASSWORD"   # variable of the database container
database = "app"                              # database to clone
user = "app"                                  # user granted access to the clone
database_vars = ["DATABASE_NAME"]             # variables naming the database
# client = "mariadb", dump = "mariadb-dump"   # defaults

# Own vhost `bay-<bay>`
[resources.rabbitmq]
service = "rabbitmq"
user = "app"
dsn_vars = ["AMQP_DSN"]

# Own database index, picked in the range and remapped from the one each service uses
[resources.redis]
service = "redis"
indexes = [3, 15]
index_vars = ["REDIS_DATABASE"]
dsn_vars = ["REDIS_DSN"]

# Lines a shared service of the main stack must load for the bay to work; skipped when the bay runs its own
[[registrations]]
service = "auth"
env_file = ".bayplan/auth.env"   # the service must load it, e.g. through a compose override
match_env = "AUTH_URL"           # bay services whose variable holds a URL get registered
lines = [
    "REDIRECT_URI_BAY_{BAY}_{SERVICE}=https://{host}/auth/callback",
]
```

## Hosts

With the Traefik router and `domain = "app.localhost"`:

| Main stack | Bay `demo` |
|---|---|
| `dashboard.app.localhost` | `dashboard-demo.app.localhost` |
| `app.localhost` | `bay-demo.app.localhost` |

A wildcard certificate on `*.<domain>` covers them. Bay containers resolve bay hosts through the host
gateway, so server-side calls to a public URL reach the bay too.

## Safety

- `bp rm` asks before removing a worktree with uncommitted changes or commits that no branch, tag or
  remote branch contains.
- A resource that cannot be removed keeps its state: `bp rm` stops, and can be run again once the cause is
  fixed. Its share is not handed to another bay in the meantime.
- Redis indexes used by the main stack are never given to a bay, whose indexes are flushed on removal.
- A database clone is recorded only once fully copied; a failed dump is reported, not taken for an empty
  database. An existing database of the clone's name is never overwritten.
- A registration file that bayplan did not write is never overwritten.
- Commands that change bays take a lock, `.bayplan/lock`, so that two of them never take the same share.

## Limits

- Rewriting is based on names: a value is rewritten where a service name or host of the bay stands as a
  host (`http://api`, `api:9000`, `@api`). A value that is only a service name (`api`) is rewritten only
  for a variable named like an address (`*HOST*`, `*URL*`, `*DSN*`, `*SERVER*`…).
- Traefik rules are understood through `Host(...)` and `HostSNI(...)`; a router matched otherwise
  (`HostRegexp`, a path alone) is not exposed by the bay.
- Services sharing an image tag with the main stack (`image:` with `build:`) overwrite that tag when rebuilt
  in a bay.
- Only one resource of each kind (one database server, one broker, one cache) is handled.
- `copy` keeps an existing copy: delete it from the worktree to refresh it.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at
your option. Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion
in bayplan by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.
