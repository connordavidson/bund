use crate::{check_prepared, definition, paths, prepared, ARTIFACT_OUTPUT_LIMIT, MAX_OUTPUT_LIMIT};
use async_trait::async_trait;
use bund_core::{
    CommandExecutor, CommandSpec, Effect, Error, PreparedTool, Result, Tool, ToolContext,
    ToolDefinition, ToolEvent, ToolOutput,
};
use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use serde::Deserialize;
use serde_json::{json, Value};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Runs approved, noninteractive host shell commands. This is not a sandbox.
#[derive(Default)]
pub struct HostCommandExecutor;
pub(crate) struct RunCommand;

struct ProcessGroup {
    pid: Pid,
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        let _ = killpg(self.pid, Signal::SIGKILL);
    }
}

async fn stop_group(child: &mut Child, group: &ProcessGroup, reaped: bool) {
    if killpg(group.pid, Signal::SIGTERM).is_ok() {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let _ = killpg(group.pid, Signal::SIGKILL);
    }
    if !reaped {
        let _ = child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    }
}

struct Capture {
    artifact: File,
    path: PathBuf,
    artifact_bytes: usize,
    total_bytes: usize,
    model_bytes: Vec<u8>,
    model_limit: usize,
    streamed_bytes: usize,
    dropped_events: bool,
}

impl Capture {
    fn new(spec: &CommandSpec) -> Result<Self> {
        std::fs::create_dir_all(&spec.artifact_dir)?;
        if std::fs::symlink_metadata(&spec.artifact_dir)?
            .file_type()
            .is_symlink()
        {
            return Err(Error::Denied(
                "artifact directory cannot be a symlink".into(),
            ));
        }
        std::fs::set_permissions(&spec.artifact_dir, std::fs::Permissions::from_mode(0o700))?;
        let path = spec
            .artifact_dir
            .join(format!("command-{}.log", uuid::Uuid::new_v4()));
        let artifact = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&path)?;
        Ok(Self {
            artifact,
            path,
            artifact_bytes: 0,
            total_bytes: 0,
            model_bytes: vec![],
            model_limit: spec.output_limit.clamp(1, MAX_OUTPUT_LIMIT),
            streamed_bytes: 0,
            dropped_events: false,
        })
    }
    fn push(&mut self, bytes: &[u8], events: &mpsc::Sender<ToolEvent>) -> Result<()> {
        self.total_bytes = self.total_bytes.saturating_add(bytes.len());
        let artifact_end = bytes
            .len()
            .min(ARTIFACT_OUTPUT_LIMIT.saturating_sub(self.artifact_bytes));
        self.artifact.write_all(&bytes[..artifact_end])?;
        self.artifact_bytes += artifact_end;
        let model_end = bytes
            .len()
            .min(self.model_limit.saturating_sub(self.model_bytes.len()));
        self.model_bytes.extend_from_slice(&bytes[..model_end]);
        let event_end = bytes
            .len()
            .min(self.model_limit.saturating_sub(self.streamed_bytes));
        if event_end != 0 {
            let mut text = String::from_utf8_lossy(&bytes[..event_end]).into_owned();
            let remaining = self.model_limit.saturating_sub(self.streamed_bytes);
            if text.len() > remaining {
                let mut end = remaining;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
            }
            self.streamed_bytes += text.len();
            if events.try_send(ToolEvent::Output(text)).is_err() {
                self.dropped_events = true;
            }
        }
        Ok(())
    }
    fn finish(
        mut self,
        status: &str,
        exit_code: Option<i32>,
        is_error: bool,
    ) -> Result<ToolOutput> {
        self.artifact.sync_all()?;
        let note = format!(
            "\n[{}; exit code: {}; output: {}; artifact: {}{}{}]",
            status,
            exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "none".into()),
            self.total_bytes,
            self.path.display(),
            if self.total_bytes > ARTIFACT_OUTPUT_LIMIT {
                "; artifact capped at 8 MiB"
            } else {
                ""
            },
            if self.dropped_events {
                "; some live output omitted"
            } else {
                ""
            }
        );
        let reserve = note.len().min(self.model_limit);
        self.model_bytes
            .truncate(self.model_limit.saturating_sub(reserve));
        let mut content = String::from_utf8_lossy(&self.model_bytes).into_owned();
        // Invalid UTF-8 expands under lossy decoding; enforce a byte bound again.
        let max_text = self.model_limit.saturating_sub(reserve);
        if content.len() > max_text {
            let mut end = max_text;
            while !content.is_char_boundary(end) {
                end -= 1;
            }
            content.truncate(end);
        }
        if self.total_bytes > self.model_bytes.len() && self.model_limit > note.len() + 20 {
            let marker = "\n[output truncated]";
            let mut end = content.len().min(max_text.saturating_sub(marker.len()));
            while !content.is_char_boundary(end) {
                end -= 1;
            }
            content.truncate(end);
            content.push_str(marker);
        }
        if note.len() <= self.model_limit {
            content.push_str(&note);
        }
        Ok(ToolOutput {
            content,
            is_error,
            exit_code,
            ..Default::default()
        })
    }
}

#[async_trait]
impl CommandExecutor for HostCommandExecutor {
    async fn execute(
        &self,
        spec: CommandSpec,
        events: mpsc::Sender<ToolEvent>,
        cancel: CancellationToken,
    ) -> Result<ToolOutput> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if spec.command.trim().is_empty()
            || spec.command.len() > 32 * 1024
            || spec.timeout_secs == 0
        {
            return Err(Error::Invalid("invalid command or timeout".into()));
        }
        let mut capture = Capture::new(&spec)?;
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(&spec.command)
            .current_dir(&spec.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .env_clear();
        // Only conventional build/runtime locations and locale are inherited.
        // API keys, access tokens, SSH_AUTH_SOCK, BASH_ENV and ENV are excluded.
        for name in [
            "PATH",
            "HOME",
            "TMPDIR",
            "USER",
            "LOGNAME",
            "LANG",
            "LC_ALL",
            "CARGO_HOME",
            "RUSTUP_HOME",
        ] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command.env("TERM", "dumb").env("NO_COLOR", "1");
        command.as_std_mut().process_group(0);
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let mut child = command.spawn()?;
        let group = ProcessGroup {
            pid: Pid::from_raw(
                child
                    .id()
                    .ok_or_else(|| Error::Invalid("missing child process ID".into()))?
                    as i32,
            ),
        };
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Invalid("missing stdout".into()))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| Error::Invalid("missing stderr".into()))?;
        let deadline = tokio::time::sleep(Duration::from_secs(spec.timeout_secs));
        tokio::pin!(deadline);
        let mut out_buf = [0u8; 8192];
        let mut err_buf = [0u8; 8192];
        let (mut out_open, mut err_open) = (true, true);
        let mut status = None;
        let mut timed_out = false;
        let mut was_cancelled = false;
        let mut read_error = None;
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => { was_cancelled = true; break; }
                _ = &mut deadline => { timed_out = true; break; }
                result = child.wait() => { status = Some(result?); break; }
                result = stdout.read(&mut out_buf), if out_open => {
                    match result { Ok(0) => out_open=false, Ok(n) => if let Err(e) = capture.push(&out_buf[..n], &events) { read_error=Some(e); break; }, Err(e) => {read_error=Some(e.into()); break;} }
                }
                result = stderr.read(&mut err_buf), if err_open => {
                    match result { Ok(0) => err_open=false, Ok(n) => if let Err(e) = capture.push(&err_buf[..n], &events) { read_error=Some(e); break; }, Err(e) => {read_error=Some(e.into()); break;} }
                }
            }
        }
        // Background descendants are terminated even if the parent exits first.
        stop_group(&mut child, &group, status.is_some()).await;
        let drain_deadline = tokio::time::sleep(Duration::from_secs(1));
        tokio::pin!(drain_deadline);
        while out_open || err_open {
            tokio::select! {
                _ = &mut drain_deadline => break,
                result = stdout.read(&mut out_buf), if out_open => {
                    match result { Ok(0) | Err(_) => out_open=false, Ok(n) => capture.push(&out_buf[..n], &events)? }
                }
                result = stderr.read(&mut err_buf), if err_open => {
                    match result { Ok(0) | Err(_) => err_open=false, Ok(n) => capture.push(&err_buf[..n], &events)? }
                }
            }
        }
        if let Some(error) = read_error {
            return Err(error);
        }
        if was_cancelled {
            let _ = capture.finish("cancelled", None, true)?;
            return Err(Error::Cancelled);
        }
        let code = status.as_ref().and_then(|s| s.code());
        capture.finish(
            if timed_out { "timed out" } else { "finished" },
            code,
            timed_out || !status.is_some_and(|s| s.success()),
        )
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunArgs {
    command: String,
    #[serde(default = "default_cwd")]
    cwd: String,
}
fn default_cwd() -> String {
    ".".into()
}

#[async_trait]
impl Tool for RunCommand {
    fn definition(&self) -> ToolDefinition {
        definition("run_command", "Run a noninteractive /bin/sh command on the host after approval. This is not sandboxed. cwd is workspace-relative; use for builds/tests and explicitly approved workspace operations. Stdin is closed; background processes are terminated.", Effect::Execute,
            json!({"command":{"type":"string"},"cwd":{"type":"string"}}), &["command"])
    }
    async fn prepare(&self, context: &ToolContext, arguments: Value) -> Result<PreparedTool> {
        let a: RunArgs = serde_json::from_value(arguments)?;
        if a.command.trim().is_empty() || a.command.len() > 32 * 1024 || a.command.contains('\0') {
            return Err(Error::Invalid(
                "command must be 1..32768 bytes without NUL".into(),
            ));
        }
        let relative = paths::relative(&a.cwd)?;
        let cwd = paths::checked_directory(context, &relative)?;
        let timeout = context.command_timeout_secs;
        prepared(json!({"command":a.command,"cwd":relative}),
            format!("Host command (not sandboxed)\nDirectory: {}\nTimeout: {} seconds\nEnvironment: build paths and locale only; no inherited credentials\n\n{}", cwd.display(), timeout, a.command),
            json!({"root":paths::root(context)?,"cwd":cwd,"timeout_secs":timeout,"output_limit":context.output_limit}))
    }
    async fn execute(
        &self,
        context: &ToolContext,
        p: PreparedTool,
        events: mpsc::Sender<ToolEvent>,
        cancel: CancellationToken,
    ) -> Result<ToolOutput> {
        check_prepared(&p, &self.prepare(context, p.arguments.clone()).await?)?;
        let a: RunArgs = serde_json::from_value(p.arguments)?;
        HostCommandExecutor
            .execute(
                CommandSpec {
                    command: a.command,
                    cwd: paths::checked_directory(context, &paths::relative(&a.cwd)?)?,
                    timeout_secs: context.command_timeout_secs,
                    output_limit: context.output_limit,
                    artifact_dir: context.artifact_dir.clone(),
                },
                events,
                cancel,
            )
            .await
    }
}
