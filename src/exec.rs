use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

fn command<I, S>(dir: &Path, program: &str, args: I) -> (Command, String)
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    #[cfg(test)]
    let mut command = Command::new(tests::stand_in(program));
    #[cfg(not(test))]
    let mut command = Command::new(program);
    command.current_dir(dir).args(args);
    let line = std::iter::once(program.to_owned())
        .chain(
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned()),
        )
        .collect::<Vec<_>>()
        .join(" ");
    (command, line)
}

/// Runs a command with its output streamed to the terminal, failing on a non-zero exit.
pub fn run<I, S>(dir: &Path, program: &str, args: I) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let (mut command, line) = command(dir, program, args);
    eprintln!("→ {line}");
    let status = command
        .status()
        .with_context(|| format!("cannot start `{program}`"))?;
    if !status.success() {
        bail!("`{line}` failed ({status})");
    }
    Ok(())
}

/// Runs a command and returns its standard output, failing on a non-zero exit.
pub fn output<I, S>(dir: &Path, program: &str, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let (mut command, line) = command(dir, program, args);
    let output = command
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("cannot start `{program}`"))?;
    if !output.status.success() {
        bail!(
            "`{line}` failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8(output.stdout)?)
}

#[cfg(test)]
pub mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::*;

    thread_local! {
        /// Programs run in place of others by the test of this thread.
        static STAND_INS: RefCell<BTreeMap<String, PathBuf>> = RefCell::default();
    }

    pub fn stand_in(program: &str) -> PathBuf {
        STAND_INS
            .with(|stand_ins| stand_ins.borrow().get(program).cloned())
            .unwrap_or_else(|| PathBuf::from(program))
    }

    /// A `docker` run in place of the real one by the test of this thread, until dropped. Each call is logged
    /// as a line of its arguments; it prints what `output` gave it, and fails when its arguments contain a
    /// text given to `fail_on`.
    pub struct FakeDocker {
        dir: PathBuf,
    }

    impl FakeDocker {
        pub fn install(dir: &Path) -> Self {
            std::fs::create_dir_all(dir).unwrap();
            let script = dir.join("docker");
            std::fs::write(
                &script,
                format!(
                    r#"#!/bin/sh
dir='{}'
echo "$*" >> "$dir/calls"
if [ -f "$dir/fail" ]; then
    while IFS= read -r text; do
        case "$*" in *"$text"*) echo "fake docker fails on: $text" >&2; exit 1 ;; esac
    done < "$dir/fail"
fi
if [ -f "$dir/output" ]; then cat "$dir/output"; fi
"#,
                    dir.display()
                ),
            )
            .unwrap();
            run(dir, "chmod", ["+x", "docker"]).unwrap();
            STAND_INS.with(|stand_ins| {
                stand_ins.borrow_mut().insert("docker".to_owned(), script);
            });
            Self {
                dir: dir.to_path_buf(),
            }
        }

        pub fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(self.dir.join("calls"))
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }

        pub fn fail_on(&self, text: &str) {
            let path = self.dir.join("fail");
            let mut texts = std::fs::read_to_string(&path).unwrap_or_default();
            texts.push_str(text);
            texts.push('\n');
            std::fs::write(path, texts).unwrap();
        }

        pub fn output(&self, text: &str) {
            std::fs::write(self.dir.join("output"), text).unwrap();
        }
    }

    impl Drop for FakeDocker {
        fn drop(&mut self) {
            STAND_INS.with(|stand_ins| stand_ins.borrow_mut().remove("docker"));
        }
    }

    #[test]
    fn runs_the_fake_docker_logging_its_calls() {
        let dir = std::env::temp_dir().join(format!("bayplan-fake-docker-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let docker = FakeDocker::install(&dir);
        docker.output("[]");
        docker.fail_on("down");
        assert_eq!(output(&dir, "docker", ["compose", "ps"]).unwrap(), "[]");
        assert!(run(&dir, "docker", ["compose", "down"]).is_err());
        assert_eq!(docker.calls(), ["compose ps", "compose down"]);
        drop(docker);
        assert_eq!(stand_in("docker"), PathBuf::from("docker"));
        std::fs::remove_dir_all(dir).ok();
    }
}
