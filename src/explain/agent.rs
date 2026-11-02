//! Routing to a coding agent already installed on the machine, so its own
//! login, subscription, model and settings are used as they are.
//!
//! Every agent gets the prompt on standard input, runs in the change's
//! checkout with only read-only tools, and answers on standard output.

use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;

/// How often a running agent is checked for having finished.
const POLL: Duration = Duration::from_millis(100);
/// How long the output of an agent that exited is still awaited after the
/// deadline.
const GRACE: Duration = Duration::from_millis(500);
/// Most characters of an agent's error output repeated to the user.
const ERROR_CHARS: usize = 400;

/// Which agent the user asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Choice {
    /// The first of Claude Code, Codex and pi found on `PATH`.
    Auto,
    /// Claude Code (`claude`).
    Claude,
    /// `OpenAI` Codex (`codex`).
    Codex,
    /// pi (`pi`).
    Pi,
}

/// A coding agent vert can ask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Agent {
    /// Claude Code, limited to its Read, Grep and Glob tools.
    Claude,
    /// Codex in its read-only sandbox.
    Codex,
    /// pi, limited to its read, grep, find and ls tools.
    Pi,
    /// A shell command that reads the prompt on stdin and answers on stdout.
    Custom(String),
}

/// The agents tried, in order, when the choice is [`Choice::Auto`].
const AUTO: [Agent; 3] = [Agent::Claude, Agent::Codex, Agent::Pi];

impl Agent {
    /// Name shown to the user.
    #[must_use]
    pub const fn name(&self) -> &str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Pi => "pi",
            Self::Custom(_) => "--explain-cmd",
        }
    }

    /// Identifies the agent in cached answers; stable across releases.
    ///
    /// ```
    /// use vert::explain::agent::Agent;
    /// assert_eq!(Agent::Pi.key(), "pi");
    /// assert_eq!(Agent::Custom("llm".into()).key(), "cmd:llm");
    /// ```
    #[must_use]
    pub fn key(&self) -> String {
        match self {
            Self::Custom(cmd) => format!("cmd:{cmd}"),
            agent => agent.name().to_owned(),
        }
    }

    /// The command that runs the agent non-interactively with read-only tools,
    /// on `model` when given (else the agent's own default). A custom command
    /// gets the model in the `VERT_EXPLAIN_MODEL` environment variable.
    ///
    /// ```
    /// use vert::explain::agent::Agent;
    /// let cmd = Agent::Pi.command(Some("sonnet"));
    /// assert_eq!(cmd.get_program(), "pi");
    /// let args: Vec<_> = cmd.get_args().collect();
    /// assert!(args.contains(&"--no-session".as_ref()));
    /// assert!(args.windows(2).any(|w| w == ["--model", "sonnet"]));
    /// ```
    #[must_use]
    pub fn command(&self, model: Option<&str>) -> Command {
        let (program, args): (&str, &[&str]) = match self {
            Self::Claude => (
                "claude",
                &[
                    "-p",
                    "--output-format",
                    "text",
                    "--no-session-persistence",
                    "--tools",
                    "Read,Grep,Glob",
                ],
            ),
            Self::Codex => (
                "codex",
                &[
                    "exec",
                    "--sandbox",
                    "read-only",
                    "--skip-git-repo-check",
                    "--ephemeral",
                    "--color",
                    "never",
                    "-",
                ],
            ),
            Self::Pi => (
                "pi",
                &["-p", "--no-session", "--tools", "read,grep,find,ls"],
            ),
            Self::Custom(cmd) => {
                let mut c = Command::new("sh");
                c.arg("-c").arg(cmd);
                if let Some(model) = model {
                    c.env("VERT_EXPLAIN_MODEL", model);
                }
                return c;
            }
        };
        let mut c = Command::new(program);
        // Codex reads the prompt from `-`, which must stay last.
        let (head, last) = match self {
            Self::Codex => args.split_at(args.len() - 1),
            _ => (args, &[][..]),
        };
        c.args(head);
        if let Some(model) = model {
            c.args(["--model", model]);
        }
        c.args(last);
        c
    }
}

/// Picks the agent to ask: `custom` when given, else `choice`, checked
/// against the directories of `path` (the `PATH` variable).
///
/// # Errors
/// Explains, for the user, why no agent can be used.
pub fn select(choice: Choice, custom: Option<&str>, path: Option<&OsStr>) -> Result<Agent, String> {
    if let Some(cmd) = custom.map(str::trim).filter(|c| !c.is_empty()) {
        return Ok(Agent::Custom(cmd.to_owned()));
    }
    let found = |agent: &Agent| path.is_some_and(|p| on_path(agent.name(), p));
    match choice {
        Choice::Auto => AUTO.into_iter().find(found).ok_or_else(|| {
            "no coding agent found: install Claude Code, Codex or pi, or pass --explain-cmd"
                .to_owned()
        }),
        Choice::Claude | Choice::Codex | Choice::Pi => {
            let agent = match choice {
                Choice::Claude => Agent::Claude,
                Choice::Codex => Agent::Codex,
                _ => Agent::Pi,
            };
            if found(&agent) {
                Ok(agent)
            } else {
                Err(format!("{} is not on PATH (--explain-agent)", agent.name()))
            }
        }
    }
}

/// Whether an executable named `program` is in one of the `path` directories.
fn on_path(program: &str, path: &OsStr) -> bool {
    std::env::split_paths(path).any(|dir| is_executable(&dir.join(program)))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Why asking an agent failed.
#[derive(Debug, Error)]
pub enum RunError {
    /// The process could not be started.
    #[error("couldn't start {name}: {source}")]
    Start {
        /// Agent name.
        name: String,
        /// Cause.
        source: std::io::Error,
    },
    /// The agent did not answer in time and was stopped.
    #[error("{name} didn't answer within {secs}s (--explain-timeout-secs)")]
    TimedOut {
        /// Agent name.
        name: String,
        /// The time allowed.
        secs: u64,
    },
    /// The agent exited unsuccessfully.
    #[error("{name} failed ({status}): {output}")]
    Failed {
        /// Agent name.
        name: String,
        /// Exit status.
        status: String,
        /// The end of what it printed.
        output: String,
    },
    /// Waiting for the process failed.
    #[error("lost track of {name}: {source}")]
    Wait {
        /// Agent name.
        name: String,
        /// Cause.
        source: std::io::Error,
    },
}

/// Runs `agent` on `model` in `dir`, writing `prompt` to its standard input, and
/// returns what it printed. It is stopped after `timeout`.
///
/// # Errors
/// Returns [`RunError`] when it cannot start, fails or runs out of time.
pub fn run(
    agent: &Agent,
    model: Option<&str>,
    dir: &Path,
    prompt: &str,
    timeout: Duration,
) -> Result<String, RunError> {
    let name = agent.name().to_owned();
    let mut child = agent
        .command(model)
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| RunError::Start {
            name: name.clone(),
            source,
        })?;
    if let Some(mut stdin) = child.stdin.take() {
        let prompt = prompt.to_owned();
        // An agent that exits without reading everything closes the pipe;
        // its exit status tells the story, so a write error is ignored.
        thread::spawn(move || stdin.write_all(prompt.as_bytes()));
    }
    let stdout = child.stdout.take().map(drain);
    let stderr = child.stderr.take().map(drain);
    let deadline = Instant::now() + timeout;
    let timed_out = || RunError::TimedOut {
        name: name.clone(),
        secs: timeout.as_secs(),
    };
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(timed_out());
            }
            Ok(None) => thread::sleep(POLL),
            Err(source) => {
                return Err(RunError::Wait {
                    name: name.clone(),
                    source,
                });
            }
        }
    };
    // A child the agent left behind can hold the pipes open after it exits,
    // so reading them is bounded by the same deadline (plus a moment for the
    // last bytes of an agent that finished just in time).
    let collect = |rx: Option<Receiver<String>>| -> Result<String, RunError> {
        let Some(rx) = rx else {
            return Ok(String::new());
        };
        let wait = deadline
            .saturating_duration_since(Instant::now())
            .max(GRACE);
        match rx.recv_timeout(wait) {
            Ok(text) => Ok(text),
            Err(RecvTimeoutError::Disconnected) => Ok(String::new()),
            Err(RecvTimeoutError::Timeout) => Err(timed_out()),
        }
    };
    let out = collect(stdout)?;
    if status.success() {
        return Ok(out);
    }
    let err = collect(stderr)?;
    let text = if err.trim().is_empty() { out } else { err };
    Err(RunError::Failed {
        name,
        status: status.to_string(),
        output: tail(&text),
    })
}

/// Reads a pipe to the end on its own thread, so neither pipe can fill up
/// and block the agent; the text arrives on the returned channel.
fn drain(mut pipe: impl Read + Send + 'static) -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = pipe.read_to_end(&mut bytes);
        let _ = tx.send(String::from_utf8_lossy(&bytes).into_owned());
    });
    rx
}

/// The last lines of `text`, up to [`ERROR_CHARS`] characters, on one line.
fn tail(text: &str) -> String {
    let joined = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
    if joined.is_empty() {
        return "no output".to_owned();
    }
    let count = joined.chars().count();
    if count <= ERROR_CHARS {
        return joined;
    }
    let kept: String = joined.chars().skip(count - ERROR_CHARS).collect();
    format!("…{kept}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(agent: &Agent) -> Vec<String> {
        args_on(agent, None)
    }

    fn args_on(agent: &Agent, model: Option<&str>) -> Vec<String> {
        agent
            .command(model)
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn agents_run_read_only_and_non_interactive() {
        assert!(args(&Agent::Claude).contains(&"Read,Grep,Glob".to_owned()));
        assert!(args(&Agent::Claude).contains(&"-p".to_owned()));
        let codex = args(&Agent::Codex);
        assert!(codex.windows(2).any(|w| w == ["--sandbox", "read-only"]));
        assert_eq!(codex.last().map(String::as_str), Some("-"), "stdin prompt");
        assert!(args(&Agent::Pi).contains(&"read,grep,find,ls".to_owned()));
        let custom = Agent::Custom("my-llm --json".to_owned());
        assert_eq!(custom.command(None).get_program(), "sh");
        assert_eq!(args(&custom), ["-c", "my-llm --json"]);
        assert!(
            args_on(&Agent::Claude, Some("opus")).ends_with(&["--model".into(), "opus".into()])
        );
        assert!(args_on(&Agent::Pi, Some("x/y")).ends_with(&["--model".into(), "x/y".into()]));
        let codex = args_on(&Agent::Codex, Some("o3"));
        assert_eq!(
            codex[codex.len() - 3..],
            ["--model", "o3", "-"],
            "prompt stays last"
        );
        assert_eq!(
            args_on(&custom, Some("m")),
            ["-c", "my-llm --json"],
            "model is in env"
        );
        assert_eq!(
            [Agent::Claude, Agent::Codex, Agent::Pi, custom].map(|a| a.name().to_owned()),
            ["claude", "codex", "pi", "--explain-cmd"]
        );
    }

    /// A directory holding fake executables named `names`.
    fn bin(names: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in names {
            let path = dir.path().join(name);
            std::fs::write(&path, "#!/bin/sh\n").expect("write");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod");
            }
        }
        dir
    }

    #[test]
    fn selects_the_first_installed_agent() {
        let both = bin(&["pi", "codex"]);
        let path = both.path().as_os_str();
        assert_eq!(select(Choice::Auto, None, Some(path)), Ok(Agent::Codex));
        assert_eq!(select(Choice::Pi, None, Some(path)), Ok(Agent::Pi));
        let missing = select(Choice::Claude, None, Some(path)).expect_err("not installed");
        assert!(missing.contains("claude is not on PATH"), "{missing}");
        let none = select(Choice::Auto, None, None).expect_err("no PATH");
        assert!(none.contains("--explain-cmd"), "{none}");
        assert_eq!(
            select(Choice::Auto, Some("  llm  "), None),
            Ok(Agent::Custom("llm".to_owned())),
            "a custom command wins"
        );
        assert!(
            select(Choice::Codex, Some(" "), Some(path)).is_ok(),
            "a blank custom command is ignored"
        );
    }

    #[cfg(unix)]
    #[test]
    fn plain_files_are_not_agents() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("claude"), "").expect("write");
        std::fs::create_dir(dir.path().join("pi")).expect("dir");
        assert!(select(Choice::Auto, None, Some(dir.path().as_os_str())).is_err());
    }

    fn sh(cmd: &str) -> Agent {
        Agent::Custom(cmd.to_owned())
    }

    #[test]
    fn runs_with_the_prompt_on_stdin_in_the_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("marker"), "here").expect("write");
        let out = run(
            &sh("cat marker; echo; tr a-z A-Z; printf %s \"$VERT_EXPLAIN_MODEL\""),
            Some("m1"),
            dir.path(),
            "hello",
            Duration::from_secs(10),
        )
        .expect("ran");
        assert_eq!(out, "here\nHELLOm1");
    }

    #[test]
    fn reports_failures_timeouts_and_missing_programs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let secs = Duration::from_secs(10);
        let failed = run(
            &sh("echo out; echo bad >&2; exit 3"),
            None,
            dir.path(),
            "",
            secs,
        )
        .expect_err("fails");
        let text = failed.to_string();
        assert!(
            text.contains("exit status: 3") && text.ends_with(": bad"),
            "{text}"
        );
        let quiet = run(&sh("echo only stdout; exit 1"), None, dir.path(), "", secs)
            .expect_err("fails")
            .to_string();
        assert!(
            quiet.ends_with("only stdout"),
            "stdout when stderr is empty: {quiet}"
        );

        let start = Instant::now();
        let left = run(
            &sh("sleep 30 & echo partial"),
            None,
            dir.path(),
            "",
            Duration::from_millis(300),
        )
        .expect_err("a leftover child holds stdout open")
        .to_string();
        assert!(left.contains("didn't answer"), "{left}");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "the timeout holds"
        );

        let slow = run(
            &sh("sleep 5"),
            None,
            dir.path(),
            "",
            Duration::from_millis(200),
        )
        .expect_err("slow")
        .to_string();
        assert!(slow.contains("didn't answer within 0s"), "{slow}");

        let missing = run(
            &sh("x"),
            None,
            &dir.path().join("gone"),
            "",
            Duration::from_secs(1),
        )
        .expect_err("no such dir");
        assert!(
            missing.to_string().starts_with("couldn't start"),
            "{missing}"
        );

        // An agent that ignores its input still answers.
        assert_eq!(
            run(&sh("echo ok"), None, dir.path(), &"x".repeat(1 << 20), secs).expect("ok"),
            "ok\n"
        );
    }

    #[test]
    fn error_output_is_shortened() {
        assert_eq!(tail(" \n\n"), "no output");
        assert_eq!(tail("a\n  b \n"), "a · b");
        let long = tail(&"y".repeat(1000));
        assert!(long.starts_with('…'));
        assert_eq!(long.chars().count(), ERROR_CHARS + 1);
    }
}
