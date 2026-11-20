/// Replaces the service names and hosts of the main stack by those of a bay inside free text: env values,
/// Traefik rules, configuration files.
pub struct Rewriter {
    hosts: Vec<(String, String)>,
    services: Vec<(String, String)>,
}

impl Rewriter {
    pub fn new(
        services: impl IntoIterator<Item = (String, String)>,
        hosts: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        let mut services: Vec<_> = services.into_iter().collect();
        let mut hosts: Vec<_> = hosts.into_iter().collect();
        // Longest first, so that `api-admin` is not caught by `api`
        services.sort_by_key(|(name, _)| std::cmp::Reverse(name.len()));
        hosts.sort_by_key(|(host, _)| std::cmp::Reverse(host.len()));
        Self { hosts, services }
    }

    pub fn apply(&self, text: &str) -> String {
        let mut text = text.to_owned();
        for (from, to) in &self.hosts {
            text = replace_bounded(&text, from, to, host_left, host_right);
        }
        for (from, to) in &self.services {
            text = replace_bounded(&text, from, to, service_left, service_right);
        }
        text
    }
}

impl Rewriter {
    /// Rewrites the value of an environment variable. A value that is only a service name, such as
    /// `ROLE=web`, could as well be a word: it is rewritten only for a variable named like an address.
    pub fn apply_env(&self, key: &str, value: &str) -> String {
        let bare_service = self.services.iter().any(|(name, _)| name == value.trim());
        if bare_service && !names_an_address(key) {
            return value.to_owned();
        }
        self.apply(value)
    }
}

/// Words that mark a variable holding an address: `DB_HOST`, `API_URL`, `CACHE_SERVER`, `BROKER_DSN`…
const ADDRESS_WORDS: [&str; 9] = [
    "HOST", "ADDR", "SERVER", "URL", "URI", "DSN", "ENDPOINT", "UPSTREAM", "BACKEND",
];

fn names_an_address(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    ADDRESS_WORDS.iter().any(|word| key.contains(word))
}

fn replace_bounded(
    text: &str,
    from: &str,
    to: &str,
    left: fn(&str) -> bool,
    right: fn(&str) -> bool,
) -> String {
    let mut result = String::with_capacity(text.len());
    let mut last = 0;
    for (index, _) in text.match_indices(from) {
        let end = index + from.len();
        if index < last || !left(&text[..index]) || !right(&text[end..]) {
            continue;
        }
        result.push_str(&text[last..index]);
        result.push_str(to);
        last = end;
    }
    result.push_str(&text[last..]);
    result
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'
}

fn host_left(before: &str) -> bool {
    before.chars().next_back().is_none_or(|c| !is_name_char(c))
}

fn host_right(after: &str) -> bool {
    after.chars().next().is_none_or(|c| !is_name_char(c))
}

/// A service name only counts where a host would stand: a whole value, after `//`, `@` or a separator.
fn service_left(before: &str) -> bool {
    before.ends_with("//")
        || before
            .chars()
            .next_back()
            .is_none_or(|c| c.is_whitespace() || "\"'=,@(".contains(c))
}

/// …and followed by a port, a path or a separator.
fn service_right(after: &str) -> bool {
    after
        .chars()
        .next()
        .is_none_or(|c| c.is_whitespace() || ":/;\"',)".contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rewriter() -> Rewriter {
        Rewriter::new(
            [
                ("api".to_owned(), "api-demo".to_owned()),
                ("web".to_owned(), "web-demo".to_owned()),
                ("node".to_owned(), "node-demo".to_owned()),
                ("mysql".to_owned(), "mysql-demo".to_owned()),
            ],
            [(
                "api.app.localhost".to_owned(),
                "api-demo.app.localhost".to_owned(),
            )],
        )
    }

    #[test]
    fn rewrites_service_names_used_as_hosts() {
        let rewriter = rewriter();
        assert_eq!(rewriter.apply("http://web"), "http://web-demo");
        assert_eq!(rewriter.apply("http://web:80/x"), "http://web-demo:80/x");
        assert_eq!(
            rewriter.apply("server api:9000 resolve;"),
            "server api-demo:9000 resolve;"
        );
        assert_eq!(rewriter.apply("mysql"), "mysql-demo");
        assert_eq!(
            rewriter.apply("amqp://u:p@mysql:3306/x"),
            "amqp://u:p@mysql-demo:3306/x"
        );
    }

    #[test]
    fn leaves_service_names_inside_paths_and_words() {
        let rewriter = rewriter();
        assert_eq!(rewriter.apply("/usr/src/node/x"), "/usr/src/node/x");
        assert_eq!(rewriter.apply("node_modules"), "node_modules");
        assert_eq!(rewriter.apply("--node-env"), "--node-env");
        assert_eq!(rewriter.apply("mysqldump"), "mysqldump");
    }

    #[test]
    fn rewrites_hosts_without_touching_longer_ones() {
        let rewriter = rewriter();
        assert_eq!(
            rewriter.apply("Host(`api.app.localhost`)"),
            "Host(`api-demo.app.localhost`)"
        );
        assert_eq!(
            rewriter.apply("https://api.app.localhost/graphql"),
            "https://api-demo.app.localhost/graphql"
        );
        assert_eq!(
            rewriter.apply("docs-api.app.localhost"),
            "docs-api.app.localhost"
        );
        assert_eq!(
            rewriter.apply("api.app.localhost.example"),
            "api.app.localhost.example"
        );
    }

    #[test]
    fn rewrites_a_bare_service_name_only_in_an_address_variable() {
        let rewriter = rewriter();
        assert_eq!(rewriter.apply_env("DB_HOST", "mysql"), "mysql-demo");
        assert_eq!(rewriter.apply_env("ROLE", "mysql"), "mysql");
        assert_eq!(rewriter.apply_env("ROLE", "http://web"), "http://web-demo");
    }

    #[test]
    fn is_idempotent() {
        let rewriter = rewriter();
        let once = rewriter.apply("http://web https://api.app.localhost");
        assert_eq!(rewriter.apply(&once), once);
    }
}
