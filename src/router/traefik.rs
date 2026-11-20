use std::collections::BTreeSet;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

use crate::compose::Rewriter;
use crate::router::Router;

/// `traefik.<protocol>.<kind>.<name>.<rest>`
static LABEL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^traefik\.(http|tcp|udp)\.(routers|services|middlewares)\.([^.]+)\.(.+)$").unwrap()
});

/// `Host(...)` or `HostSNI(...)`, with one or several hosts.
static HOST: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bHost(?:SNI)?\(([^)]*)\)").unwrap());

/// A quoted argument: Traefik accepts backticks and double quotes.
static ARGUMENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"[`"]([^`"]+)[`"]"#).unwrap());

/// Routes the bay through the Traefik of the main stack, on hosts suffixed with the bay name.
pub struct Traefik {
    domain: String,
}

impl Traefik {
    pub fn new(domain: &str) -> Self {
        Self {
            domain: domain.to_owned(),
        }
    }
}

impl Router for Traefik {
    fn hosts(&self, service: &Value) -> Vec<String> {
        let Some(labels) = service.get("labels").and_then(Value::as_object) else {
            return Vec::new();
        };
        labels
            .iter()
            .filter(|(key, _)| key.rsplit('.').next() == Some("rule"))
            .filter_map(|(_, value)| value.as_str())
            .flat_map(|rule| {
                HOST.captures_iter(rule)
                    .flat_map(|call| {
                        ARGUMENT
                            .captures_iter(&call[1])
                            .map(|argument| argument[1].to_owned())
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>()
            })
            .filter(|host| host != "*")
            .collect()
    }

    /// Keeps a single label under the domain, so that the wildcard certificate of the domain still applies.
    fn bay_host(&self, host: &str, bay: &str) -> String {
        if host == self.domain {
            return format!("bay-{bay}.{}", self.domain);
        }
        match host.split_once('.') {
            Some((first, rest)) => format!("{first}-{bay}.{rest}"),
            None => format!("{host}-{bay}"),
        }
    }

    fn rewrite(&self, service: &mut Value, bay: &str, rewriter: &Rewriter) -> Vec<String> {
        let Some(labels) = service.get_mut("labels").and_then(Value::as_object_mut) else {
            return Vec::new();
        };
        let local: BTreeSet<String> = labels
            .keys()
            .filter_map(|key| LABEL.captures(key).map(|c| c[3].to_owned()))
            .collect();
        let suffixed = |name: &str| {
            let (name, provider) = match name.split_once('@') {
                Some((name, provider)) => (name, Some(provider)),
                None => (name, None),
            };
            if !local.contains(name) {
                return provider.map_or(name.to_owned(), |p| format!("{name}@{p}"));
            }
            provider.map_or(format!("{name}-{bay}"), |p| format!("{name}-{bay}@{p}"))
        };

        let mut rewritten = Map::new();
        // Routers whose rule names no host of the bay: kept, they would take requests of the main stack
        let mut unroutable = BTreeSet::new();
        let mut warnings = Vec::new();
        for (key, value) in std::mem::take(labels) {
            let Some(captures) = LABEL.captures(&key) else {
                rewritten.insert(key, value);
                continue;
            };
            let rest = &captures[4];
            let key = format!(
                "traefik.{}.{}.{}-{bay}.{rest}",
                &captures[1], &captures[2], &captures[3]
            );
            if captures[2] == *"routers" && rest == "rule" {
                let rule = value.as_str().unwrap_or_default();
                if rewriter.apply(rule) == rule {
                    warnings.push(format!(
                        "router `{}`: its rule `{rule}` names no host bayplan can rewrite, so the bay does not expose it",
                        &captures[3]
                    ));
                    unroutable.insert(format!(
                        "traefik.{}.routers.{}-{bay}.",
                        &captures[1], &captures[3]
                    ));
                }
            }
            let value = match value.as_str() {
                Some(text) if rest == "rule" => Value::String(rewriter.apply(text)),
                Some(text) if rest == "service" => Value::String(suffixed(text)),
                Some(text) if rest == "middlewares" => Value::String(
                    text.split(',')
                        .map(|m| suffixed(m.trim()))
                        .collect::<Vec<_>>()
                        .join(","),
                ),
                _ => value,
            };
            rewritten.insert(key, value);
        }
        rewritten.retain(|key, _| !unroutable.iter().any(|prefix| key.starts_with(prefix)));
        *labels = rewritten;
        warnings
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn suffixes_the_first_label_and_moves_the_apex_under_the_domain() {
        let traefik = Traefik::new("app.localhost");
        assert_eq!(
            traefik.bay_host("dashboard.app.localhost", "demo"),
            "dashboard-demo.app.localhost"
        );
        assert_eq!(
            traefik.bay_host("app.localhost", "demo"),
            "bay-demo.app.localhost"
        );
    }

    #[test]
    fn reads_hosts_from_rules() {
        let traefik = Traefik::new("app.localhost");
        let service = json!({"labels": {
            "traefik.http.routers.api.rule": "Host(`api.app.localhost`)",
            "traefik.http.routers.api.tls": "true",
        }});
        assert_eq!(traefik.hosts(&service), vec!["api.app.localhost"]);
    }

    #[test]
    fn reads_every_host_form() {
        let traefik = Traefik::new("app.localhost");
        let service = json!({"labels": {
            "traefik.http.routers.a.rule": "Host(`a.app.localhost`, `b.app.localhost`)",
            "traefik.http.routers.c.rule": "Host(\"c.app.localhost\") && PathPrefix(`/x`)",
            "traefik.tcp.routers.d.rule": "HostSNI(`d.app.localhost`)",
            "traefik.tcp.routers.e.rule": "HostSNI(`*`)",
        }});
        assert_eq!(
            traefik.hosts(&service),
            vec![
                "a.app.localhost",
                "b.app.localhost",
                "c.app.localhost",
                "d.app.localhost"
            ]
        );
    }

    #[test]
    fn drops_the_routers_it_cannot_move_to_the_bay() {
        let traefik = Traefik::new("app.localhost");
        let rewriter = Rewriter::new([], []);
        let mut service = json!({"labels": {
            "traefik.http.routers.api.rule": "PathPrefix(`/api`)",
            "traefik.http.routers.api.tls": "true",
            "traefik.http.services.api.loadbalancer.server.port": "80",
        }});
        let warnings = traefik.rewrite(&mut service, "demo", &rewriter);
        assert_eq!(warnings.len(), 1);
        assert_eq!(
            service["labels"],
            json!({"traefik.http.services.api-demo.loadbalancer.server.port": "80"})
        );
    }

    #[test]
    fn renames_routers_services_and_local_middlewares() {
        let traefik = Traefik::new("app.localhost");
        let rewriter = Rewriter::new(
            [],
            [(
                "auth.app.localhost".to_owned(),
                "auth-demo.app.localhost".to_owned(),
            )],
        );
        let mut service = json!({"labels": {
            "traefik.enable": "true",
            "traefik.http.routers.auth.rule": "Host(`auth.app.localhost`)",
            "traefik.http.routers.auth.service": "auth",
            "traefik.http.routers.auth.middlewares": "gate, sso@docker, forward@file",
            "traefik.http.middlewares.gate.forwardauth.address": "http://gate:4181",
            "traefik.http.services.auth.loadbalancer.server.port": "3000",
        }});
        assert_eq!(
            traefik.rewrite(&mut service, "demo", &rewriter),
            Vec::<String>::new()
        );
        assert_eq!(
            service["labels"],
            json!({
                "traefik.enable": "true",
                "traefik.http.routers.auth-demo.rule": "Host(`auth-demo.app.localhost`)",
                "traefik.http.routers.auth-demo.service": "auth-demo",
                "traefik.http.routers.auth-demo.middlewares": "gate-demo,sso@docker,forward@file",
                "traefik.http.middlewares.gate-demo.forwardauth.address": "http://gate:4181",
                "traefik.http.services.auth-demo.loadbalancer.server.port": "3000",
            })
        );
    }
}
