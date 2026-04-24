//! Background terraform subprocess runner.
//!
//! The UI event loop polls [`RunnerEvent`]s from an mpsc channel. A runner
//! thread spawns `terraform <user args>`, streams combined stdout/stderr
//! line-by-line, and reports exit status. After a successful `terraform plan
//! -out=...`, the runner automatically runs `terraform show -json` on the
//! binary plan file so the UI can reparse without the user asking.

use anyhow::Result;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

const PLAN_BIN_FILE: &str = ".pliny.tfplan";

/// Current + available workspaces, as reported by `terraform workspace`.
/// Populated synchronously at startup and after workspace-changing commands.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceState {
    pub current: Option<String>,
    pub all: Vec<String>,
}

/// Blocking sync call. Fast when a terraform project is initialized
/// (reads `.terraform/environment` + `.terraform/environments`), otherwise
/// returns an empty state quickly.
pub fn read_workspaces() -> WorkspaceState {
    let current = Command::new("terraform")
        .args(["workspace", "show"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
        .filter(|s| !s.is_empty());

    let all = Command::new("terraform")
        .args(["workspace", "list"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                let text = String::from_utf8_lossy(&o.stdout).to_string();
                Some(
                    text.lines()
                        .map(|l| l.trim_start_matches('*').trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect(),
                )
            } else {
                None
            }
        })
        .unwrap_or_default();

    WorkspaceState { current, all }
}

/// Events emitted by the runner thread into the UI event loop.
#[derive(Debug, Clone)]
pub enum RunnerEvent {
    /// A single line of output (stderr or stdout) from the subprocess.
    Line(String),
    /// Subprocess exited. `json_plan` is populated only when the user's
    /// command was `plan` (or `plan ...`) and the auto-follow-up
    /// `terraform show -json` succeeded.
    Done {
        exit_code: Option<i32>,
        json_plan: Option<String>,
        /// Command the user typed (without the `terraform` prefix).
        original_cmd: String,
    },
}

/// Handle that owns the channel receiver.
pub struct RunnerHandle {
    pub rx: Receiver<RunnerEvent>,
}

impl RunnerHandle {
    /// Non-blocking drain of all pending events.
    pub fn drain(&self) -> Vec<RunnerEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = self.rx.try_recv() {
            out.push(ev);
        }
        out
    }
}

/// Spawn a terraform command in a background thread. The command is whatever
/// the user typed at the `:` prompt; pliny always prepends `terraform`.
///
/// For `plan` (with any args), pliny injects `-out=.pliny.tfplan` unless the
/// user already passed `-out=`, so that we can run `terraform show -json`
/// afterwards and populate the tree.
pub fn spawn(user_cmd: &str) -> RunnerHandle {
    let (tx, rx) = channel::<RunnerEvent>();
    let user_cmd = user_cmd.to_string();
    thread::spawn(move || {
        let _ = run_inner(&user_cmd, &tx);
    });
    RunnerHandle { rx }
}

fn run_inner(user_cmd: &str, tx: &Sender<RunnerEvent>) -> Result<()> {
    let tokens: Vec<String> = shell_split(user_cmd);
    if tokens.is_empty() {
        let _ = tx.send(RunnerEvent::Done {
            exit_code: Some(1),
            json_plan: None,
            original_cmd: user_cmd.to_string(),
        });
        return Ok(());
    }

    let mut args = tokens.clone();
    let mut inject_out = false;
    if args[0] == "plan" && !args.iter().any(|a| a.starts_with("-out=") || a == "-out") {
        args.push(format!("-out={PLAN_BIN_FILE}"));
        inject_out = true;
    }

    let _ = tx.send(RunnerEvent::Line(format!("$ terraform {}", args.join(" "))));

    let exit_code = stream_subprocess("terraform", &args, tx)?;

    let json_plan = if exit_code == Some(0) && args[0] == "plan" && inject_out {
        match read_json_plan(tx) {
            Ok(s) => Some(s),
            Err(e) => {
                let _ = tx.send(RunnerEvent::Line(format!(
                    "pliny: failed to parse plan: {e}"
                )));
                None
            }
        }
    } else {
        None
    };

    let _ = tx.send(RunnerEvent::Done {
        exit_code,
        json_plan,
        original_cmd: user_cmd.to_string(),
    });
    Ok(())
}

/// Spawn a subprocess, stream stdout+stderr line-by-line into `tx`,
/// return exit code.
fn stream_subprocess(bin: &str, args: &[String], tx: &Sender<RunnerEvent>) -> Result<Option<i32>> {
    let mut child = match Command::new(bin)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(RunnerEvent::Line(format!("pliny: failed to spawn {bin}: {e}")));
            return Ok(Some(127));
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let tx_out = tx.clone();
    let stdout_thread = stdout.map(|s| {
        thread::spawn(move || {
            for line in BufReader::new(s).lines().map_while(Result::ok) {
                let _ = tx_out.send(RunnerEvent::Line(line));
            }
        })
    });
    let tx_err = tx.clone();
    let stderr_thread = stderr.map(|s| {
        thread::spawn(move || {
            for line in BufReader::new(s).lines().map_while(Result::ok) {
                let _ = tx_err.send(RunnerEvent::Line(line));
            }
        })
    });

    let status = child.wait()?;
    if let Some(t) = stdout_thread {
        let _ = t.join();
    }
    if let Some(t) = stderr_thread {
        let _ = t.join();
    }
    Ok(status.code())
}

/// After a successful `terraform plan -out=.pliny.tfplan`, capture the JSON
/// form so the UI can reparse.
fn read_json_plan(tx: &Sender<RunnerEvent>) -> Result<String> {
    let _ = tx.send(RunnerEvent::Line(format!(
        "$ terraform show -json {PLAN_BIN_FILE}"
    )));
    let out = Command::new("terraform")
        .args(["show", "-json", PLAN_BIN_FILE])
        .output()?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("terraform show -json failed: {stderr}");
    }
    Ok(String::from_utf8(out.stdout)?)
}

/// Minimal shell-style tokenizer: splits on whitespace, respects single
/// and double quotes. Good enough for `plan -target=module.foo.bar` and
/// `apply -var="foo=bar baz"`.
fn shell_split(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = input.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match (c, quote) {
            ('\'' | '"', None) => quote = Some(c),
            (c, Some(q)) if c == q => quote = None,
            ('\\', _) => {
                if let Some(next) = chars.next() {
                    cur.push(next);
                }
            }
            (c, None) if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            (c, _) => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_simple() {
        assert_eq!(shell_split("plan -refresh=false"), vec!["plan", "-refresh=false"]);
    }

    #[test]
    fn tokenize_quoted() {
        assert_eq!(
            shell_split(r#"apply -var="foo=bar baz""#),
            vec!["apply", "-var=foo=bar baz"]
        );
    }

    #[test]
    fn tokenize_empty() {
        assert!(shell_split("   ").is_empty());
    }
}
