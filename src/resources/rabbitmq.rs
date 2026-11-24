use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::compose::runner;
use crate::compose::transform::Env;
use crate::config::RabbitmqConfig;
use crate::resources::{Context, Resource, load, store, uses_any, with_path};

/// Options of a bay: none yet, so that a typo in `[resources.rabbitmq]` is reported.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Options {}

#[derive(Serialize, Deserialize, Default)]
struct State {
    vhost: Option<String>,
}

/// A vhost per bay, so that its consumers do not take the messages of the main stack.
pub struct Rabbitmq<'a> {
    pub config: &'a RabbitmqConfig,
}

impl Resource for Rabbitmq<'_> {
    fn name(&self) -> &'static str {
        "rabbitmq"
    }

    fn provision(&self, context: &Context, state: &mut Value) -> Result<bool> {
        let Options {} = context.options()?;
        let mut typed: State = load(state);
        if typed.vhost.is_some() || !uses_any(context.envs, &self.config.dsn_vars) {
            return Ok(false);
        }
        let vhost = format!("bay-{}", context.bay);
        let service = &self.config.service;
        runner::main_exec(context.root, service, &["rabbitmqctl", "add_vhost", &vhost])?;
        let permitted = runner::main_exec(
            context.root,
            service,
            &[
                "rabbitmqctl",
                "set_permissions",
                "-p",
                &vhost,
                &self.config.user,
                ".*",
                ".*",
                ".*",
            ],
        );
        if let Err(error) = permitted {
            let _ = runner::main_exec(
                context.root,
                service,
                &["rabbitmqctl", "delete_vhost", &vhost],
            );
            return Err(error);
        }
        typed.vhost = Some(vhost);
        store(state, &typed)?;
        Ok(true)
    }

    fn teardown(&self, root: &Path, state: &mut Value) -> Result<()> {
        let typed: State = load(state);
        if let Some(vhost) = &typed.vhost {
            runner::main_exec(
                root,
                &self.config.service,
                &["rabbitmqctl", "delete_vhost", vhost],
            )?;
        }
        *state = Value::Null;
        Ok(())
    }

    fn env(&self, state: &Value, env: &Env) -> Env {
        let typed: State = load(state);
        let Some(vhost) = &typed.vhost else {
            return Env::new();
        };
        self.config
            .dsn_vars
            .iter()
            .filter_map(|var| env.get(var).map(|dsn| (var.clone(), with_path(dsn, vhost))))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;
    use crate::exec::tests::FakeDocker;

    fn config() -> RabbitmqConfig {
        RabbitmqConfig {
            service: "rabbitmq".to_owned(),
            user: "app".to_owned(),
            dsn_vars: vec!["MESSENGER_DSN".to_owned()],
        }
    }

    fn dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("bayplan-rabbitmq-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn points_the_dsn_at_the_vhost_keeping_its_query() {
        let config = config();
        let rabbitmq = Rabbitmq { config: &config };
        let env = Env::from([
            (
                "MESSENGER_DSN".to_owned(),
                "amqp://app:app@rabbitmq:5672/%2f?heartbeat=30".to_owned(),
            ),
            (
                "OTHER_DSN".to_owned(),
                "amqp://rabbitmq:5672/%2f".to_owned(),
            ),
        ]);
        assert_eq!(
            rabbitmq.env(&json!({"vhost": "bay-demo"}), &env),
            Env::from([(
                "MESSENGER_DSN".to_owned(),
                "amqp://app:app@rabbitmq:5672/bay-demo?heartbeat=30".to_owned()
            )])
        );
        assert!(rabbitmq.env(&Value::Null, &env).is_empty());
    }

    #[test]
    fn deletes_the_vhost_when_its_permissions_fail() {
        let dir = dir("provision-failed");
        let docker = FakeDocker::install(&dir);
        docker.fail_on("set_permissions");
        let config = config();
        let envs = [Env::from([(
            "MESSENGER_DSN".to_owned(),
            "amqp://rabbitmq:5672/%2f".to_owned(),
        )])];
        let context = Context {
            root: &dir,
            bay: "demo",
            options: &toml::Table::new(),
            envs: &envs,
            main_envs: &[],
            others: &[],
        };
        let mut state = Value::Null;
        assert!(
            Rabbitmq { config: &config }
                .provision(&context, &mut state)
                .is_err()
        );
        assert_eq!(state, Value::Null);
        assert!(docker.calls()[2].ends_with("rabbitmqctl delete_vhost bay-demo"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn deletes_the_vhost_on_teardown_and_keeps_it_when_that_fails() {
        let dir = dir("teardown");
        let docker = FakeDocker::install(&dir);
        let config = config();
        let rabbitmq = Rabbitmq { config: &config };

        docker.fail_on("delete_vhost");
        let mut state = json!({"vhost": "bay-demo"});
        assert!(rabbitmq.teardown(&dir, &mut state).is_err());
        assert_eq!(state, json!({"vhost": "bay-demo"}));

        std::fs::remove_file(dir.join("fail")).unwrap();
        rabbitmq.teardown(&dir, &mut state).unwrap();
        assert_eq!(state, Value::Null);
        assert_eq!(
            docker.calls()[1],
            "compose exec -T rabbitmq rabbitmqctl delete_vhost bay-demo"
        );
        std::fs::remove_dir_all(dir).ok();
    }
}
