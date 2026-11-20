use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::compose::runner;
use crate::compose::transform::Env;
use crate::config::MysqlConfig;
use crate::resources::{Context, Resource, load, store, uses_any};

/// Options of a bay: `[resources.mysql]` in its config.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Options {
    /// Gives the bay its own copy of the main database instead of sharing it.
    #[serde(default)]
    clone: bool,
}

#[derive(Serialize, Deserialize, Default)]
struct State {
    /// Cloned database, stored only once completely copied.
    database: Option<String>,
    /// Set while the bay config no longer asks for the clone: kept, but the bay uses the main database.
    #[serde(default)]
    paused: bool,
}

/// Copy of the main database, only when the bay config asks for it.
pub struct Mysql<'a> {
    pub config: &'a MysqlConfig,
}

impl Mysql<'_> {
    fn database(&self, bay: &str) -> String {
        format!("{}_bay_{}", self.config.database, bay.replace('-', "_"))
    }

    fn root_args(&self) -> String {
        format!(r#"-uroot -p"${}""#, self.config.root_password_env)
    }

    fn sql(&self, root: &Path, statement: &str) -> Result<()> {
        let script = format!(
            r#"{} {} -e "{statement}""#,
            self.config.client,
            self.root_args()
        );
        runner::main_exec(root, &self.config.service, &["sh", "-c", &script])
    }

    /// Copies the main database into `database`. The dump goes through a pipe, whose status `sh` reduces to
    /// the one of its last command: the dump status is kept aside, so that a failed dump is not taken for
    /// an empty database.
    fn copy(&self, root: &Path, database: &str) -> Result<()> {
        let config = self.config;
        let auth = self.root_args();
        let script = format!(
            r#"status=$(mktemp); {{ {dump} {auth} --single-transaction --routines --triggers --events {source}; echo $? > "$status"; }} | {client} {auth} {database}; loaded=$?; dumped=$(cat "$status"); rm -f "$status"; [ "$dumped" = 0 ] && [ "$loaded" = 0 ]"#,
            dump = config.dump,
            client = config.client,
            source = config.database,
        );
        runner::main_exec(root, &config.service, &["sh", "-c", &script])
    }
}

impl Resource for Mysql<'_> {
    fn name(&self) -> &'static str {
        "mysql"
    }

    fn provision(&self, context: &Context, state: &mut Value) -> Result<bool> {
        let options: Options = context.options()?;
        let mut typed: State = load(state);
        if let Some(database) = &typed.database {
            if typed.paused == options.clone {
                typed.paused = !options.clone;
                if typed.paused {
                    eprintln!(
                        "warning: the bay uses the main database again; its copy `{database}` is kept until `bp rm`"
                    );
                }
                store(state, &typed)?;
            }
            return Ok(false);
        }
        if !options.clone || !uses_any(context.envs, &self.config.database_vars) {
            return Ok(false);
        }
        let database = self.database(context.bay);
        // No `IF NOT EXISTS`: a database of that name that bayplan did not record is not overwritten.
        self.sql(
            context.root,
            &format!(
                "CREATE DATABASE \\`{database}\\`; GRANT ALL ON \\`{database}\\`.* TO '{}'@'%'",
                self.config.user
            ),
        )?;
        if let Err(error) = self.copy(context.root, &database) {
            let _ = self.sql(
                context.root,
                &format!("DROP DATABASE IF EXISTS \\`{database}\\`"),
            );
            return Err(error);
        }
        store(
            state,
            &State {
                database: Some(database),
                paused: false,
            },
        )?;
        Ok(true)
    }

    fn teardown(&self, root: &Path, state: &mut Value) -> Result<()> {
        let typed: State = load(state);
        if let Some(database) = &typed.database {
            self.sql(root, &format!("DROP DATABASE IF EXISTS \\`{database}\\`"))?;
        }
        *state = Value::Null;
        Ok(())
    }

    fn env(&self, state: &Value, env: &Env) -> Env {
        let typed: State = load(state);
        let Some(database) = typed.database.filter(|_| !typed.paused) else {
            return Env::new();
        };
        self.config
            .database_vars
            .iter()
            .filter(|var| env.contains_key(*var))
            .map(|var| (var.clone(), database.clone()))
            .collect()
    }
}
