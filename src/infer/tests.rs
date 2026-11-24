use serde_json::json;

use super::*;
use crate::config::Descriptor;

fn write_files(root: &Path) {
    let files = [
        (
            "compose.yaml",
            "services:\n  api:\n    env_file: .env.local\n",
        ),
        (".env.local", "SECRET=1"),
        (
            "compose.override.yaml",
            "services:\n  auth:\n    env_file: .bayplan/auth.env\n",
        ),
        (".bayplan/auth.env", ""),
        (
            "compose.e2e.yaml",
            "services:\n  e2e:\n    volumes: [certs/e2e.pem]\n",
        ),
        ("certs/e2e.pem", "cert"),
        ("secrets/jwt.pem", "key"),
        ("api/composer.lock", "{}"),
        ("api/vendor/autoload.php", ""),
        ("api/.env.local", "APP_ENV=dev"),
        ("api/.ssh/id_rsa", "key"),
        ("api/var/cache.txt", ""),
        ("docker/nginx.conf", "location / { fastcgi_pass api:9000; }"),
        ("web/pnpm-lock.yaml", ""),
        ("web/node_modules/react/index.js", ""),
    ];
    for (path, content) in files {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
}

/// A repository shaped as a real one: a router, an API behind nginx, a front, a worker, the shared
/// services, untracked env files, secrets and dependency directories.
fn repo(name: &str) -> Repo {
    let root = std::env::temp_dir().join(format!("bayplan-infer-{name}-{}", std::process::id()));
    std::fs::remove_dir_all(&root).ok();
    write_files(&root);
    let at = |path: &str| root.join(path).to_string_lossy().into_owned();
    let bind = |path: &str| json!({"type": "bind", "source": at(path), "target": "/app"});
    let config = json!({
        "name": "shop",
        "services": {
            "traefik": {"image": "traefik:v3.6", "labels": {}},
            "nginx": {
                "build": {"context": at("docker")},
                "labels": {"traefik.http.routers.api.rule": "Host(`api.app.localhost`)"},
                "depends_on": {"api": {}},
                "volumes": [bind("docker/nginx.conf")],
            },
            "api": {
                "build": {"context": at("api")},
                "volumes": [bind("api")],
                "environment": {
                    "DATABASE_NAME": "app",
                    "DATABASE_USER": "app",
                    "MESSENGER_DSN": "amqp://app:app@rabbitmq:5672/%2f",
                    "REDIS_DSN": "redis://redis:6379/2?timeout=5",
                    "REDIS_DATABASE": "1",
                },
            },
            "worker": {"image": "shop-api", "volumes": [bind("api")]},
            "api-image": {"build": {"context": at("api")}, "image": "shop-api"},
            "docs": {"build": {"context": at("web")}, "volumes": [bind("api")]},
            "mailpit": {
                "image": "axllent/mailpit:v1",
                "labels": {"traefik.http.routers.mail.rule": "Host(`mail.app.localhost`)"},
                "volumes": [bind("docker")],
            },
            "web": {
                "build": {"context": at("web")},
                "labels": {"traefik.http.routers.web.rule": "Host(`web.app.localhost`)"},
                "volumes": [bind("web")],
            },
            "mysql": {
                "build": {"context": at("docker/mysql")},
                "environment": {
                    "MARIADB_ROOT_PASSWORD": "root",
                    "MARIADB_DATABASE": "app",
                    "MARIADB_USER": "app",
                },
            },
            "rabbitmq": {"image": "rabbitmq:4", "environment": {"RABBITMQ_DEFAULT_USER": "app"}},
            "redis": {"image": "redis:8"},
            "redis-sentinel": {"image": "redis:8", "depends_on": {"redis": {}}},
            "analytics": {"image": "postgres:15"},
        },
        "secrets": {"jwt": {"file": at("secrets/jwt.pem")}},
    });
    let tracked = [
        "compose.yaml",
        "api/composer.lock",
        "api/src/Kernel.php",
        "docker/nginx.conf",
        "web/pnpm-lock.yaml",
        "web/package.json",
    ];
    let untracked = [
        ".env.local",
        "compose.override.yaml",
        ".bayplan",
        "certs",
        "secrets",
        "api/vendor",
        "api/.env.local",
        "api/.ssh",
        "api/var",
        "web/node_modules",
    ];
    Repo {
        root,
        config,
        tracked: tracked.iter().map(|path| (*path).to_owned()).collect(),
        untracked: untracked.iter().map(|path| (*path).to_owned()).collect(),
    }
}

fn values(found: &[Why<String>]) -> Vec<&str> {
    found.iter().map(|found| found.value.as_str()).collect()
}

#[test]
fn infers_the_router_and_its_domain() {
    let repo = repo("router");
    let router = infer(&repo).router.unwrap();
    assert_eq!(router.value, "app.localhost");
    assert_eq!(
        router.why,
        "3 services carry Traefik rules; their hosts end with app.localhost"
    );
    assert_eq!(
        common_domain(&["app.localhost".into(), "api.app.localhost".into()]).as_deref(),
        Some("app.localhost")
    );
    assert_eq!(common_domain(&["a.com".into(), "b.org".into()]), None);
    std::fs::remove_dir_all(repo.root).ok();
}

#[test]
fn groups_each_exposed_service_with_what_it_needs_then_the_others_alone() {
    let repo = repo("groups");
    let groups: Vec<(String, Vec<String>)> = infer(&repo)
        .groups
        .into_iter()
        .map(|group| group.value)
        .collect();
    let group = |name: &str, members: &[&str]| {
        (
            name.to_owned(),
            members.iter().map(|m| (*m).to_owned()).collect::<Vec<_>>(),
        )
    };
    assert_eq!(
        groups,
        [
            group("nginx", &["nginx", "api"]),
            group("web", &["web"]),
            group("docs", &["docs"]),
            group("worker", &["worker"]),
        ]
    );
    std::fs::remove_dir_all(repo.root).ok();
}

#[test]
fn links_what_compose_reads_and_copies_what_containers_read() {
    let repo = repo("files");
    let inference = infer(&repo);
    assert_eq!(
        values(&inference.link),
        ["compose.override.yaml", "secrets/jwt.pem", ".env.local"]
    );
    assert_eq!(inference.link[2].why, "not in git, named in compose.yaml");
    assert_eq!(values(&inference.copy), ["api/.env.local", "api/.ssh"]);
    assert_eq!(
        inference.copy[0].why,
        "not in git, under api, mounted into api and 2 other services"
    );
    assert_eq!(values(&inference.rewrite_files), ["docker/nginx.conf"]);
    assert_eq!(
        inference.rewrite_files[0].why,
        "names the service api, mounted into nginx"
    );
    std::fs::remove_dir_all(repo.root).ok();
}

#[test]
fn finds_the_dependencies_beside_their_lockfile() {
    let repo = repo("deps");
    let deps: Vec<DepGuess> = infer(&repo).deps.into_iter().map(|dep| dep.value).collect();
    assert_eq!(
        deps,
        [
            DepGuess {
                path: "api/vendor".into(),
                lockfile: "api/composer.lock".into(),
                services: vec!["api".into(), "worker".into()],
                command: vec!["composer".into(), "install".into()],
            },
            DepGuess {
                path: "web/node_modules".into(),
                lockfile: "web/pnpm-lock.yaml".into(),
                services: vec!["web".into()],
                command: vec!["pnpm".into(), "install".into()],
            },
        ]
    );
    std::fs::remove_dir_all(repo.root).ok();
}

#[test]
fn finds_the_shared_resources_and_tells_the_ones_it_cannot_handle() {
    let repo = repo("resources");
    let inference = infer(&repo);
    assert_eq!(
        inference.mysql.unwrap().value,
        MysqlGuess {
            service: "mysql".into(),
            root_password_env: "MARIADB_ROOT_PASSWORD".into(),
            database: "app".into(),
            user: "app".into(),
            database_vars: vec!["DATABASE_NAME".into()],
            tools: None,
        }
    );
    assert_eq!(
        inference.rabbitmq.unwrap().value,
        RabbitmqGuess {
            service: "rabbitmq".into(),
            user: "app".into(),
            dsn_vars: vec!["MESSENGER_DSN".into()],
        }
    );
    assert_eq!(
        inference.redis.unwrap().value,
        RedisGuess {
            service: "redis".into(),
            indexes: [3, 15],
            index_vars: vec!["REDIS_DATABASE".into()],
            dsn_vars: vec!["REDIS_DSN".into()],
        }
    );
    assert!(inference.notes[0].starts_with("analytics looks like PostgreSQL"));
    std::fs::remove_dir_all(repo.root).ok();
}

#[test]
fn writes_a_descriptor_bayplan_reads() {
    let repo = repo("render");
    let text = render::render(&infer(&repo));
    let descriptor: Descriptor = toml::from_str(&text).unwrap();
    assert_eq!(descriptor.router.domain, "app.localhost");
    assert_eq!(descriptor.groups["nginx"], ["nginx", "api"]);
    assert_eq!(descriptor.deps.len(), 2);
    assert!(descriptor.resources.redis.is_some());
    assert!(text.contains("# names the service api, mounted into nginx"));
    assert!(text.contains("# - analytics looks like PostgreSQL"));
    std::fs::remove_dir_all(repo.root).ok();
}
