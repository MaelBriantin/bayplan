use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;

/// What bayplan needs to know about a repository, read from `bayplan.toml` at the repository root.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Descriptor {
    pub router: RouterConfig,
    /// Untracked files of the main tree that Compose reads on the host (env files, compose override, secrets),
    /// symlinked into each worktree so that changes in the main tree apply to every bay.
    #[serde(default)]
    pub link: Vec<PathBuf>,
    /// Untracked files or directories of the main tree read inside containers through a bind mount, copied
    /// into each worktree: a symlink to a host path does not resolve inside a container.
    #[serde(default)]
    pub copy: Vec<PathBuf>,
    /// Bind-mounted files whose content refers to service names or hosts, relative to the repository root.
    #[serde(default)]
    pub rewrite_files: Vec<PathBuf>,
    /// Named sets of Compose services a bay picks from.
    pub groups: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub deps: Vec<Dep>,
    #[serde(default)]
    pub resources: Resources,
    #[serde(default)]
    pub registrations: Vec<Registration>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterConfig {
    pub kind: RouterKind,
    /// Domain the hosts live under, e.g. `app.localhost`.
    pub domain: String,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum RouterKind {
    Traefik,
}

/// Untracked dependency directory cloned from the main tree into each worktree.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dep {
    pub path: PathBuf,
    /// Lockfile whose change in the worktree triggers `install`.
    pub lockfile: PathBuf,
    pub install: Option<Install>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Install {
    /// Services able to run the command; the first one the bay duplicates is used.
    pub services: Vec<String>,
    pub command: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    pub mysql: Option<MysqlConfig>,
    pub rabbitmq: Option<RabbitmqConfig>,
    pub redis: Option<RedisConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MysqlConfig {
    pub service: String,
    #[serde(default = "default_mysql_client")]
    pub client: String,
    #[serde(default = "default_mysql_dump")]
    pub dump: String,
    /// Variable of the database container holding the root password.
    pub root_password_env: String,
    /// Database cloned when a bay asks for its own.
    pub database: String,
    /// User granted access to the cloned database.
    pub user: String,
    /// Variables of the bay services that name the database.
    pub database_vars: Vec<String>,
}

fn default_mysql_client() -> String {
    "mariadb".to_owned()
}

fn default_mysql_dump() -> String {
    "mariadb-dump".to_owned()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RabbitmqConfig {
    pub service: String,
    /// User granted access to the bay vhost.
    pub user: String,
    /// Variables of the bay services holding an AMQP DSN.
    pub dsn_vars: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedisConfig {
    pub service: String,
    /// Range of database indexes bays may use, bounds included.
    pub indexes: [u8; 2],
    /// Variables of the bay services holding a database index.
    #[serde(default)]
    pub index_vars: Vec<String>,
    /// Variables of the bay services holding a DSN ending with `/<index>`.
    #[serde(default)]
    pub dsn_vars: Vec<String>,
}

/// Lines a shared service of the main stack must load for the bay to work, e.g. OAuth redirect URIs.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registration {
    /// Main stack service, recreated when its lines change; skipped when the bay runs its own.
    pub service: String,
    /// File the service loads, relative to the repository root.
    pub env_file: PathBuf,
    /// Variable whose URL gives the host of each bay service to register.
    pub match_env: String,
    /// Templates with `{BAY}`, `{SERVICE}` and `{host}`.
    pub lines: Vec<String>,
}
