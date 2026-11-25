//! The descriptor bayplan runs with: what `bayplan.toml` states, and what it leaves out inferred.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use super::{Inference, Repo, Why, infer};
use crate::config::{Dep, Descriptor, Install, Resources, RouterConfig, RouterKind, Stated};

/// What `bayplan.toml` states, completed with what is inferred; returns it with the lines telling what was
/// inferred, and what the bays share for want of a stated resource.
pub fn complete(root: &Path, stated: Stated) -> Result<(Descriptor, Vec<String>)> {
    if stated.is_complete() {
        return fill(stated, &Inference::default());
    }
    fill(stated, &infer(&Repo::read(root)?))
}

fn paths(
    stated: Option<Vec<PathBuf>>,
    found: &[Why<String>],
    key: &str,
    told: &mut Vec<String>,
) -> Vec<PathBuf> {
    stated.unwrap_or_else(|| {
        if !found.is_empty() {
            let values: Vec<&str> = found.iter().map(|found| found.value.as_str()).collect();
            told.push(format!("inferred {key}: {}", values.join(", ")));
        }
        found
            .iter()
            .map(|found| PathBuf::from(&found.value))
            .collect()
    })
}

pub fn fill(stated: Stated, inference: &Inference) -> Result<(Descriptor, Vec<String>)> {
    let mut told = Vec::new();
    let router = if let Some(router) = stated.router {
        router
    } else {
        let Some(found) = &inference.router else {
            bail!("bayplan.toml states no [router], and none can be inferred: state it");
        };
        told.push(format!(
            "inferred router: Traefik, domain {} ({})",
            found.value, found.why
        ));
        RouterConfig {
            kind: RouterKind::Traefik,
            domain: found.value.clone(),
        }
    };
    let link = paths(stated.link, &inference.link, "link", &mut told);
    let copy = paths(stated.copy, &inference.copy, "copy", &mut told);
    let rewrite_files = paths(
        stated.rewrite_files,
        &inference.rewrite_files,
        "rewrite_files",
        &mut told,
    );
    let groups = stated.groups.unwrap_or_else(|| {
        let names: Vec<&str> = inference
            .groups
            .iter()
            .map(|g| g.value.0.as_str())
            .collect();
        if !names.is_empty() {
            told.push(format!("inferred groups: {}", names.join(", ")));
        }
        inference
            .groups
            .iter()
            .map(|group| group.value.clone())
            .collect()
    });
    let deps = stated.deps.unwrap_or_else(|| {
        let found: Vec<String> = inference
            .deps
            .iter()
            .map(|dep| format!("{} ({})", dep.value.path, dep.value.command.join(" ")))
            .collect();
        if !found.is_empty() {
            told.push(format!("inferred deps: {}", found.join(", ")));
        }
        inference
            .deps
            .iter()
            .map(|dep| Dep {
                path: PathBuf::from(&dep.value.path),
                lockfile: PathBuf::from(&dep.value.lockfile),
                install: Some(Install {
                    services: dep.value.services.clone(),
                    command: dep.value.command.clone(),
                }),
            })
            .collect()
    });

    warn_shared(&stated.resources, inference, &mut told);
    if told.iter().any(|line| line.starts_with("inferred")) {
        told.push(
            "`bp init --write` writes what is inferred into bayplan.toml, to review and keep"
                .to_owned(),
        );
    }

    let descriptor = Descriptor {
        router,
        link,
        copy,
        rewrite_files,
        groups,
        deps,
        resources: stated.resources,
        registrations: stated.registrations,
    };
    Ok((descriptor, told))
}

/// Resources change the shared services of the main stack: never inferred, only told when found. What
/// bayplan cannot handle, as a PostgreSQL, is left to the notes of `bp init`.
fn warn_shared(resources: &Resources, inference: &Inference, told: &mut Vec<String>) {
    if let (Some(found), None) = (&inference.mysql, &resources.mysql) {
        told.push(format!(
            "warning: the bays write to the main stack's database {}; `bp init` proposes [resources.mysql] for a copy of their own",
            found.value.database
        ));
    }
    if let (Some(found), None) = (&inference.rabbitmq, &resources.rabbitmq) {
        told.push(format!(
            "warning: the bays' consumers share the queues of {}; `bp init` proposes [resources.rabbitmq] for a vhost of their own",
            found.value.service
        ));
    }
    if let (Some(found), None) = (&inference.redis, &resources.redis) {
        told.push(format!(
            "warning: the bays share the databases of {}; `bp init` proposes [resources.redis] for databases of their own",
            found.value.service
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::super::{DepGuess, RedisGuess, why};
    use super::*;

    fn inference() -> Inference {
        Inference {
            router: Some(why("app.localhost".to_owned(), "2 services")),
            link: vec![why(".env.local".to_owned(), "named in compose.yaml")],
            groups: vec![why(
                (
                    "nginx".to_owned(),
                    vec!["nginx".to_owned(), "api".to_owned()],
                ),
                "exposed",
            )],
            deps: vec![why(
                DepGuess {
                    path: "web/node_modules".into(),
                    lockfile: "web/pnpm-lock.yaml".into(),
                    services: vec!["web".into()],
                    command: vec!["pnpm".into(), "install".into()],
                },
                "beside its lockfile",
            )],
            redis: Some(why(
                RedisGuess {
                    service: "redis".into(),
                    indexes: [3, 15],
                    index_vars: Vec::new(),
                    dsn_vars: vec!["REDIS_DSN".into()],
                },
                "a Redis server",
            )),
            notes: vec!["db looks like PostgreSQL: bayplan has no resource for it yet".into()],
            ..Inference::default()
        }
    }

    #[test]
    fn infers_what_is_not_stated_and_says_so() {
        let (descriptor, told) = fill(Stated::default(), &inference()).unwrap();
        assert_eq!(descriptor.router.domain, "app.localhost");
        assert_eq!(descriptor.link, [PathBuf::from(".env.local")]);
        assert_eq!(descriptor.groups["nginx"], ["nginx", "api"]);
        assert_eq!(
            descriptor.deps[0].install.as_ref().unwrap().command,
            ["pnpm", "install"]
        );
        assert!(descriptor.resources.redis.is_none());
        assert_eq!(
            told,
            [
                "inferred router: Traefik, domain app.localhost (2 services)",
                "inferred link: .env.local",
                "inferred groups: nginx",
                "inferred deps: web/node_modules (pnpm install)",
                "warning: the bays share the databases of redis; `bp init` proposes [resources.redis] for databases of their own",
                "`bp init --write` writes what is inferred into bayplan.toml, to review and keep",
            ]
        );
    }

    #[test]
    fn keeps_what_is_stated_even_empty() {
        let stated: Stated = toml::from_str(
            r#"
            link = []
            [router]
            kind = "traefik"
            domain = "shop.localhost"
            [groups]
            back = ["api"]
            [resources.redis]
            service = "redis"
            indexes = [5, 9]
            "#,
        )
        .unwrap();
        let (descriptor, told) = fill(stated, &inference()).unwrap();
        assert_eq!(descriptor.router.domain, "shop.localhost");
        assert_eq!(descriptor.link, Vec::<PathBuf>::new());
        assert_eq!(descriptor.groups.len(), 1);
        assert!(
            !told
                .iter()
                .any(|line| line.contains("router") || line.contains("redis"))
        );
        assert!(told.contains(&"inferred deps: web/node_modules (pnpm install)".to_owned()));
    }

    #[test]
    fn needs_a_router_stated_or_inferred() {
        assert!(fill(Stated::default(), &Inference::default()).is_err());
    }
}
