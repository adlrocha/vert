//! Routing to a coding agent already installed on the machine, so its own
//! login, subscription, model and settings are used as they are.
//!
//! Every agent gets the prompt on standard input, runs in the change's
//! checkout and answers on standard output. Claude Code, Codex and pi are
//! limited to read-only tools to explain a change. To fix one, Claude Code
//! gets file tools confined to the checkout (`--restricted`, no shell, no
//! MCP servers) and Codex its workspace-write sandbox. pi can't be confined
//! to the checkout, so it fixes only when asked for by name, with its file
//! tools and nothing else. A custom command runs unconfined, with the
//! user's own permissions.

use std::collections::BTreeSet;

use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;

/// How often a running agent is checked for having finished.
const POLL: Duration = Duration::from_millis(100);
/// How long the output of an agent that exited is still awaited after the
/// deadline.
const GRACE: Duration = Duration::from_millis(500);
/// Most bytes kept from each of an agent's output streams; an answer past
/// this is not one vert can use.
const MAX_OUTPUT: usize = 16 << 20;
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

/// What an agent may do in the checkout it runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Read files only: explanations.
    ReadOnly,
    /// Read and edit files: fixes.
    Edit,
}

/// A coding agent vert can ask. The tools each gets follow the [`Access`]
/// it runs with (see [`Agent::command`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Agent {
    /// Claude Code: Read, Grep and Glob to explain; with Edit and Write,
    /// confined to the checkout, to fix.
    Claude,
    /// Codex: its read-only sandbox to explain, its workspace-write one to fix.
    Codex,
    /// pi: read, grep, find and ls to explain; with edit and write to fix.
    /// It can't be confined to a checkout, so it fixes only when chosen by
    /// name (see [`select_editor`]).
    Pi,
    /// A shell command that reads the prompt on stdin and answers on stdout.
    Custom(String),
}

/// pi's arguments to explain. No extensions, MCP servers, skills or context
/// files: only the file tools listed, none of which runs code.
const PI_READ_ONLY: [&str; 8] = [
    "-p",
    "--no-session",
    "--no-extensions",
    "--no-mcp",
    "--no-skills",
    "--no-context-files",
    "--tools",
    "read,grep,find,ls",
];

/// pi's arguments to fix: as [`PI_READ_ONLY`], with edit and write. Its
/// edits are still not kept inside the checkout.
const PI_EDIT: [&str; 8] = [
    "-p",
    "--no-session",
    "--no-extensions",
    "--no-mcp",
    "--no-skills",
    "--no-context-files",
    "--tools",
    "read,grep,find,ls,edit,write",
];

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

    /// Whether the agent's edits are kept inside the checkout it runs in by
    /// the agent itself, not only by asking it to: what a fix needs, since
    /// the checkout under review may hold instructions aimed at the agent.
    ///
    /// ```
    /// use vert::explain::agent::Agent;
    /// assert!(Agent::Claude.confines_edits() && Agent::Codex.confines_edits());
    /// assert!(!Agent::Pi.confines_edits());
    /// ```
    #[must_use]
    pub const fn confines_edits(&self) -> bool {
        matches!(self, Self::Claude | Self::Codex)
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

    /// The command that runs the agent non-interactively with `access`
    /// (a custom command always runs unconfined), on `model` when given
    /// (else the agent's own default). A custom command gets the model in
    /// the `VERT_EXPLAIN_MODEL` environment variable.
    ///
    /// ```
    /// use vert::explain::agent::{Access, Agent};
    /// let cmd = Agent::Pi.command(Access::ReadOnly, Some("sonnet"));
    /// assert_eq!(cmd.get_program(), "pi");
    /// let args: Vec<_> = cmd.get_args().collect();
    /// assert!(args.contains(&"--no-session".as_ref()));
    /// assert!(args.windows(2).any(|w| w == ["--model", "sonnet"]));
    /// ```
    #[must_use]
    pub fn command(&self, access: Access, model: Option<&str>) -> Command {
        let (program, args): (&str, &[&str]) = match (self, access) {
            (Self::Claude, Access::ReadOnly) => (
                "claude",
                &[
                    "-p",
                    "--output-format",
                    "text",
                    "--no-session-persistence",
                    "--strict-mcp-config",
                    "--tools",
                    "Read,Grep,Glob",
                ],
            ),
            (Self::Claude, Access::Edit) => (
                "claude",
                &[
                    "-p",
                    "--output-format",
                    "text",
                    "--no-session-persistence",
                    // Confines the file tools to the checkout, drops every
                    // tool that runs code, and ignores the checkout's settings.
                    "--restricted",
                    "--strict-mcp-config",
                    "--tools",
                    "Read,Grep,Glob,Edit,Write",
                    "--permission-mode",
                    "acceptEdits",
                ],
            ),
            (Self::Codex, _) => (
                "codex",
                match access {
                    Access::ReadOnly => &[
                        "exec",
                        "--sandbox",
                        "read-only",
                        "--skip-git-repo-check",
                        "--ephemeral",
                        "--color",
                        "never",
                        "-",
                    ],
                    // workspace-write also opens /tmp and $TMPDIR unless
                    // told not to; only the checkout stays writable.
                    Access::Edit => &[
                        "exec",
                        "--sandbox",
                        "workspace-write",
                        "-c",
                        "sandbox_workspace_write.exclude_slash_tmp=true",
                        "-c",
                        "sandbox_workspace_write.exclude_tmpdir_env_var=true",
                        "--skip-git-repo-check",
                        "--ephemeral",
                        "--color",
                        "never",
                        "-",
                    ],
                },
            ),
            (Self::Pi, Access::ReadOnly) => ("pi", &PI_READ_ONLY),
            (Self::Pi, Access::Edit) => ("pi", &PI_EDIT),
            (Self::Custom(cmd), _) => {
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

/// Picks the agent to fix changes with, from `choice`, checked against the
/// directories of `path`. `auto` takes the first installed agent that
/// [`confines_edits`](Agent::confines_edits); pi, which doesn't, fixes only
/// when chosen by name.
///
/// # Errors
/// Explains, for the user, why no agent can fix.
pub fn select_editor(choice: Choice, path: Option<&OsStr>) -> Result<Agent, String> {
    if choice == Choice::Auto {
        return AUTO
            .into_iter()
            .filter(Agent::confines_edits)
            .find(|agent| path.is_some_and(|p| on_path(agent.name(), p)))
            .ok_or_else(|| {
                "no agent that can fix found: install Claude Code or Codex, or set VERT_EXPLAIN_AGENT=pi to fix without confinement"
                    .to_owned()
            });
    }
    select(choice, None, path)
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

/// The process groups of the agents running now, so they can all be stopped
/// when vert exits instead of working on unwatched.
#[derive(Debug, Clone, Default)]
pub struct Groups(Arc<Mutex<BTreeSet<u32>>>);

impl Groups {
    fn insert(&self, pid: u32) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(pid);
    }

    fn remove(&self, pid: u32) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&pid);
    }

    /// How many agents are running.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).len()
    }

    /// Whether no agent is running.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Stops every running agent and whatever it started.
    pub fn stop_all(&self) {
        let pids = std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner));
        for pid in pids {
            kill_group(pid);
        }
    }
}

/// Records a running agent's process group in [`Groups`] until dropped.
struct Tracked<'a> {
    groups: &'a Groups,
    pid: u32,
}

impl<'a> Tracked<'a> {
    fn new(groups: &'a Groups, pid: u32) -> Self {
        groups.insert(pid);
        Self { groups, pid }
    }
}

impl Drop for Tracked<'_> {
    fn drop(&mut self) {
        self.groups.remove(self.pid);
    }
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

/// How to run one agent: what it may do, on which model, where, and for how
/// long, tracked in `groups` while it runs.
#[derive(Debug, Clone, Copy)]
pub struct Run<'a> {
    /// What the agent may do.
    pub access: Access,
    /// The model, or `None` for the agent's default.
    pub model: Option<&'a str>,
    /// The checkout it runs in.
    pub dir: &'a Path,
    /// How long it may take.
    pub timeout: Duration,
    /// Where its process group is recorded while it runs.
    pub groups: &'a Groups,
}

/// Runs `agent` as `how` says, writing `prompt` to its standard input, and
/// returns what it printed. It is stopped after the timeout.
///
/// # Errors
/// Returns [`RunError`] when it cannot start, fails or runs out of time.
pub fn run(agent: &Agent, how: Run<'_>, prompt: &str) -> Result<String, RunError> {
    let Run {
        access,
        model,
        dir,
        timeout,
        groups,
    } = how;
    let name = agent.name().to_owned();
    let mut command = agent.command(access, model);
    // Its own process group, so a timeout stops whatever the agent started.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = command
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| RunError::Start {
            name: name.clone(),
            source,
        })?;
    let _tracked = Tracked::new(groups, child.id());
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
                kill_group(child.id());
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
    let collect = |rx: Option<Receiver<(String, bool)>>| -> Result<(String, bool), RunError> {
        let Some(rx) = rx else {
            return Ok((String::new(), false));
        };
        let wait = deadline
            .saturating_duration_since(Instant::now())
            .max(GRACE);
        match rx.recv_timeout(wait) {
            Ok(text) => Ok(text),
            Err(RecvTimeoutError::Disconnected) => Ok((String::new(), false)),
            Err(RecvTimeoutError::Timeout) => Err(timed_out()),
        }
    };
    // Whatever the agent left behind goes with it, so nothing keeps running
    // (and spending the user's subscription) after vert stopped listening.
    kill_group(child.id());
    let (out, cut) = collect(stdout)?;
    if cut {
        return Err(RunError::Failed {
            name,
            status: status.to_string(),
            output: format!("printed more than {} MiB", MAX_OUTPUT >> 20),
        });
    }
    if status.success() {
        return Ok(out);
    }
    let (err, _) = collect(stderr)?;
    let text = if err.trim().is_empty() { out } else { err };
    Err(RunError::Failed {
        name,
        status: status.to_string(),
        output: tail(&text),
    })
}

/// Reads a pipe to the end on its own thread, so neither pipe can fill up
/// and block the agent. The text arrives on the returned channel, with
/// whether it was cut at [`MAX_OUTPUT`] bytes (the rest is read and dropped).
fn drain(pipe: impl Read + Send + 'static) -> Receiver<(String, bool)> {
    drain_up_to(pipe, MAX_OUTPUT)
}

fn drain_up_to(mut pipe: impl Read + Send + 'static, max: usize) -> Receiver<(String, bool)> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let limit = u64::try_from(max).unwrap_or(u64::MAX);
        let _ = pipe.by_ref().take(limit).read_to_end(&mut bytes);
        let cut = std::io::copy(&mut pipe, &mut std::io::sink()).is_ok_and(|n| n > 0);
        let _ = tx.send((String::from_utf8_lossy(&bytes).into_owned(), cut));
    });
    rx
}

/// Stops every process in the group `pid` leads. Best effort: the group is
/// usually gone already.
fn kill_group(pid: u32) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-KILL", "--", &format!("-{pid}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(unix))]
    let _ = pid;
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
        args_with(agent, Access::ReadOnly, model)
    }

    fn args_with(agent: &Agent, access: Access, model: Option<&str>) -> Vec<String> {
        agent
            .command(access, model)
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
        assert!(args(&Agent::Claude).contains(&"--strict-mcp-config".to_owned()));
        let pi = args(&Agent::Pi);
        assert!(pi.windows(2).any(|w| w == ["--tools", "read,grep,find,ls"]));
        for off in [
            "--no-extensions",
            "--no-mcp",
            "--no-skills",
            "--no-context-files",
        ] {
            assert!(pi.contains(&off.to_owned()), "{off}");
        }
        let custom = Agent::Custom("my-llm --json".to_owned());
        assert_eq!(custom.command(Access::ReadOnly, None).get_program(), "sh");
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

    #[test]
    fn fixing_agents_edit_files_without_a_shell() {
        let claude = args_with(&Agent::Claude, Access::Edit, Some("opus"));
        assert!(claude.contains(&"Read,Grep,Glob,Edit,Write".to_owned()));
        assert!(
            claude.contains(&"--restricted".to_owned()),
            "file tools stay in the checkout"
        );
        assert!(
            claude.contains(&"--strict-mcp-config".to_owned()),
            "no MCP servers"
        );
        assert!(!args(&Agent::Claude).contains(&"--restricted".to_owned()));
        assert!(
            claude
                .windows(2)
                .any(|w| w == ["--permission-mode", "acceptEdits"])
        );
        assert!(claude.ends_with(&["--model".into(), "opus".into()]));
        assert!(!claude.iter().any(|a| a.contains("Bash")));
        let codex = args_with(&Agent::Codex, Access::Edit, Some("o3"));
        assert!(
            codex
                .windows(2)
                .any(|w| w == ["--sandbox", "workspace-write"])
        );
        for exclude in ["exclude_slash_tmp", "exclude_tmpdir_env_var"] {
            let setting = format!("sandbox_workspace_write.{exclude}=true");
            assert!(
                codex.windows(2).any(|w| w == ["-c", setting.as_str()]),
                "{exclude}"
            );
        }
        assert_eq!(codex[codex.len() - 3..], ["--model", "o3", "-"]);
        let pi = args_with(&Agent::Pi, Access::Edit, None);
        assert!(
            pi.windows(2)
                .any(|w| w == ["--tools", "read,grep,find,ls,edit,write"]),
            "file tools only, no bash"
        );
        assert_eq!(
            pi.iter().filter(|a| a.starts_with("--no-")).count(),
            5,
            "same switches off as explaining: {pi:?}"
        );
        let custom = Agent::Custom("x".to_owned());
        assert_eq!(args_with(&custom, Access::Edit, None), ["-c", "x"]);
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

    /// Runs `agent` read-only, checking it is forgotten once done.
    fn run_ro(
        agent: &Agent,
        model: Option<&str>,
        dir: &Path,
        prompt: &str,
        timeout: Duration,
    ) -> Result<String, RunError> {
        let groups = Groups::default();
        let how = Run {
            access: Access::ReadOnly,
            model,
            dir,
            timeout,
            groups: &groups,
        };
        let out = run(agent, how, prompt);
        assert!(groups.is_empty(), "the agent is forgotten once it is done");
        out
    }

    #[test]
    fn running_agents_can_all_be_stopped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let groups = Groups::default();
        let marker = dir.path().join("late");
        let worker = {
            let (groups, path) = (groups.clone(), dir.path().to_path_buf());
            thread::spawn(move || {
                let how = Run {
                    access: Access::Edit,
                    model: None,
                    dir: &path,
                    timeout: Duration::from_secs(20),
                    groups: &groups,
                };
                run(&sh("sleep 2; touch late"), how, "")
            })
        };
        let start = Instant::now();
        while groups.is_empty() && start.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(groups.len(), 1, "the agent is tracked while it runs");
        groups.stop_all();
        assert!(groups.is_empty());
        let stopped = worker.join().expect("joined");
        assert!(stopped.is_err(), "{stopped:?}");
        assert!(start.elapsed() < Duration::from_secs(2), "stopped at once");
        thread::sleep(Duration::from_millis(2500));
        assert!(!marker.exists(), "nothing it started went on");
    }

    fn sh(cmd: &str) -> Agent {
        Agent::Custom(cmd.to_owned())
    }

    #[test]
    fn runs_with_the_prompt_on_stdin_in_the_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("marker"), "here").expect("write");
        let out = run_ro(
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
        let failed = run_ro(
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
        let quiet = run_ro(&sh("echo only stdout; exit 1"), None, dir.path(), "", secs)
            .expect_err("fails")
            .to_string();
        assert!(
            quiet.ends_with("only stdout"),
            "stdout when stderr is empty: {quiet}"
        );
    }

    #[test]
    fn stops_what_the_agent_left_behind_and_slow_agents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let secs = Duration::from_secs(10);
        // A child left behind in the agent's group is stopped with it.
        let start = Instant::now();
        let left = run_ro(
            &sh("sleep 30 & echo partial"),
            None,
            dir.path(),
            "",
            Duration::from_secs(10),
        )
        .expect("answered");
        assert_eq!(left, "partial\n");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
        // One that left the group and holds stdout open runs into the timeout
        // (`setsid` is Linux-only).
        let has_setsid = std::env::var_os("PATH").is_some_and(|p| on_path("setsid", &p));
        let start = Instant::now();
        let escaped = run_ro(
            &sh(if has_setsid {
                "setsid sleep 30 & echo partial"
            } else {
                "sleep 30"
            }),
            None,
            dir.path(),
            "",
            Duration::from_millis(300),
        )
        .expect_err("stdout held open")
        .to_string();
        assert!(escaped.contains("didn't answer"), "{escaped}");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "the timeout holds"
        );
        // The leftover child was stopped with the agent.
        let marker = dir.path().join("still-running");
        let _ = run_ro(
            &sh("(sleep 1; touch still-running) & sleep 30"),
            None,
            dir.path(),
            "",
            Duration::from_millis(200),
        );
        thread::sleep(Duration::from_millis(1500));
        assert!(!marker.exists(), "the whole group was stopped");

        let slow = run_ro(
            &sh("sleep 5"),
            None,
            dir.path(),
            "",
            Duration::from_millis(200),
        )
        .expect_err("slow")
        .to_string();
        assert!(slow.contains("didn't answer within 0s"), "{slow}");

        let missing = run_ro(
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
            run_ro(&sh("echo ok"), None, dir.path(), &"x".repeat(1 << 20), secs).expect("ok"),
            "ok\n"
        );
    }

    #[test]
    fn output_past_the_cap_is_dropped_and_reported() {
        let (text, cut) = drain_up_to(&b"abcdef"[..], 4).recv().expect("drained");
        assert_eq!((text.as_str(), cut), ("abcd", true));
        let (text, cut) = drain_up_to(&b"abcd"[..], 4).recv().expect("drained");
        assert_eq!((text.as_str(), cut), ("abcd", false));
        let dir = tempfile::tempdir().expect("tempdir");
        let big = run_ro(
            &sh("head -c 17000000 /dev/zero"),
            None,
            dir.path(),
            "",
            Duration::from_secs(20),
        )
        .expect_err("too much")
        .to_string();
        assert!(big.ends_with("printed more than 16 MiB"), "{big}");
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
