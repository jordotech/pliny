//! Background terraform subprocess runner.
//!
//! The UI event loop polls [`RunnerEvent`]s from an mpsc channel. A runner
//! thread spawns `terraform <user args>`, streams combined stdout/stderr
//! line-by-line, and reports exit status. After a successful `terraform plan
//! -out=...`, the runner automatically runs `terraform show -json` on the
//! binary plan file so the UI can reparse without the user asking.
//!
//! Terraform's binary plan format requires a file path for both `-out=` and
//! `show -json` — there's no stdin/stdout pipe. To keep the user's project
//! directory clean, pliny writes the transient plan to the OS tempdir with
//! a PID-scoped name and deletes it via [`PlanFileGuard`] as soon as the
//! JSON has been captured (or the runner errors out).
//!
//! The runner also wires up the child's stdin so the UI can forward typed
//! lines (e.g. `yes` for `terraform apply`), and exposes [`RunnerHandle::interrupt`]
//! to deliver SIGINT so terraform can release its state lock cleanly.

use anyhow::Result;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread;
use std::time::Duration;

/// Build a unique temp path for this plan invocation. Lives in `$TMPDIR`
/// (or `/tmp`), never in the user's terraform project directory.
fn plan_tempfile() -> PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("pliny-{}-{}.tfplan", std::process::id(), ts))
}

/// Deletes the plan tempfile when dropped, even on panic / early return.
struct PlanFileGuard(PathBuf);

impl PlanFileGuard {
    fn new(p: PathBuf) -> Self {
        Self(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for PlanFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

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
    /// `:apply` phase 1 produced a plan. UI refreshes the tree + AI summary
    /// and prompts the user to confirm or cancel. No apply has run yet.
    ApplyPlanReady { json_plan: String },
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

/// Handle that owns the channel receiver and stdin forwarder.
pub struct RunnerHandle {
    pub rx: Receiver<RunnerEvent>,
    stdin_tx: Sender<String>,
    pid: Arc<AtomicI32>,
    /// `true` means proceed with phase-2 apply, `false` means cancel. Only
    /// meaningful while a phase-1 plan is awaiting confirmation.
    approve_tx: Sender<bool>,
    /// Cooperative flag the runner checks after phase 1. Set by
    /// [`RunnerHandle::interrupt`] to skip phase 2 entirely.
    cancelled: Arc<AtomicBool>,
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

    /// Forward a line of text to the child's stdin. Trailing newline is
    /// appended. Silently drops the send if the subprocess has exited or
    /// the stdin pipe is closed.
    pub fn send_line(&self, mut text: String) {
        if !text.ends_with('\n') {
            text.push('\n');
        }
        let _ = self.stdin_tx.send(text);
    }

    /// Deliver SIGINT to the running child. Lets terraform finish its
    /// cleanup (release the state lock, print partial output) instead
    /// of being killed hard with SIGKILL.
    pub fn interrupt(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        // Unblock phase-1 approval wait, if one is pending.
        let _ = self.approve_tx.send(false);
        let pid = self.pid.load(Ordering::SeqCst);
        if pid > 0 {
            // SAFETY: libc::kill is safe to call; invalid pids just return -1.
            unsafe {
                libc::kill(pid, libc::SIGINT);
            }
        }
    }

    /// Proceed with the phase-2 apply after reviewing the phase-1 plan.
    /// No-op if no apply is pending confirmation.
    pub fn approve_apply(&self) {
        let _ = self.approve_tx.send(true);
    }

    /// Cancel a pending phase-2 apply without sending a signal to any
    /// running child. Used when the user declines at the confirmation prompt.
    pub fn cancel_apply(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        let _ = self.approve_tx.send(false);
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
    let (stdin_tx, stdin_rx) = channel::<String>();
    let (approve_tx, approve_rx) = channel::<bool>();
    let pid = Arc::new(AtomicI32::new(0));
    let cancelled = Arc::new(AtomicBool::new(false));
    let pid_bg = pid.clone();
    let cancelled_bg = cancelled.clone();
    let user_cmd = user_cmd.to_string();
    thread::spawn(move || {
        let _ = run_inner(&user_cmd, &tx, stdin_rx, approve_rx, pid_bg, cancelled_bg);
    });
    RunnerHandle {
        rx,
        stdin_tx,
        pid,
        approve_tx,
        cancelled,
    }
}

fn run_inner(
    user_cmd: &str,
    tx: &Sender<RunnerEvent>,
    stdin_rx: Receiver<String>,
    approve_rx: Receiver<bool>,
    pid: Arc<AtomicI32>,
    cancelled: Arc<AtomicBool>,
) -> Result<()> {
    let tokens: Vec<String> = shell_split(user_cmd);
    if tokens.is_empty() {
        let _ = tx.send(RunnerEvent::Done {
            exit_code: Some(1),
            json_plan: None,
            original_cmd: user_cmd.to_string(),
        });
        return Ok(());
    }

    // `:apply` with no saved plan gets a two-phase flow so pliny can emit
    // an AI summary before terraform prompts for confirmation. Any user
    // that passes a plan file (or `-auto-approve`) bypasses this and hits
    // the single-phase path.
    let is_bare_apply = tokens[0] == "apply"
        && !tokens.iter().any(|a| {
            a == "-auto-approve"
                || a.starts_with("-auto-approve=")
                || (!a.starts_with('-') && a != "apply")
        });
    if is_bare_apply {
        return run_two_phase_apply(
            &tokens,
            user_cmd,
            tx,
            stdin_rx,
            approve_rx,
            pid,
            cancelled,
        );
    }

    run_single_phase(&tokens, user_cmd, tx, stdin_rx, pid)
}

fn run_single_phase(
    tokens: &[String],
    user_cmd: &str,
    tx: &Sender<RunnerEvent>,
    stdin_rx: Receiver<String>,
    pid: Arc<AtomicI32>,
) -> Result<()> {
    let mut args = tokens.to_vec();
    let mut plan_guard: Option<PlanFileGuard> = None;
    if args[0] == "plan" && !args.iter().any(|a| a.starts_with("-out=") || a == "-out") {
        let path = plan_tempfile();
        args.push(format!("-out={}", path.display()));
        plan_guard = Some(PlanFileGuard::new(path));
    }

    let _ = tx.send(RunnerEvent::Line(format!("$ terraform {}", args.join(" "))));

    let exit_code = stream_subprocess("terraform", &args, tx, stdin_rx, pid)?;

    let json_plan = if exit_code == Some(0)
        && args[0] == "plan"
        && let Some(guard) = plan_guard.as_ref()
    {
        match read_json_plan(tx, guard.path()) {
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
    drop(plan_guard);

    let _ = tx.send(RunnerEvent::Done {
        exit_code,
        json_plan,
        original_cmd: user_cmd.to_string(),
    });
    Ok(())
}

/// Two-phase `:apply` flow.
///
/// 1. Run `terraform plan -out=<tmp> <extra args>` silently-ish and capture
///    JSON. Emit [`RunnerEvent::ApplyPlanReady`] so the UI refreshes the
///    tree and requests an AI summary.
/// 2. Wait for the user to approve via [`RunnerHandle::approve_apply`] or
///    cancel via [`RunnerHandle::cancel_apply`].
/// 3. On approval, run `terraform apply -auto-approve <tmp>` against the
///    same saved plan — the plan is locked in, so `-auto-approve` here is
///    as safe as answering "yes" to terraform's own prompt.
fn run_two_phase_apply(
    tokens: &[String],
    user_cmd: &str,
    tx: &Sender<RunnerEvent>,
    stdin_rx: Receiver<String>,
    approve_rx: Receiver<bool>,
    pid: Arc<AtomicI32>,
    cancelled: Arc<AtomicBool>,
) -> Result<()> {
    // Build plan args: swap the leading `apply` for `plan`, keep any -var,
    // -target, -refresh, etc. the user passed.
    let extra: Vec<String> = tokens.iter().skip(1).cloned().collect();
    let plan_path = plan_tempfile();
    let plan_guard = PlanFileGuard::new(plan_path.clone());
    let mut plan_args: Vec<String> = vec!["plan".into()];
    plan_args.extend(extra.iter().cloned());
    plan_args.push(format!("-out={}", plan_path.display()));

    let _ = tx.send(RunnerEvent::Line(
        "pliny: running plan first to build AI summary…".into(),
    ));
    let _ = tx.send(RunnerEvent::Line(format!(
        "$ terraform {}",
        plan_args.join(" ")
    )));

    // Drain stdin_rx briefly so phase-1 terraform (normally non-interactive
    // for `plan`) doesn't hang on a pipe — but we also want to preserve
    // the receiver for phase 2. Solution: create a fresh forwarder channel
    // for phase 1 that we close immediately, and only use the real stdin_rx
    // during phase 2.
    let (_ph1_stdin_tx, ph1_stdin_rx) = channel::<String>();
    drop(_ph1_stdin_tx);

    let plan_exit = stream_subprocess("terraform", &plan_args, tx, ph1_stdin_rx, pid.clone())?;
    if plan_exit != Some(0) || cancelled.load(Ordering::SeqCst) {
        let _ = tx.send(RunnerEvent::Line(
            "pliny: plan failed or cancelled, skipping apply.".into(),
        ));
        drop(plan_guard);
        let _ = tx.send(RunnerEvent::Done {
            exit_code: plan_exit,
            json_plan: None,
            original_cmd: user_cmd.to_string(),
        });
        return Ok(());
    }

    let json_plan = match read_json_plan(tx, plan_guard.path()) {
        Ok(s) => s,
        Err(e) => {
            let _ = tx.send(RunnerEvent::Line(format!(
                "pliny: failed to parse plan: {e}"
            )));
            drop(plan_guard);
            let _ = tx.send(RunnerEvent::Done {
                exit_code: Some(1),
                json_plan: None,
                original_cmd: user_cmd.to_string(),
            });
            return Ok(());
        }
    };

    let _ = tx.send(RunnerEvent::ApplyPlanReady {
        json_plan: json_plan.clone(),
    });
    let _ = tx.send(RunnerEvent::Line(
        "pliny: review the plan + AI summary. Type 'y' <Enter> to apply, or 'n' <Enter> to cancel."
            .into(),
    ));

    // Wait for UI to emit approval. Poll in a loop so we can also honor
    // cancel-via-interrupt if the user hits Ctrl-C before deciding.
    let approved = loop {
        match approve_rx.recv_timeout(Duration::from_millis(250)) {
            Ok(v) => break v,
            Err(RecvTimeoutError::Timeout) => {
                if cancelled.load(Ordering::SeqCst) {
                    break false;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break false,
        }
    };
    if !approved {
        let _ = tx.send(RunnerEvent::Line("pliny: apply cancelled.".into()));
        drop(plan_guard);
        let _ = tx.send(RunnerEvent::Done {
            exit_code: Some(0),
            json_plan: Some(json_plan),
            original_cmd: user_cmd.to_string(),
        });
        return Ok(());
    }

    // Phase 2: apply against the saved plan. Passing the plan file means
    // terraform doesn't re-prompt for "yes" and doesn't re-refresh state.
    let apply_args = vec!["apply".into(), plan_guard.path().display().to_string()];
    let _ = tx.send(RunnerEvent::Line(format!(
        "$ terraform {}",
        apply_args.join(" ")
    )));
    let apply_exit = stream_subprocess("terraform", &apply_args, tx, stdin_rx, pid)?;

    drop(plan_guard);
    let _ = tx.send(RunnerEvent::Done {
        exit_code: apply_exit,
        json_plan: Some(json_plan),
        original_cmd: user_cmd.to_string(),
    });
    Ok(())
}

/// Spawn a subprocess, stream stdout+stderr line-by-line into `tx`,
/// forward lines from `stdin_rx` into the child's stdin, publish the
/// child pid to `pid`, and return exit code.
fn stream_subprocess(
    bin: &str,
    args: &[String],
    tx: &Sender<RunnerEvent>,
    stdin_rx: Receiver<String>,
    pid: Arc<AtomicI32>,
) -> Result<Option<i32>> {
    let mut child = match Command::new(bin)
        .args(args)
        .stdin(Stdio::piped())
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

    pid.store(child.id() as i32, Ordering::SeqCst);

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdin = child.stdin.take();

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

    // Writer thread: reads lines from the UI, writes them to child stdin.
    // Exits when the channel closes (handle dropped) or the pipe breaks
    // (child exited).
    let stdin_thread = stdin.map(|mut s| {
        thread::spawn(move || {
            while let Ok(line) = stdin_rx.recv() {
                if s.write_all(line.as_bytes()).is_err() {
                    break;
                }
                let _ = s.flush();
            }
        })
    });

    let status = child.wait()?;
    pid.store(0, Ordering::SeqCst);

    if let Some(t) = stdout_thread {
        let _ = t.join();
    }
    if let Some(t) = stderr_thread {
        let _ = t.join();
    }
    // Writer thread exits naturally when the pipe closes; no need to join,
    // but drop the handle so we don't leak it.
    drop(stdin_thread);

    Ok(status.code())
}

/// After a successful `terraform plan -out=<path>`, capture the JSON form
/// so the UI can reparse. `path` is a tempfile — deleted by [`PlanFileGuard`]
/// after we return.
fn read_json_plan(tx: &Sender<RunnerEvent>, path: &Path) -> Result<String> {
    let shown = path.display().to_string();
    let _ = tx.send(RunnerEvent::Line(format!("$ terraform show -json {shown}")));
    let out = Command::new("terraform")
        .args(["show", "-json", &shown])
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
