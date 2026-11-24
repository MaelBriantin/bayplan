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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;
    use crate::exec::tests::FakeDocker;

    fn config() -> MysqlConfig {
        MysqlConfig {
            service: "mysql".to_owned(),
            client: "mariadb".to_owned(),
            dump: "mariadb-dump".to_owned(),
            root_password_env: "MYSQL_ROOT_PASSWORD".to_owned(),
            database: "app".to_owned(),
            user: "app".to_owned(),
            database_vars: vec!["DATABASE_NAME".to_owned()],
        }
    }

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bayplan-mysql-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        dir
    }

    fn provision(root: &Path, state: &mut Value) -> Result<bool> {
        let config = config();
        let envs = [Env::from([("DATABASE_NAME".to_owned(), "app".to_owned())])];
        let context = Context {
            root,
            bay: "feat-1",
            options: &toml::toml! { clone = true },
            envs: &envs,
            main_envs: &[],
            others: &[],
        };
        Mysql { config: &config }.provision(&context, state)
    }

    #[test]
    fn points_the_database_variables_at_the_clone() {
        let config = config();
        let mysql = Mysql { config: &config };
        let env = Env::from([
            ("DATABASE_NAME".to_owned(), "app".to_owned()),
            ("DATABASE_HOST".to_owned(), "mysql".to_owned()),
        ]);
        assert_eq!(
            mysql.env(&json!({"database": "app_bay_demo"}), &env),
            Env::from([("DATABASE_NAME".to_owned(), "app_bay_demo".to_owned())])
        );
        assert!(
            mysql
                .env(&json!({"database": "app_bay_demo", "paused": true}), &env)
                .is_empty()
        );
        assert!(mysql.env(&Value::Null, &env).is_empty());
    }

    #[test]
    fn records_the_clone_once_copied() {
        let dir = dir("clone");
        let docker = FakeDocker::install(&dir);
        let mut state = Value::Null;
        assert!(provision(&dir, &mut state).unwrap());
        assert_eq!(
            state,
            json!({"database": "app_bay_feat_1", "paused": false})
        );
        let calls = docker.calls();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].contains("CREATE DATABASE \\`app_bay_feat_1\\`"));
        assert!(calls[1].contains("mariadb-dump"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn drops_the_clone_when_the_copy_fails() {
        let dir = dir("clone-failed");
        let docker = FakeDocker::install(&dir);
        docker.fail_on("mariadb-dump");
        let mut state = Value::Null;
        assert!(provision(&dir, &mut state).is_err());
        assert_eq!(state, Value::Null);
        assert!(docker.calls()[2].contains("DROP DATABASE IF EXISTS \\`app_bay_feat_1\\`"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn drops_the_clone_on_teardown_and_keeps_it_when_that_fails() {
        let dir = dir("teardown");
        let docker = FakeDocker::install(&dir);
        let config = config();
        let mysql = Mysql { config: &config };

        docker.fail_on("DROP DATABASE");
        let mut state = json!({"database": "app_bay_demo"});
        assert!(mysql.teardown(&dir, &mut state).is_err());
        assert_eq!(state, json!({"database": "app_bay_demo"}));

        std::fs::remove_file(dir.join("fail")).unwrap();
        mysql.teardown(&dir, &mut state).unwrap();
        assert_eq!(state, Value::Null);
        assert!(docker.calls()[1].contains("DROP DATABASE IF EXISTS \\`app_bay_demo\\`"));

        mysql.teardown(&dir, &mut state).unwrap();
        assert_eq!(docker.calls().len(), 2);
        std::fs::remove_dir_all(dir).ok();
    }
}
