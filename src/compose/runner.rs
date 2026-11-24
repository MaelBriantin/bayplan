use std::collections::BTreeMap;
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

/// Last lines of the logs of a service of a Compose project.
pub fn logs(project: &str, service: &str, lines: usize) -> Result<String> {
    exec::output(
        Path::new("."),
        "docker",
        [
            "compose",
            "-p",
            project,
            "logs",
            "--no-color",
            "--no-log-prefix",
            "--tail",
            &lines.to_string(),
            service,
        ],
    )
}

/// CPU and memory use of the running containers, by container name: `("0.52%", "120MiB")`.
pub fn stats() -> Result<BTreeMap<String, (String, String)>> {
    let output = exec::output(
        Path::new("."),
        "docker",
        ["stats", "--no-stream", "--format", "json"],
    )?;
    Ok(parse_stats(&output))
}

fn parse_stats(output: &str) -> BTreeMap<String, (String, String)> {
    output
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|stats| {
            let field = |name: &str| stats.get(name).and_then(Value::as_str).map(str::to_owned);
            let memory = field("MemUsage")?;
            let used = memory.split(" / ").next().unwrap_or_default().to_owned();
            Some((field("Name")?, (field("CPUPerc")?, used)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_cpu_and_memory_of_each_container() {
        let output = concat!(
            r#"{"Name":"web-demo-1","CPUPerc":"0.52%","MemUsage":"120.4MiB / 7.6GiB"}"#,
            "\n",
            "not json\n",
            r#"{"Name":"api-demo-1","CPUPerc":"12.00%","MemUsage":"1.2GiB / 7.6GiB"}"#,
            "\n",
        );
        let stats = parse_stats(output);
        assert_eq!(stats.len(), 2);
        assert_eq!(
            stats["web-demo-1"],
            ("0.52%".to_owned(), "120.4MiB".to_owned())
        );
        assert_eq!(
            stats["api-demo-1"],
            ("12.00%".to_owned(), "1.2GiB".to_owned())
        );
    }
}
