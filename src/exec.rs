use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

fn command<I, S>(dir: &Path, program: &str, args: I) -> (Command, String)
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
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
