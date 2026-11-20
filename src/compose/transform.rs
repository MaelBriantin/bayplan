use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde_json::{Map, Value, json};

use crate::compose::Rewriter;
use crate::router::Router;

pub type Env = BTreeMap<String, String>;

pub struct Input<'a> {
    /// Resolved configuration of the stack, as printed by `docker compose config --format json`.
    pub config: &'a Value,
    /// Services the bay duplicates.
    pub services: &'a BTreeSet<String>,
    pub bay: &'a str,
    /// Compose project of the bay, which the configuration was resolved with.
    pub project: &'a str,
    /// Compose project of the main stack, whose networks and containers the bay reaches.
    pub main_project: &'a str,
    pub router: &'a dyn Router,
    /// Bind-mounted files to rewrite, as absolute paths.
    pub rewrite_files: &'a [PathBuf],
    /// Directory the rewritten files are written to.
    pub files_dir: &'a Path,
    /// Variables forced on a service, computed from its original environment.
    pub env_overrides: &'a dyn Fn(&Env) -> Env,
}

pub struct Output {
    pub compose: Value,
    /// Host of the main stack → host of the bay.
    pub hosts: BTreeMap<String, String>,
    /// Rewritten copies to write: source → destination.
    pub files: Vec<(PathBuf, PathBuf)>,
    pub rewriter: Rewriter,
    /// Environment of each duplicated service once rewritten, by original name.
    pub envs: BTreeMap<String, Env>,
    /// What could not be carried over to the bay.
    pub warnings: Vec<String>,
}

pub fn suffixed(name: &str, bay: &str) -> String {
    format!("{name}-{bay}")
}

pub fn env_of(service: &Value) -> Env {
    service
        .get("environment")
        .and_then(Value::as_object)
        .map_or_default(|env| {
            env.iter()
                .filter_map(|(key, value)| value.as_str().map(|v| (key.clone(), v.to_owned())))
                .collect()
        })
}

/// Turns the configuration of the main stack into the one of a bay: only the chosen services, renamed, with
/// every reference to them pointing at the bay, routed on their own hosts.
pub fn transform(input: &Input) -> Result<Output> {
    let Some(all) = input.config.get("services").and_then(Value::as_object) else {
        bail!("the Compose configuration has no services");
    };
    let missing: Vec<_> = input
        .services
        .iter()
        .filter(|name| !all.contains_key(*name))
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        bail!("unknown Compose services: {}", missing.join(", "));
    }

    let hosts: BTreeMap<String, String> = input
        .services
        .iter()
        .flat_map(|name| input.router.hosts(&all[name]))
        .map(|host| {
            let bay_host = input.router.bay_host(&host, input.bay);
            (host, bay_host)
        })
        .collect();
    let rewriter = Rewriter::new(
        input
            .services
            .iter()
            .map(|name| (name.clone(), suffixed(name, input.bay))),
        hosts.clone(),
    );
    let files: Vec<(PathBuf, PathBuf)> = input
        .rewrite_files
        .iter()
        .enumerate()
        .map(|(index, source)| {
            let name = source.file_name().unwrap_or_default().to_string_lossy();
            (
                source.clone(),
                input.files_dir.join(format!("{index}-{name}")),
            )
        })
        .collect();

    let mut services = Map::new();
    let mut envs = BTreeMap::new();
    let mut named_volumes = BTreeSet::new();
    let mut warnings = Vec::new();
    for name in input.services {
        let mut service = all[name].clone();
        let original_env = env_of(&service);
        let service_warnings =
            transform_service(&mut service, all, input, &rewriter, &hosts, &files);
        warnings.extend(
            service_warnings
                .into_iter()
                .map(|warning| format!("service `{name}`: {warning}")),
        );
        apply_env_overrides(&mut service, (input.env_overrides)(&original_env));
        named_volumes.extend(named_volumes_of(&service));
        envs.insert(name.clone(), env_of(&service));
        services.insert(suffixed(name, input.bay), service);
    }

    let mut compose = Map::new();
    compose.insert("name".to_owned(), json!(input.project));
    compose.insert("services".to_owned(), Value::Object(services));
    if let Some(networks) = input.config.get("networks").and_then(Value::as_object) {
        let joined: Map<_, _> = networks
            .iter()
            .map(|(key, network)| (key.clone(), joined_network(key, network, input)))
            .collect();
        compose.insert("networks".to_owned(), Value::Object(joined));
    }
    for key in ["secrets", "configs"] {
        if let Some(value) = input.config.get(key) {
            compose.insert(key.to_owned(), value.clone());
        }
    }
    if let Some(volumes) = input.config.get("volumes").and_then(Value::as_object) {
        let used: Map<_, _> = volumes
            .iter()
            .filter(|(name, _)| named_volumes.contains(*name))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        if !used.is_empty() {
            compose.insert("volumes".to_owned(), Value::Object(used));
        }
    }

    Ok(Output {
        compose: Value::Object(compose),
        hosts,
        files,
        rewriter,
        envs,
        warnings,
    })
}

/// The network of the main stack a bay network stands for, joined as an external one: the bay reaches the
/// shared services on it, and removing the bay leaves it in place. A network Compose named after the bay
/// project, as it does without an explicit `name`, is the one the main project created.
fn joined_network(key: &str, network: &Value, input: &Input) -> Value {
    if network.get("external").and_then(Value::as_bool) == Some(true) {
        return network.clone();
    }
    let name = network.get("name").and_then(Value::as_str).unwrap_or(key);
    let name = if name == format!("{}_{key}", input.project) {
        format!("{}_{key}", input.main_project)
    } else {
        name.to_owned()
    };
    json!({"name": name, "external": true})
}

/// Container of a service outside the bay, in the main stack.
fn main_container(all: &Map<String, Value>, name: &str, main_project: &str) -> String {
    all.get(name)
        .and_then(|service| service.get("container_name"))
        .and_then(Value::as_str)
        .map_or_else(|| format!("{main_project}-{name}-1"), str::to_owned)
}

fn transform_service(
    service: &mut Value,
    all: &Map<String, Value>,
    input: &Input,
    rewriter: &Rewriter,
    hosts: &BTreeMap<String, String>,
    files: &[(PathBuf, PathBuf)],
) -> Vec<String> {
    let bay = input.bay;
    let Some(object) = service.as_object_mut() else {
        return Vec::new();
    };

    // Published ports would collide with the main stack; profiles would keep the service from starting
    object.remove("ports");
    object.remove("profiles");
    for key in ["container_name", "hostname"] {
        if let Some(Value::String(name)) = object.get_mut(key) {
            *name = suffixed(name, bay);
        }
    }
    if let Some(deploy) = object.get_mut("deploy").and_then(Value::as_object_mut)
        && deploy.get("replicas").and_then(Value::as_u64) == Some(0)
    {
        deploy.remove("replicas");
    }

    point_references(object, all, input);

    // Aliases are shared names on the network: the bay would answer in place of the main stack
    if let Some(networks) = object.get_mut("networks").and_then(Value::as_object_mut) {
        for network in networks.values_mut() {
            if let Some(settings) = network.as_object_mut() {
                settings.remove("aliases");
                if settings.is_empty() {
                    *network = Value::Null;
                }
            }
        }
    }

    if let Some(env) = object.get_mut("environment").and_then(Value::as_object_mut) {
        for (key, value) in env.iter_mut() {
            if let Value::String(text) = value {
                *text = rewriter.apply_env(key, text);
            }
        }
    }

    if let Some(volumes) = object.get_mut("volumes").and_then(Value::as_array_mut) {
        for volume in volumes {
            let Some(source) = volume.get("source").and_then(Value::as_str) else {
                continue;
            };
            if let Some((_, destination)) = files.iter().find(|(s, _)| s == Path::new(source)) {
                volume["source"] = json!(destination);
            }
        }
    }

    // The bay hosts are unknown to Docker DNS: reach them through the reverse proxy published on the host
    let extra_hosts = object
        .entry("extra_hosts")
        .or_insert_with(|| Value::Array(Vec::new()));
    if let Some(extra_hosts) = extra_hosts.as_array_mut() {
        for host in hosts.values() {
            let entry = json!(format!("{host}=host-gateway"));
            if !extra_hosts.contains(&entry) {
                extra_hosts.push(entry);
            }
        }
    }

    input.router.rewrite(service, bay, rewriter)
}

/// Points what names other services (dependencies, shared namespaces and volumes, links) at the bay copy
/// of a duplicated service, or at the main container of one the bay does not duplicate.
fn point_references(object: &mut Map<String, Value>, all: &Map<String, Value>, input: &Input) {
    let bay = input.bay;
    if let Some(depends_on) = object
        .remove("depends_on")
        .and_then(|v| v.as_object().cloned())
    {
        let kept: Map<_, _> = depends_on
            .into_iter()
            .filter(|(name, _)| input.services.contains(name))
            .map(|(name, condition)| (suffixed(&name, bay), condition))
            .collect();
        if !kept.is_empty() {
            object.insert("depends_on".to_owned(), Value::Object(kept));
        }
    }
    // A service of the bay is reached under its new name, one outside it through its main container
    let reference = |name: &str| {
        if input.services.contains(name) {
            suffixed(name, bay)
        } else {
            format!(
                "container:{}",
                main_container(all, name, input.main_project)
            )
        }
    };
    for key in ["network_mode", "ipc", "pid"] {
        if let Some(Value::String(mode)) = object.get_mut(key)
            && let Some(target) = mode.strip_prefix("service:").map(str::to_owned)
        {
            let target = reference(&target);
            *mode = if target.starts_with("container:") {
                target
            } else {
                format!("service:{target}")
            };
        }
    }
    if let Some(list) = object.get_mut("volumes_from").and_then(Value::as_array_mut) {
        for entry in list {
            if let Value::String(text) = entry
                && !text.starts_with("container:")
            {
                *text = match text.split_once(':') {
                    Some((name, mode)) => format!("{}:{mode}", reference(name)),
                    None => reference(text),
                };
            }
        }
    }
    // Links to the main stack are dropped: its services already answer by name on the shared network
    if let Some(links) = object.remove("links").and_then(|v| v.as_array().cloned()) {
        let kept: Vec<Value> = links
            .iter()
            .filter_map(Value::as_str)
            .filter_map(|link| {
                let (name, alias) = link.split_once(':').unwrap_or((link, link));
                input
                    .services
                    .contains(name)
                    .then(|| json!(format!("{}:{alias}", suffixed(name, bay))))
            })
            .collect();
        if !kept.is_empty() {
            object.insert("links".to_owned(), Value::Array(kept));
        }
    }
}

fn apply_env_overrides(service: &mut Value, overrides: Env) {
    if overrides.is_empty() {
        return;
    }
    let Some(object) = service.as_object_mut() else {
        return;
    };
    let env = object
        .entry("environment")
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(env) = env.as_object_mut() {
        for (key, value) in overrides {
            env.insert(key, Value::String(value));
        }
    }
}

fn named_volumes_of(service: &Value) -> Vec<String> {
    service
        .get("volumes")
        .and_then(Value::as_array)
        .map_or_default(|volumes| {
            volumes
                .iter()
                .filter(|v| v.get("type").and_then(Value::as_str) == Some("volume"))
                .filter_map(|v| v.get("source").and_then(Value::as_str).map(str::to_owned))
                .collect()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Descriptor, RouterKind};
    use crate::router;

    fn config() -> Value {
        json!({
            "name": "repo",
            "networks": {
                "default": {"name": "shared", "external": true},
                "back": {"name": "repo-bay-demo_back", "ipam": {}},
                "named": {"name": "named", "driver": "bridge"},
            },
            "secrets": {"jwt": {"file": "/wt/secrets/jwt.pem"}},
            "volumes": {"database": {}, "cache": {}},
            "services": {
                "api": {
                    "build": {"context": "/wt/api"},
                    "environment": {"DATABASE_HOST": "mysql", "DATABASE_NAME": "app"},
                    "networks": {"default": null},
                    "volumes": [{"type": "volume", "source": "cache", "target": "/cache"}],
                    "volumes_from": ["mysql:ro", "web", "container:other"],
                    "links": ["web", "mysql:db"],
                    "pid": "service:mysql",
                },
                "web": {
                    "depends_on": {"api": {"condition": "service_started"}},
                    "labels": {
                        "traefik.enable": "true",
                        "traefik.http.routers.api.rule": "Host(`api.app.localhost`)",
                    },
                    "volumes": [{"type": "bind", "source": "/wt/nginx.conf", "target": "/etc/nginx/nginx.conf"}],
                },
                "dashboard-app": {
                    "depends_on": {
                        "web": {"condition": "service_started"},
                        "mysql": {"condition": "service_started"},
                    },
                    "deploy": {"replicas": 0, "resources": {}},
                    "environment": {
                        "NEXT_API_HOST": "http://web",
                        "NEXT_AUTH_URL_INTERNAL": "http://dashboard-app:3000",
                        "AUTH_URL": "https://dashboard.app.localhost",
                        "NEXT_PUBLIC_API_HOST": "https://api.app.localhost",
                        "AUTH_HOST": "https://auth.app.localhost",
                        "UNSET": null,
                    },
                    "labels": {"traefik.http.routers.dashboard-app.rule": "Host(`dashboard.app.localhost`)"},
                    "networks": {"default": {"aliases": ["dashboard.internal"]}},
                    "ports": [{"target": 3000, "published": "3000"}],
                    "profiles": ["apps"],
                },
                "mysql": {
                    "volumes": [{"type": "volume", "source": "database", "target": "/var/lib/mysql"}],
                    "ports": [{"target": 3306, "published": "3306"}],
                },
            },
        })
    }

    fn descriptor() -> Descriptor {
        toml::from_str(
            r#"
            [router]
            kind = "traefik"
            domain = "app.localhost"
            [groups]
            "#,
        )
        .unwrap()
    }

    fn run(env_overrides: &dyn Fn(&Env) -> Env) -> Output {
        let descriptor = descriptor();
        assert!(matches!(descriptor.router.kind, RouterKind::Traefik));
        let router = router::from_descriptor(&descriptor);
        let config = config();
        let services: BTreeSet<String> = ["api", "web", "dashboard-app"].map(str::to_owned).into();
        let rewrite_files = [PathBuf::from("/wt/nginx.conf")];
        transform(&Input {
            config: &config,
            services: &services,
            bay: "demo",
            project: "repo-bay-demo",
            main_project: "repo",
            router: router.as_ref(),
            rewrite_files: &rewrite_files,
            files_dir: Path::new("/bayplan/bays/demo"),
            env_overrides,
        })
        .unwrap()
    }

    fn no_overrides(_: &Env) -> Env {
        Env::new()
    }

    #[test]
    fn keeps_only_the_bay_services_renamed() {
        let output = run(&no_overrides);
        let names: Vec<_> = output.compose["services"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(names, ["api-demo", "dashboard-app-demo", "web-demo"]);
        assert_eq!(output.compose["name"], "repo-bay-demo");
    }

    #[test]
    fn points_references_at_the_bay_and_leaves_the_rest_on_the_main_stack() {
        let output = run(&no_overrides);
        let env = &output.compose["services"]["dashboard-app-demo"]["environment"];
        assert_eq!(env["NEXT_API_HOST"], "http://web-demo");
        assert_eq!(
            env["NEXT_AUTH_URL_INTERNAL"],
            "http://dashboard-app-demo:3000"
        );
        assert_eq!(env["AUTH_URL"], "https://dashboard-demo.app.localhost");
        assert_eq!(
            env["NEXT_PUBLIC_API_HOST"],
            "https://api-demo.app.localhost"
        );
        assert_eq!(env["AUTH_HOST"], "https://auth.app.localhost");
        assert_eq!(env["UNSET"], Value::Null);
        assert_eq!(
            output.compose["services"]["api-demo"]["environment"]["DATABASE_HOST"],
            "mysql"
        );
    }

    #[test]
    fn removes_what_would_collide_with_the_main_stack() {
        let output = run(&no_overrides);
        let app = &output.compose["services"]["dashboard-app-demo"];
        assert!(app.get("ports").is_none());
        assert!(app.get("profiles").is_none());
        assert_eq!(app["networks"]["default"], Value::Null);
        assert_eq!(app["deploy"], json!({"resources": {}}));
        assert_eq!(
            app["depends_on"],
            json!({"web-demo": {"condition": "service_started"}})
        );
    }

    #[test]
    fn routes_the_bay_on_its_own_hosts() {
        let output = run(&no_overrides);
        let nginx = &output.compose["services"]["web-demo"];
        assert_eq!(
            nginx["labels"]["traefik.http.routers.api-demo.rule"],
            "Host(`api-demo.app.localhost`)"
        );
        assert_eq!(
            nginx["extra_hosts"],
            json!([
                "api-demo.app.localhost=host-gateway",
                "dashboard-demo.app.localhost=host-gateway",
            ])
        );
        assert_eq!(
            output.hosts["dashboard.app.localhost"],
            "dashboard-demo.app.localhost"
        );
    }

    #[test]
    fn mounts_rewritten_copies_of_the_listed_files() {
        let output = run(&no_overrides);
        let nginx = &output.compose["services"]["web-demo"];
        assert_eq!(
            nginx["volumes"][0]["source"],
            "/bayplan/bays/demo/0-nginx.conf"
        );
        assert_eq!(
            output.files,
            [(
                PathBuf::from("/wt/nginx.conf"),
                PathBuf::from("/bayplan/bays/demo/0-nginx.conf")
            )]
        );
    }

    #[test]
    fn keeps_only_the_named_volumes_in_use() {
        let output = run(&no_overrides);
        assert_eq!(output.compose["volumes"], json!({"cache": {}}));
        assert_eq!(output.compose["networks"]["default"]["name"], "shared");
        assert_eq!(
            output.compose["secrets"]["jwt"]["file"],
            "/wt/secrets/jwt.pem"
        );
    }

    #[test]
    fn applies_overrides_from_the_original_environment() {
        let overrides = |env: &Env| -> Env {
            match env.get("DATABASE_NAME") {
                Some(name) => [("DATABASE_NAME".to_owned(), format!("{name}_bay_sw"))].into(),
                None => Env::new(),
            }
        };
        let output = run(&overrides);
        assert_eq!(
            output.compose["services"]["api-demo"]["environment"]["DATABASE_NAME"],
            "app_bay_sw"
        );
        assert_eq!(output.envs["api"]["DATABASE_NAME"], "app_bay_sw");
    }

    #[test]
    fn joins_the_networks_of_the_main_stack() {
        let output = run(&no_overrides);
        assert_eq!(
            output.compose["networks"],
            json!({
                "default": {"name": "shared", "external": true},
                "back": {"name": "repo_back", "external": true},
                "named": {"name": "named", "external": true},
            })
        );
    }

    #[test]
    fn reaches_services_outside_the_bay_through_their_main_container() {
        let output = run(&no_overrides);
        let api = &output.compose["services"]["api-demo"];
        assert_eq!(
            api["volumes_from"],
            json!(["container:repo-mysql-1:ro", "web-demo", "container:other"])
        );
        assert_eq!(api["links"], json!(["web-demo:web"]));
        assert_eq!(api["pid"], "container:repo-mysql-1");
    }

    #[test]
    fn rejects_unknown_services() {
        let descriptor = descriptor();
        let router = router::from_descriptor(&descriptor);
        let services: BTreeSet<String> = ["nope".to_owned()].into();
        let result = transform(&Input {
            config: &config(),
            services: &services,
            bay: "demo",
            project: "p",
            main_project: "m",
            router: router.as_ref(),
            rewrite_files: &[],
            files_dir: Path::new("/x"),
            env_overrides: &no_overrides,
        });
        assert!(result.is_err());
    }
}
