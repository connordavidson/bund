use crate::{check_prepared, definition, paths, prepared, HostCommandExecutor};
use async_trait::async_trait;
use bund_core::{
    CommandExecutor, CommandSpec, Effect, Error, PreparedTool, Result, Tool, ToolContext,
    ToolDefinition, ToolEvent, ToolOutput,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{process::Stdio, time::Duration};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub(crate) struct GitStatus;
pub(crate) struct GitDiff;

// Fixed executable, no user/system configuration, no optional index writes or
// helper programs. Per-repository diff/textconv/fsmonitor helpers are disabled.
const GIT: &str = "GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_OPTIONAL_LOCKS=0 GIT_TERMINAL_PROMPT=0 /usr/bin/git --no-pager -c core.fsmonitor=false -c core.hooksPath=/dev/null -c core.pager=cat";

async fn run(
    context: &ToolContext,
    command: String,
    events: mpsc::Sender<ToolEvent>,
    cancel: CancellationToken,
) -> Result<ToolOutput> {
    // Even status/diff may invoke a configured clean/process filter while
    // converting worktree bytes. Such repositories require approved execution.
    // Querying config itself does not invoke filters; never display its values.
    let mut inspect = tokio::process::Command::new("/usr/bin/git");
    inspect
        .args([
            "--no-pager",
            "config",
            "--get-regexp",
            r"^filter\..*\.(clean|process)$",
        ])
        .current_dir(paths::root(context)?)
        .env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = inspect.spawn()?;
    let status = tokio::select! {
        _ = cancel.cancelled() => { let _ = child.kill().await; return Err(Error::Cancelled); }
        status = tokio::time::timeout(Duration::from_secs(5), child.wait()) => {
            match status {
                Ok(status) => status?,
                Err(_) => { let _ = child.kill().await; return Err(Error::Invalid("Git configuration inspection timed out".into())); }
            }
        }
    };
    match status.code() {
        Some(1) => {} // no configured filters
        Some(0) => {
            return Err(Error::Denied(
                "repository configures executable Git filters; use an approved host command".into(),
            ))
        }
        _ => {
            return Err(Error::Invalid(
                "Git configuration could not be inspected".into(),
            ))
        }
    }
    HostCommandExecutor
        .execute(
            CommandSpec {
                command,
                cwd: paths::root(context)?,
                timeout_secs: context.command_timeout_secs.min(30),
                output_limit: context.output_limit,
                artifact_dir: context.artifact_dir.clone(),
            },
            events,
            cancel,
        )
        .await
}

#[async_trait]
impl Tool for GitStatus {
    fn definition(&self) -> ToolDefinition {
        definition(
            "git_status",
            "Read Git status without hooks, fsmonitor helpers or index writes.",
            Effect::Read,
            json!({}),
            &[],
        )
    }
    async fn prepare(&self, context: &ToolContext, arguments: Value) -> Result<PreparedTool> {
        if arguments != json!({}) {
            return Err(Error::Invalid("git_status takes no arguments".into()));
        }
        prepared(
            json!({}),
            "Read Git status".into(),
            json!({"root":paths::root(context)?}),
        )
    }
    async fn execute(
        &self,
        context: &ToolContext,
        p: PreparedTool,
        events: mpsc::Sender<ToolEvent>,
        cancel: CancellationToken,
    ) -> Result<ToolOutput> {
        check_prepared(&p, &self.prepare(context, p.arguments.clone()).await?)?;
        run(
            context,
            format!("{GIT} status --porcelain=v1 --untracked-files=normal --ignore-submodules=all"),
            events,
            cancel,
        )
        .await
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DiffArgs {
    #[serde(default)]
    staged: bool,
}

#[async_trait]
impl Tool for GitDiff {
    fn definition(&self) -> ToolDefinition {
        definition("git_diff", "Read tracked-file Git diff; staged=true selects the index. External diff, textconv, pagers and fsmonitor helpers are disabled. Untracked files are listed by git_status.", Effect::Read,
            json!({"staged":{"type":"boolean"}}), &[])
    }
    async fn prepare(&self, context: &ToolContext, arguments: Value) -> Result<PreparedTool> {
        let a: DiffArgs = serde_json::from_value(arguments)?;
        prepared(
            json!({"staged":a.staged}),
            format!("Read {}Git diff", if a.staged { "staged " } else { "" }),
            json!({"root":paths::root(context)?}),
        )
    }
    async fn execute(
        &self,
        context: &ToolContext,
        p: PreparedTool,
        events: mpsc::Sender<ToolEvent>,
        cancel: CancellationToken,
    ) -> Result<ToolOutput> {
        check_prepared(&p, &self.prepare(context, p.arguments.clone()).await?)?;
        let a: DiffArgs = serde_json::from_value(p.arguments)?;
        run(
            context,
            format!(
                "{GIT} diff --no-ext-diff --no-textconv --no-color --ignore-submodules=all {} --",
                if a.staged { "--cached" } else { "" }
            ),
            events,
            cancel,
        )
        .await
    }
}
