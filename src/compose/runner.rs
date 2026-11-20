use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::exec;

/// Resolved configuration of the stack defined in `dir`, every profile included.
pub fn config(dir: &Path, project: &str) -> Result<Value> {
    let output = exec::output(
        dir,
        "docker",
        [
            "compose",
            "--profile",
            "*",
            "-p",
            project,
            "config",
            "--format",
            "json",
        ],
    )?;
    serde_json::from_str(&output).context("unreadable `docker compose config` output")
}

/// Compose project name of the main stack, as Compose resolves it: from `name:`, `COMPOSE_PROJECT_NAME` or
/// the directory.
pub fn main_project_name(root: &Path) -> Result<String> {
    let output = exec::output(root, "docker", ["compose", "config", "--format", "json"])?;
    let config: Value =
        serde_json::from_str(&output).context("unreadable `docker compose config` output")?;
    config
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .context("the main Compose configuration has no project name")
}

/// Runs a command in a running service of the main stack.
pub fn main_exec(root: &Path, service: &str, args: &[&str]) -> Result<()> {
    let mut command = vec!["compose", "exec", "-T", service];
    command.extend(args);
    exec::run(root, "docker", command)
}

/// Recreates a service of the main stack, so that it reloads its env files.
pub fn main_recreate(root: &Path, service: &str) -> Result<()> {
    exec::run(
        root,
        "docker",
        ["compose", "up", "-d", "--no-deps", service],
    )
}

/// A bay as a Compose project.
pub struct Project {
    pub name: String,
    pub file: PathBuf,
    pub dir: PathBuf,
}

impl Project {
    fn args(&self, extra: &[&str]) -> Vec<OsString> {
        let mut args: Vec<OsString> = vec![
            "compose".into(),
            "-p".into(),
            self.name.clone().into(),
            "-f".into(),
            self.file.clone().into(),
        ];
        args.extend(extra.iter().map(OsString::from));
        args
    }

    pub fn up(&self, build: bool) -> Result<()> {
        let mut extra = vec!["up", "-d", "--remove-orphans"];
        if build {
            extra.push("--build");
        }
        exec::run(&self.dir, "docker", self.args(&extra))
    }

    pub fn down(&self, volumes: bool) -> Result<()> {
        let mut extra = vec!["down", "--remove-orphans"];
        if volumes {
            extra.push("--volumes");
        }
        exec::run(&self.dir, "docker", self.args(&extra))
    }

    pub fn run(&self, service: &str, command: &[String]) -> Result<()> {
        let mut extra = vec!["run", "--rm", "--no-deps", service];
        extra.extend(command.iter().map(String::as_str));
        exec::run(&self.dir, "docker", self.args(&extra))
    }
}

/// Containers of a Compose project, running or not, whatever its compose file.
pub fn containers(project: &str) -> Result<Vec<Value>> {
    let output = exec::output(
        Path::new("."),
        "docker",
        ["compose", "-p", project, "ps", "-a", "--format", "json"],
    )?;
    let output = output.trim();
    if output.starts_with('[') {
        return Ok(serde_json::from_str(output)?);
    }
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| Ok(serde_json::from_str(line)?))
        .collect()
}
