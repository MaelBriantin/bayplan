use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::compose::runner;
use crate::compose::transform::Env;
use crate::config::RedisConfig;
use crate::resources::{Context, Resource, load, path_of, store, with_path};

/// Options of a bay: none yet, so that a typo in `[resources.redis]` is reported.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Options {}

#[derive(Serialize, Deserialize, Default)]
struct State {
    /// Index of the main stack → index of the bay.
    indexes: BTreeMap<u8, u8>,
}

/// One database index of the bay for each index the main stack uses, so that caches and sessions stay apart.
pub struct Redis<'a> {
    pub config: &'a RedisConfig,
}

impl Redis<'_> {
    fn indexes_used(&self, env: &Env) -> Vec<(String, u8)> {
        let from_index = self
            .config
            .index_vars
            .iter()
            .filter_map(|var| Some((var.clone(), env.get(var)?.parse().ok()?)));
        let from_dsn = self
            .config
            .dsn_vars
            .iter()
            .filter_map(|var| Some((var.clone(), path_of(env.get(var)?)?.parse().ok()?)));
        from_index.chain(from_dsn).collect()
    }
}

impl Resource for Redis<'_> {
    fn name(&self) -> &'static str {
        "redis"
    }

    fn provision(&self, context: &Context, state: &mut Value) -> Result<bool> {
        let Options {} = context.options()?;
        let mut typed: State = load(state);
        let needed: BTreeSet<u8> = context
            .envs
            .iter()
            .flat_map(|env| self.indexes_used(env))
            .map(|(_, index)| index)
            .filter(|index| !typed.indexes.contains_key(index))
            .collect();
        if needed.is_empty() {
            return Ok(false);
        }
        let [first, last] = self.config.indexes;
        // Never hand out an index the main stack uses: removing the bay flushes its indexes.
        let main: BTreeSet<u8> = context
            .main_envs
            .iter()
            .flat_map(|env| self.indexes_used(env))
            .map(|(_, index)| index)
            .collect();
        let overlap: Vec<String> = main
            .iter()
            .filter(|index| (first..=last).contains(*index))
            .map(ToString::to_string)
            .collect();
        if !overlap.is_empty() {
            eprintln!(
                "warning: the main stack uses Redis index {} inside the range {first}..={last} given to bays: skipped",
                overlap.join(", ")
            );
        }
        let mut taken: BTreeSet<u8> = context
            .others
            .iter()
            .flat_map(|other| load::<State>(other).indexes.into_values())
            .collect();
        taken.extend(main);
        taken.extend(typed.indexes.values());
        for index in needed {
            let Some(free) = (first..=last).find(|i| !taken.contains(i)) else {
                bail!("no free Redis index left in {first}..={last}: remove a bay");
            };
            taken.insert(free);
            typed.indexes.insert(index, free);
        }
        store(state, &typed)?;
        Ok(true)
    }

    fn teardown(&self, root: &Path, state: &mut Value) -> Result<()> {
        let typed: State = load(state);
        for index in typed.indexes.values() {
            runner::main_exec(
                root,
                &self.config.service,
                &["redis-cli", "-n", &index.to_string(), "FLUSHDB"],
            )?;
        }
        *state = Value::Null;
        Ok(())
    }

    /// Flushes only the indexes allocated by the failed run: those the bay had before keep their data.
    fn rollback(&self, root: &Path, before: &Value, state: &mut Value) -> Result<()> {
        let before_typed: State = load(before);
        let typed: State = load(state);
        for (index, bay_index) in &typed.indexes {
            if !before_typed.indexes.contains_key(index) {
                runner::main_exec(
                    root,
                    &self.config.service,
                    &["redis-cli", "-n", &bay_index.to_string(), "FLUSHDB"],
                )?;
            }
        }
        *state = before.clone();
        Ok(())
    }

    fn env(&self, state: &Value, env: &Env) -> Env {
        let typed: State = load(state);
        self.indexes_used(env)
            .into_iter()
            .filter_map(|(var, index)| {
                let bay_index = typed.indexes.get(&index)?.to_string();
                let value = if self.config.dsn_vars.contains(&var) {
                    with_path(&env[&var], &bay_index)
                } else {
                    bay_index
                };
                Some((var, value))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;

    fn config() -> RedisConfig {
        RedisConfig {
            service: "redis".to_owned(),
            indexes: [3, 6],
            index_vars: vec!["REDIS_DATABASE".to_owned()],
            dsn_vars: vec!["REDIS_DSN".to_owned()],
        }
    }

    fn provision(options: &toml::Table, others: &[Value], state: &mut Value) -> Result<bool> {
        let config = config();
        let envs = [Env::from([
            ("REDIS_DATABASE".to_owned(), "0".to_owned()),
            ("REDIS_DSN".to_owned(), "redis://cache:6379/1".to_owned()),
        ])];
        let main_envs = [Env::from([("REDIS_DATABASE".to_owned(), "4".to_owned())])];
        let root = PathBuf::from("/repo");
        let context = Context {
            root: &root,
            bay: "demo",
            options,
            envs: &envs,
            main_envs: &main_envs,
            others,
        };
        Redis { config: &config }.provision(&context, state)
    }

    #[test]
    fn takes_free_indexes_avoiding_the_main_stack_and_the_other_bays() {
        let others = [json!({"indexes": {"0": 3}}), Value::Null];
        let mut state = Value::Null;
        assert!(provision(&toml::Table::new(), &others, &mut state).unwrap());
        assert_eq!(state, json!({"indexes": {"0": 5, "1": 6}}));
        assert!(!provision(&toml::Table::new(), &others, &mut state).unwrap());
    }

    #[test]
    fn rejects_unknown_options() {
        let options = toml::toml! { clone = true };
        assert!(provision(&options, &[], &mut Value::Null).is_err());
    }
}
