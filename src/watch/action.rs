//! A `bp` command run from the screen, its output read line by line as it comes.

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::sync::mpsc::{self, Receiver, Sender};

use regex::Regex;

/// Colors and cursor moves, which the screen does not interpret.
static ESCAPE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07]*\x07").unwrap());

#[derive(Debug, PartialEq, Eq)]
pub enum Status {
    Running,
    Done,
    Failed(String),
}

enum Message {
    Line(String),
    Finished(Status),
}

pub struct Action {
    /// The command as typed, e.g. `bp up demo`.
    pub title: String,
    pub lines: Vec<String>,
    pub status: Status,
    messages: Receiver<Message>,
}

impl Action {
    /// Starts `program` with `args` in `dir`, its output plain: no colors, Compose progress as lines.
    pub fn start(title: String, program: &Path, args: &[&str], dir: &Path) -> Self {
        let (sender, messages) = mpsc::channel();
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(dir)
            .env("NO_COLOR", "1")
            .env("COMPOSE_ANSI", "never")
            .env("COMPOSE_PROGRESS", "plain")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        std::thread::spawn(move || run(command, &sender));
        Self {
            title,
            lines: Vec::new(),
            status: Status::Running,
            messages,
        }
    }

    /// Takes what came since the last call.
    pub fn poll(&mut self) {
        for message in self.messages.try_iter() {
            match message {
                Message::Line(line) => self.lines.push(line),
                Message::Finished(status) => self.status = status,
            }
        }
    }
}

fn run(mut command: Command, sender: &Sender<Message>) {
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = sender.send(Message::Finished(Status::Failed(format!(
                "cannot start: {error}"
            ))));
            return;
        }
    };
    let stderr = child.stderr.take().map(|stderr| {
        let sender = sender.clone();
        std::thread::spawn(move || forward(stderr, &sender))
    });
    if let Some(stdout) = child.stdout.take() {
        forward(stdout, sender);
    }
    if let Some(stderr) = stderr {
        let _ = stderr.join();
    }
    let status = match child.wait() {
        Ok(status) if status.success() => Status::Done,
        Ok(status) => Status::Failed(status.to_string()),
        Err(error) => Status::Failed(error.to_string()),
    };
    let _ = sender.send(Message::Finished(status));
}

/// Sends each line of `output`; of a line rewritten in place with `\r`, only what it ends up showing.
fn forward(output: impl Read, sender: &Sender<Message>) {
    for chunk in BufReader::new(output).split(b'\n') {
        let Ok(chunk) = chunk else { return };
        let text = String::from_utf8_lossy(&chunk);
        let shown = text
            .split('\r')
            .rfind(|part| !part.trim().is_empty())
            .unwrap_or_default();
        if sender
            .send(Message::Line(ESCAPE.replace_all(shown, "").into_owned()))
            .is_err()
        {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    fn finish(action: &mut Action) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while action.status == Status::Running {
            assert!(Instant::now() < deadline, "the command did not finish");
            std::thread::sleep(Duration::from_millis(10));
            action.poll();
        }
    }

    #[test]
    fn reads_the_output_as_plain_lines_and_tells_a_failure() {
        let dir = std::env::temp_dir();
        let script = r"printf 'one\n\033[32mtwo\033[0m\n'; printf 'pulling 10%%\rpulling 100%%\n' >&2; exit 3";
        let mut action = Action::start(
            "bp up demo".to_owned(),
            Path::new("sh"),
            &["-c", script],
            &dir,
        );
        finish(&mut action);
        let mut lines = action.lines.clone();
        lines.sort();
        assert_eq!(lines, ["one", "pulling 100%", "two"]);
        assert_eq!(action.status, Status::Failed("exit status: 3".to_owned()));
    }

    #[test]
    fn hands_its_settings_to_the_command() {
        let mut action = Action::start(
            "bp up demo".to_owned(),
            Path::new("sh"),
            &["-c", "echo $NO_COLOR $COMPOSE_PROGRESS; pwd"],
            Path::new("/"),
        );
        finish(&mut action);
        assert_eq!(action.lines, ["1 plain", "/"]);
        assert_eq!(action.status, Status::Done);
    }
}
