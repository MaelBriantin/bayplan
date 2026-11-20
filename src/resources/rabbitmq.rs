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
