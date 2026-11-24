//! `bp init`: the descriptor inferred from the Compose configuration of the main stack and git, each line
//! saying where it comes from, to review before keeping it.

mod render;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use regex::Regex;
use serde_json::Value;

use crate::compose::runner;
use crate::config::Layout;
use crate::{exec, router};

/// A value of the descriptor and why it is inferred.
#[derive(Debug, PartialEq, Eq)]
pub struct Why<T> {
    pub value: T,
    pub why: String,
}

fn why<T>(value: T, why: impl Into<String>) -> Why<T> {
    Why {
        value,
        why: why.into(),
    }
}

/// A group of services: its name and services, and why.
pub type Group = Why<(String, Vec<String>)>;

#[derive(Debug, PartialEq, Eq)]
pub struct DepGuess {
    pub path: String,
    pub lockfile: String,
    pub services: Vec<String>,
    pub command: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct MysqlGuess {
    pub service: String,
    pub root_password_env: String,
    pub database: String,
    pub user: String,
    pub database_vars: Vec<String>,
    /// Client and dump commands, `None` for MariaDB's, the defaults.
    pub tools: Option<(String, String)>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct RabbitmqGuess {
    pub service: String,
    pub user: String,
    pub dsn_vars: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct RedisGuess {
    pub service: String,
    pub indexes: [u8; 2],
    pub index_vars: Vec<String>,
    pub dsn_vars: Vec<String>,
}

/// What `bp init` infers, and what it sees but cannot handle.
#[derive(Debug, Default)]
pub struct Inference {
    /// Domain of the Traefik hosts.
    pub router: Option<Why<String>>,
    pub link: Vec<Why<String>>,
    pub copy: Vec<Why<String>>,
    pub rewrite_files: Vec<Why<String>>,
    pub groups: Vec<Group>,
    pub deps: Vec<Why<DepGuess>>,
    pub mysql: Option<Why<MysqlGuess>>,
    pub rabbitmq: Option<Why<RabbitmqGuess>>,
    pub redis: Option<Why<RedisGuess>>,
    /// What the descriptor needs written by hand, or what bays cannot isolate.
    pub notes: Vec<String>,
}

/// The repository as `bp init` reads it.
pub struct Repo {
    pub root: PathBuf,
    /// Resolved Compose configuration of the main stack.
    pub config: Value,
    /// Files known to git, relative to the root.
    pub tracked: BTreeSet<String>,
    /// Files and directories git does not know, ignored or not, relative to the root; a directory with
    /// nothing tracked inside is listed alone.
    pub untracked: Vec<String>,
    /// Services of the main stack whose container ran and ended without error, as one-off tasks: they are
    /// not offered to the bays.
    pub ended: BTreeSet<String>,
}

impl Repo {
    fn read(root: &Path) -> Result<Self> {
        let list = |args: &[&str]| -> Result<Vec<String>> {
            Ok(exec::output(root, "git", args)?
                .split('\0')
                .filter(|path| !path.is_empty())
                .map(|path| path.trim_end_matches('/').to_owned())
                .collect())
        };
        let config = runner::main_config(root)?;
        let containers = config["name"]
            .as_str()
            .map_or_else(Vec::new, crate::commands::containers);
        let ended = containers
            .into_iter()
            .filter(|container| container.state == "exited" && container.exit_code == 0)
            .map(|container| container.service)
            .collect();
        Ok(Self {
            root: root.to_path_buf(),
            config,
            ended,
            tracked: list(&["ls-files", "-z"])?.into_iter().collect(),
            untracked: list(&["ls-files", "-z", "--others", "--directory"])?,
        })
    }

    fn services(&self) -> Vec<(&str, &Value)> {
        self.config["services"]
            .as_object()
            .map_or_default(|services| {
                services
                    .iter()
                    .map(|(name, s)| (name.as_str(), s))
                    .collect()
            })
    }

    /// A path of the configuration relative to the root, `None` outside it.
    fn relative(&self, path: &str) -> Option<String> {
        let relative = Path::new(path).strip_prefix(&self.root).ok()?;
        Some(relative.to_string_lossy().into_owned())
    }

    /// Whether git knows the file, or a file inside the directory.
    fn is_tracked(&self, path: &str) -> bool {
        let inside = format!("{path}/");
        self.tracked.contains(path)
            || self
                .tracked
                .range(inside.clone()..)
                .next()
                .is_some_and(|file| file.starts_with(&inside))
    }
}

/// What a service is, from its image, name or build context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Router,
    Mysql,
    Rabbitmq,
    Redis,
    /// Stateful, with no resource of bayplan for it yet.
    Unsupported(&'static str),
}

fn kind(name: &str, service: &Value) -> Option<Kind> {
    let text = kind_text(name, service);
    let has = |word: &str| text.contains(word);
    if has("traefik") {
        Some(Kind::Router)
    } else if has("mariadb") || has("mysql") {
        Some(Kind::Mysql)
    } else if has("rabbitmq") {
        Some(Kind::Rabbitmq)
    } else if has("redis") {
        Some(Kind::Redis)
    } else if has("postgres") {
        Some(Kind::Unsupported("PostgreSQL"))
    } else if has("mongo") {
        Some(Kind::Unsupported("MongoDB"))
    } else {
        None
    }
}

/// Whether the service runs code of the repository, the code a bay changes: built by the stack, from its
/// own build or an image another service builds, and either mounting a directory of the repository or
/// exposed. A service only building an image, a third-party image, or a shared one, is not.
fn is_app(repo: &Repo, name: &str, service: &Value) -> bool {
    if kind(name, service).is_some() {
        return false;
    }
    let image = service["image"].as_str();
    let built = !service["build"].is_null()
        || repo.services().iter().any(|(_, other)| {
            !other["build"].is_null() && image.is_some() && other["image"].as_str() == image
        });
    let mounts_code = binds(repo, service)
        .iter()
        .any(|source| repo.root.join(source).is_dir());
    built && (mounts_code || !router::traefik_hosts(service).is_empty())
}

/// Build context of the service, relative to the root: its own, or the one of the service building its image.
fn context(repo: &Repo, service: &Value) -> Option<String> {
    let own = service["build"]["context"].as_str();
    let builder = || {
        let image = service["image"].as_str()?;
        repo.services()
            .into_iter()
            .find(|(_, other)| !other["build"].is_null() && other["image"].as_str() == Some(image))
            .and_then(|(_, other)| other["build"]["context"].as_str())
    };
    repo.relative(own.or_else(builder)?)
}

fn env(service: &Value) -> BTreeMap<String, String> {
    service["environment"].as_object().map_or_default(|vars| {
        vars.iter()
            .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
            .collect()
    })
}

fn depends_on(service: &Value) -> Vec<String> {
    service["depends_on"]
        .as_object()
        .map_or_default(|deps| deps.keys().cloned().collect())
}

/// Sources of the bind mounts of a service, relative to the root.
fn binds(repo: &Repo, service: &Value) -> Vec<String> {
    service["volumes"].as_array().map_or_default(|volumes| {
        volumes
            .iter()
            .filter(|volume| volume["type"] == "bind")
            .filter_map(|volume| repo.relative(volume["source"].as_str()?))
            .collect()
    })
}

pub fn infer(repo: &Repo) -> Inference {
    let (groups, left_out) = infer_groups(repo);
    let mut inference = Inference {
        router: infer_router(repo),
        groups,
        deps: infer_deps(repo),
        rewrite_files: infer_rewrite_files(repo),
        ..Inference::default()
    };
    inference.link = infer_link(repo);
    let linked: BTreeSet<&str> = inference.link.iter().map(|l| l.value.as_str()).collect();
    let deps: BTreeSet<&str> = inference
        .deps
        .iter()
        .map(|d| d.value.path.as_str())
        .collect();
    inference.copy = infer_copy(repo, &linked, &deps);
    infer_resources(repo, &mut inference);
    if !left_out.is_empty() {
        inference.notes.push(format!(
            "{} not offered in a group: they ran on the main stack and ended, as one-off tasks",
            left_out.join(", ")
        ));
    }
    if inference.router.is_none() {
        inference.notes.push(
            "no Traefik router found: bayplan only knows Traefik for now; write [router] by hand"
                .to_owned(),
        );
    }
    inference.notes.push(
        "registrations, for shared services that must be told about each bay (OAuth redirect URIs…), are never inferred"
            .to_owned(),
    );
    inference
}

fn infer_router(repo: &Repo) -> Option<Why<String>> {
    let hosts: Vec<String> = repo
        .services()
        .into_iter()
        .flat_map(|(_, service)| router::traefik_hosts(service))
        .collect();
    let domain = common_domain(&hosts)?;
    let services = repo
        .services()
        .into_iter()
        .filter(|(_, service)| !router::traefik_hosts(service).is_empty())
        .count();
    Some(why(
        domain.clone(),
        format!("{services} services carry Traefik rules; their hosts end with {domain}"),
    ))
}

/// The longest domain, of two labels at least, all the hosts end with.
fn common_domain(hosts: &[String]) -> Option<String> {
    let mut labels: Vec<Vec<&str>> = hosts
        .iter()
        .map(|host| host.split('.').rev().collect())
        .collect();
    let first = labels.pop()?;
    let common = first
        .iter()
        .enumerate()
        .take_while(|(index, label)| labels.iter().all(|other| other.get(*index) == Some(label)))
        .map(|(_, label)| *label)
        .collect::<Vec<_>>();
    (common.len() >= 2).then(|| common.into_iter().rev().collect::<Vec<_>>().join("."))
}

/// A group per service of the repository's code the router exposes, with such services it depends on; then
/// a group per image several other ones run, as consumers; then a group per other one. Services the main
/// stack does not run, as one-off tasks, are left out, and returned.
fn infer_groups(repo: &Repo) -> (Vec<Group>, Vec<String>) {
    let services = repo.services();
    let built: BTreeMap<&str, &Value> = services
        .iter()
        .filter(|(name, service)| is_app(repo, name, service))
        .map(|(name, service)| (*name, *service))
        .collect();
    let exposed = |service: &Value| router::traefik_hosts(service).first().cloned();
    let mut grouped = BTreeSet::new();
    let mut groups = Vec::new();
    for (name, service) in &built {
        let Some(host) = exposed(service) else {
            continue;
        };
        let with: Vec<String> = depends_on(service)
            .into_iter()
            .filter(|dep| {
                built
                    .get(dep.as_str())
                    .is_some_and(|s| exposed(s).is_none())
            })
            .collect();
        grouped.insert((*name).to_owned());
        grouped.extend(with.iter().cloned());
        let reason = if with.is_empty() {
            format!("exposed at {host}")
        } else {
            format!("exposed at {host}, with what it depends on")
        };
        let members = std::iter::once((*name).to_owned()).chain(with).collect();
        groups.push(why(((*name).to_owned(), members), reason));
    }

    let (others, left_out): (Vec<&str>, Vec<&str>) = built
        .keys()
        .copied()
        .filter(|name| !grouped.contains(*name))
        .partition(|name| !repo.ended.contains(*name));
    let mut by_image: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for name in &others {
        if let Some(image) = built[name]["image"].as_str() {
            by_image.entry(image).or_default().push(name);
        }
    }
    let project = repo.config["name"].as_str().unwrap_or_default();
    for name in &others {
        let image = built[name]["image"].as_str();
        match image.and_then(|image| by_image.get(image).map(|members| (image, members))) {
            Some((image, members)) if members.len() > 1 => {
                if members[0] == *name {
                    let group = image_group(image, project);
                    groups.push(why(
                        (group, members.iter().map(|m| (*m).to_owned()).collect()),
                        format!("not exposed, all running the image {image}"),
                    ));
                }
            }
            _ => groups.push(why(
                ((*name).to_owned(), vec![(*name).to_owned()]),
                "runs code of the repository, not exposed",
            )),
        }
    }
    (groups, left_out.into_iter().map(str::to_owned).collect())
}

/// Name of a group of the services running an image: the image's name without its registry, tag and the
/// project's prefix, in the plural, as `consumers` for `registry/shop-consumer:1`.
fn image_group(image: &str, project: &str) -> String {
    let name = image.rsplit('/').next().unwrap_or(image);
    let name = name.split(':').next().unwrap_or(name);
    let name = name.strip_prefix(&format!("{project}-")).unwrap_or(name);
    if name.ends_with('s') {
        name.to_owned()
    } else {
        format!("{name}s")
    }
}

/// Lockfiles and the dependency directory beside them, with the command installing it.
const LOCKFILES: &[(&str, &str, &[&str])] = &[
    ("pnpm-lock.yaml", "node_modules", &["pnpm", "install"]),
    ("package-lock.json", "node_modules", &["npm", "ci"]),
    ("yarn.lock", "node_modules", &["yarn", "install"]),
    ("composer.lock", "vendor", &["composer", "install"]),
];

fn infer_deps(repo: &Repo) -> Vec<Why<DepGuess>> {
    let services = repo.services();
    let mut deps = Vec::new();
    for lockfile in &repo.tracked {
        let path = Path::new(lockfile);
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
        let Some((_, dir, command)) = LOCKFILES
            .iter()
            .find(|(file, ..)| Some(*file) == name.as_deref())
        else {
            continue;
        };
        let parent = path
            .parent()
            .map_or_default(|p| p.to_string_lossy().into_owned());
        let dep = if parent.is_empty() {
            (*dir).to_owned()
        } else {
            format!("{parent}/{dir}")
        };
        if !repo.root.join(&dep).is_dir() || repo.is_tracked(&dep) {
            continue;
        }
        let mounting: Vec<(&str, &Value)> = services
            .iter()
            .filter(|(name, service)| is_app(repo, name, service))
            .filter(|(_, service)| {
                binds(repo, service)
                    .iter()
                    .any(|source| dep == *source || dep.starts_with(&format!("{source}/")))
            })
            .copied()
            .collect();
        // `install` runs in the first of them a bay duplicates: those built from the lockfile's directory,
        // which have its tools, rather than any service mounting it.
        let related = |a: &str, b: &str| {
            a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
        };
        let built_there: Vec<&str> = mounting
            .iter()
            .filter(|(_, service)| {
                context(repo, service).is_some_and(|context| related(&context, &parent))
            })
            .map(|(name, _)| *name)
            .collect();
        let users: Vec<String> = if built_there.is_empty() {
            mounting
                .iter()
                .map(|(name, _)| (*name).to_owned())
                .collect()
        } else {
            built_there.into_iter().map(str::to_owned).collect()
        };
        if users.is_empty() {
            continue;
        }
        deps.push(why(
            DepGuess {
                path: dep.clone(),
                lockfile: lockfile.clone(),
                services: users,
                command: command.iter().map(|part| (*part).to_owned()).collect(),
            },
            format!("{dep} is not in git, beside {lockfile}, and mounted into the services"),
        ));
    }
    deps
}

/// Untracked files Compose reads on the host: overrides it loads by itself, `.env`, secret files, and
/// files the root compose files name.
fn infer_link(repo: &Repo) -> Vec<Why<String>> {
    let mut link: Vec<Why<String>> = Vec::new();
    let add = |path: String, reason: String, link: &mut Vec<Why<String>>| {
        // bayplan's own files are made for each bay, never linked.
        if !path.starts_with(".bayplan/")
            && repo.root.join(&path).is_file()
            && !repo.is_tracked(&path)
            && !link.iter().any(|l| l.value == path)
        {
            link.push(why(path, reason));
        }
    };
    for name in [
        ".env",
        "compose.override.yaml",
        "compose.override.yml",
        "docker-compose.override.yaml",
        "docker-compose.override.yml",
    ] {
        add(
            name.to_owned(),
            format!("not in git, Compose reads {name} by itself"),
            &mut link,
        );
    }
    if let Some(secrets) = repo.config["secrets"].as_object() {
        for (name, secret) in secrets {
            if let Some(file) = secret["file"].as_str().and_then(|file| repo.relative(file)) {
                add(
                    file,
                    format!("not in git, the file of secret {name}"),
                    &mut link,
                );
            }
        }
    }
    let token = Regex::new(r"[\w./-]+").unwrap();
    for file in compose_files(repo) {
        let Ok(text) = std::fs::read_to_string(repo.root.join(&file)) else {
            continue;
        };
        for found in token.find_iter(&text) {
            let path = found.as_str().trim_start_matches("./");
            if path.contains('.')
                && !path.starts_with('/')
                && !Path::new(path)
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("yaml"))
            {
                add(
                    path.to_owned(),
                    format!("not in git, named in {file}"),
                    &mut link,
                );
            }
        }
    }
    link
}

/// The compose files of the main stack: those of `COMPOSE_FILE`, from the environment or `.env`, or else the
/// ones Compose finds by itself.
fn compose_files(repo: &Repo) -> Vec<String> {
    let from_dot_env = || {
        std::fs::read_to_string(repo.root.join(".env"))
            .ok()?
            .lines()
            .find_map(|line| line.trim().strip_prefix("COMPOSE_FILE=").map(str::to_owned))
    };
    if let Some(files) = std::env::var("COMPOSE_FILE").ok().or_else(from_dot_env) {
        let separator = std::env::var("COMPOSE_PATH_SEPARATOR").unwrap_or_else(|_| ":".to_owned());
        return files
            .trim_matches('"')
            .split(separator.as_str())
            .map(|file| file.trim_start_matches("./").to_owned())
            .collect();
    }
    let main = [
        "compose.yaml",
        "compose.yml",
        "docker-compose.yaml",
        "docker-compose.yml",
    ]
    .into_iter()
    .find(|file| repo.root.join(file).is_file());
    let overrides = [
        "compose.override.yaml",
        "compose.override.yml",
        "docker-compose.override.yaml",
        "docker-compose.override.yml",
    ];
    main.into_iter()
        .chain(
            overrides
                .into_iter()
                .filter(|file| repo.root.join(file).is_file()),
        )
        .map(str::to_owned)
        .collect()
}

/// Untracked secrets and env files under directories mounted into containers, which a worktree lacks.
fn infer_copy(repo: &Repo, linked: &BTreeSet<&str>, deps: &BTreeSet<&str>) -> Vec<Why<String>> {
    let secret =
        Regex::new(r"(^\.env)|(\.(pem|key|crt|p12)$)|^(\.ssh|\.npmrc|\.netrc|auth\.json)$")
            .unwrap();
    let mut mounted: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for (name, service) in repo.services() {
        for source in binds(repo, service) {
            if repo.root.join(&source).is_dir() {
                mounted.entry(source).or_default().push(name);
            }
        }
    }
    repo.untracked
        .iter()
        .filter(|path| !linked.contains(path.as_str()) && !deps.contains(path.as_str()))
        .filter(|path| {
            Path::new(path)
                .file_name()
                .is_some_and(|name| secret.is_match(&name.to_string_lossy()))
        })
        .filter_map(|path| {
            let (source, services) = mounted
                .iter()
                .filter(|(source, _)| path.starts_with(&format!("{source}/")))
                .max_by_key(|(source, _)| source.len())?;
            let into = match services.as_slice() {
                [one] => (*one).to_owned(),
                [first, second] => format!("{first} and {second}"),
                [first, others @ ..] => format!("{first} and {} other services", others.len()),
                [] => String::new(),
            };
            Some(why(
                path.clone(),
                format!("not in git, under {source}, mounted into {into}"),
            ))
        })
        .collect()
}

/// Files mounted alone whose content reaches a service by its name, as `server api:9000`.
fn infer_rewrite_files(repo: &Repo) -> Vec<Why<String>> {
    let services = repo.services();
    let names: Vec<&str> = services.iter().map(|(name, _)| *name).collect();
    let mut files: BTreeMap<String, String> = BTreeMap::new();
    for (mounter, service) in &services {
        for source in binds(repo, service) {
            let path = repo.root.join(&source);
            let small = path
                .metadata()
                .is_ok_and(|meta| meta.is_file() && meta.len() < 1 << 20);
            if !small || files.contains_key(&source) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let reached = names.iter().find(|name| {
                let pattern = format!(r"(^|[^\w.-]){}(:\d+|\s*;)", regex::escape(name));
                Regex::new(&pattern).is_ok_and(|re| re.is_match(&text))
            });
            if let Some(name) = reached {
                files.insert(
                    source,
                    format!("names the service {name}, mounted into {mounter}"),
                );
            }
        }
    }
    files
        .into_iter()
        .map(|(path, reason)| why(path, reason))
        .collect()
}

/// The services of the main stack, with the environments of those built here, which name the shared ones.
struct Shared<'a> {
    services: Vec<(&'a str, &'a Value)>,
    built_envs: Vec<BTreeMap<String, String>>,
}

impl<'a> Shared<'a> {
    fn new(repo: &'a Repo) -> Self {
        let services = repo.services();
        let built_envs = services
            .iter()
            .filter(|(name, service)| is_app(repo, name, service))
            .map(|(_, service)| env(service))
            .collect();
        Self {
            services,
            built_envs,
        }
    }

    /// Variables of the services built here passing `test`, by name.
    fn vars_where(&self, test: &dyn Fn(&str, &str) -> bool) -> Vec<String> {
        self.built_envs
            .iter()
            .flat_map(|env| {
                env.iter()
                    .filter(|(key, value)| test(key, value))
                    .map(|(key, _)| key.clone())
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// The service of this kind; of several, the one the others depend on, as a Redis its sentinel watches.
    fn of_kind(&self, wanted: Kind) -> Option<(&'a str, &'a Value)> {
        let found: Vec<(&str, &Value)> = self
            .services
            .iter()
            .filter(|(name, service)| kind(name, service) == Some(wanted))
            .copied()
            .collect();
        let depended = |name: &str| {
            found
                .iter()
                .any(|(_, other)| depends_on(other).iter().any(|dep| dep == name))
        };
        found
            .iter()
            .find(|(name, _)| depended(name))
            .or_else(|| found.first())
            .copied()
    }
}

fn infer_resources(repo: &Repo, inference: &mut Inference) {
    let shared = Shared::new(repo);
    for (name, service) in &shared.services {
        if let Some(Kind::Unsupported(what)) = kind(name, service) {
            inference.notes.push(format!(
                "{name} looks like {what}: bayplan has no resource for it yet, so the bays share it with the main stack"
            ));
        }
    }
    infer_mysql(&shared, inference);
    infer_rabbitmq(&shared, inference);
    infer_redis(&shared, inference);
}

fn infer_mysql(shared: &Shared, inference: &mut Inference) {
    let Some((name, service)) = shared.of_kind(Kind::Mysql) else {
        return;
    };
    let vars = env(service);
    let first = |keys: &[&str]| {
        keys.iter()
            .find(|key| vars.contains_key(**key))
            .map(|key| (*key).to_owned())
    };
    let root_password_env = first(&["MARIADB_ROOT_PASSWORD", "MYSQL_ROOT_PASSWORD"]);
    let database = first(&["MARIADB_DATABASE", "MYSQL_DATABASE"]).map(|key| vars[&key].clone());
    let user = first(&["MARIADB_USER", "MYSQL_USER"]).map(|key| vars[&key].clone());
    let (Some(root_password_env), Some(database), Some(user)) = (root_password_env, database, user)
    else {
        inference.notes.push(format!(
            "{name} looks like MySQL/MariaDB, but its root password variable, database or user is not in its environment: write [resources.mysql] by hand"
        ));
        return;
    };
    let named = Regex::new(r"(DATABASE|DB)(_NAME)?$").unwrap();
    let database_vars = shared.vars_where(&|key, value| value == database && named.is_match(key));
    let mariadb = kind_text(name, service).contains("mariadb")
        || vars.keys().any(|key| key.starts_with("MARIADB_"));
    let tools = (!mariadb).then(|| ("mysql".to_owned(), "mysqldump".to_owned()));
    inference.mysql = Some(why(
        MysqlGuess {
            service: name.to_owned(),
            root_password_env,
            database: database.clone(),
            user,
            database_vars,
            tools,
        },
        format!(
            "{name} is a MySQL/MariaDB server for database {database}; a bay gets its own copy with `clone = true`"
        ),
    ));
}

fn infer_rabbitmq(shared: &Shared, inference: &mut Inference) {
    let Some((name, service)) = shared.of_kind(Kind::Rabbitmq) else {
        return;
    };
    let user = env(service)
        .get("RABBITMQ_DEFAULT_USER")
        .cloned()
        .unwrap_or_else(|| "guest".to_owned());
    let dsn_vars = shared
        .vars_where(&|_, value| value.starts_with("amqp://") || value.starts_with("amqps://"));
    if dsn_vars.is_empty() {
        inference.notes.push(format!(
            "{name} looks like RabbitMQ, but no service built here holds an amqp:// DSN"
        ));
        return;
    }
    inference.rabbitmq = Some(why(
        RabbitmqGuess {
            service: name.to_owned(),
            user,
            dsn_vars,
        },
        format!("{name} is a RabbitMQ server; a bay gets its own vhost"),
    ));
}

fn infer_redis(shared: &Shared, inference: &mut Inference) {
    let Some((name, _)) = shared.of_kind(Kind::Redis) else {
        return;
    };
    let dsn_vars = shared
        .vars_where(&|_, value| value.starts_with("redis://") || value.starts_with("rediss://"));
    let index_vars = shared.vars_where(&|key, value| {
        key.contains("REDIS")
            && (key.contains("DB") || key.contains("DATABASE"))
            && value.parse::<u8>().is_ok()
    });
    let used = shared
        .built_envs
        .iter()
        .flat_map(|env| env.iter())
        .filter_map(|(key, value)| {
            if index_vars.contains(key) {
                value.parse::<u8>().ok()
            } else if dsn_vars.contains(key) {
                let path = value.split('?').next()?.rsplit('/').next()?;
                path.parse::<u8>().ok()
            } else {
                None
            }
        })
        .max();
    let first = used.map_or(1, |used| used + 1);
    if dsn_vars.is_empty() && index_vars.is_empty() {
        inference.notes.push(format!(
            "{name} looks like Redis, but no service built here holds its DSN or database index"
        ));
    } else if first > 15 {
        inference.notes.push(format!(
            "{name} looks like Redis, but the main stack uses its last database index: no room for the bays"
        ));
    } else {
        inference.redis = Some(why(
            RedisGuess {
                service: name.to_owned(),
                indexes: [first, 15],
                index_vars,
                dsn_vars,
            },
            format!(
                "{name} is a Redis server; a bay gets its own database indexes, above the ones in use"
            ),
        ));
    }
}

/// What tells a service's kind: its image, name and build context.
fn kind_text(name: &str, service: &Value) -> String {
    format!(
        "{} {name} {}",
        service["image"].as_str().unwrap_or_default(),
        service["build"]["context"].as_str().unwrap_or_default()
    )
    .to_lowercase()
}

/// `bp init`: prints the inferred descriptor, or writes it with `write` unless one exists.
pub fn init(write: bool) -> Result<()> {
    let layout = Layout::discover()?;
    let repo = Repo::read(&layout.root)?;
    let text = render::render(&infer(&repo));
    let path = layout.descriptor();
    if write {
        if path.exists() {
            bail!(
                "{} exists: compare it with `bp init`, and change it by hand",
                path.display()
            );
        }
        std::fs::write(&path, text)?;
        println!(
            "Wrote {}: review it, each line says where it comes from",
            path.display()
        );
        return Ok(());
    }
    if path.exists() {
        eprintln!(
            "{} exists, and is what bayplan uses; this is what it would infer:\n",
            path.display()
        );
    }
    print!("{text}");
    Ok(())
}

#[cfg(test)]
mod tests;
