mod mysql;
mod rabbitmq;
mod redis;

use std::path::Path;

use anyhow::{Context as _, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::compose::transform::Env;
use crate::config::Resources;

pub struct Context<'a> {
    /// Root of the main tree, where the shared services are reached with `docker compose exec`.
    pub root: &'a Path,
    pub bay: &'a str,
    /// Options the bay config gives this resource.
    pub options: &'a toml::Table,
    /// Original environments of the duplicated services.
    pub envs: &'a [Env],
    /// Environments of every service of the main stack, to avoid what it uses itself.
    pub main_envs: &'a [Env],
    /// States of this resource in the other bays, to avoid what they hold.
    pub others: &'a [Value],
}

impl Context<'_> {
    /// Options of the resource, rejecting unknown ones.
    pub fn options<T: DeserializeOwned>(&self) -> Result<T> {
        self.options
            .clone()
            .try_into()
            .context("invalid options in the bay config")
    }
}

/// Something of the shared infrastructure a bay gets its own share of: a database, a vhost, an index. Its
/// state is its own, stored opaquely in the bay state.
pub trait Resource {
    fn name(&self) -> &'static str;

    /// Creates the share of the bay unless its state says it exists. Returns whether it created something.
    fn provision(&self, context: &Context, state: &mut Value) -> Result<bool>;

    /// Removes the share of the bay, leaving its state `null`. On failure, the state keeps what is left.
    fn teardown(&self, root: &Path, state: &mut Value) -> Result<()>;

    /// Undoes what `provision` added in a run that failed later, given the state before it: by default, the
    /// whole share when the bay had none.
    fn rollback(&self, root: &Path, before: &Value, state: &mut Value) -> Result<()> {
        if before.is_null() {
            self.teardown(root, state)
        } else {
            Ok(())
        }
    }

    /// Variables pointing a service at the share of the bay, from the original environment of the service.
    fn env(&self, state: &Value, env: &Env) -> Env;

    /// What the bay has of the resource, in a few words for the people running it: its own share, or the
    /// main stack's when it has none.
    fn describe(&self, state: &Value) -> String;
}

/// Reads the typed state of a resource: the default one when the bay has none yet, or an unreadable one.
fn load<T: DeserializeOwned + Default>(state: &Value) -> T {
    serde_json::from_value(state.clone()).unwrap_or_default()
}

fn store<T: Serialize>(state: &mut Value, typed: &T) -> Result<()> {
    *state = serde_json::to_value(typed)?;
    Ok(())
}

pub fn from_config(resources: &Resources) -> Vec<Box<dyn Resource + '_>> {
    let mut list: Vec<Box<dyn Resource>> = Vec::new();
    if let Some(config) = &resources.mysql {
        list.push(Box::new(mysql::Mysql { config }));
    }
    if let Some(config) = &resources.rabbitmq {
        list.push(Box::new(rabbitmq::Rabbitmq { config }));
    }
    if let Some(config) = &resources.redis {
        list.push(Box::new(redis::Redis { config }));
    }
    list
}

fn uses_any(envs: &[Env], vars: &[String]) -> bool {
    envs.iter()
        .any(|env| vars.iter().any(|var| env.contains_key(var)))
}

/// Replaces the path of a URL, keeping its query: `amqp://h:5672/%2f?x` → `amqp://h:5672/<path>?x`.
fn with_path(url: &str, path: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (url, None),
    };
    let authority_end = base
        .find("://")
        .map(|scheme| scheme + 3)
        .and_then(|start| base[start..].find('/').map(|slash| start + slash))
        .unwrap_or(base.len());
    let mut result = format!("{}/{path}", &base[..authority_end]);
    if let Some(query) = query {
        result.push('?');
        result.push_str(query);
    }
    result
}

/// Last path segment of a URL: `redis://h:26379/2?x` → `2`.
fn path_of(url: &str) -> Option<&str> {
    let base = url.split('?').next()?;
    let after_scheme = &base[base.find("://")? + 3..];
    let path = &after_scheme[after_scheme.find('/')? + 1..];
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_the_path_and_keeps_the_query() {
        assert_eq!(
            with_path("amqp://app:app@rabbitmq:5672/%2f", "bay-demo"),
            "amqp://app:app@rabbitmq:5672/bay-demo"
        );
        assert_eq!(
            with_path("redis://cache:6379/0?timeout=5", "4"),
            "redis://cache:6379/4?timeout=5"
        );
        assert_eq!(with_path("redis://host", "4"), "redis://host/4");
    }

    #[test]
    fn reads_the_path() {
        assert_eq!(path_of("redis://cache:6379/0?timeout=5"), Some("0"));
        assert_eq!(path_of("redis://host"), None);
    }
}
