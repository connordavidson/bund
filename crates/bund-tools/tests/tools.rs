use bund_core::{CommandExecutor, CommandSpec, Error, Tool, ToolContext, ToolOutput};
use bund_tools::{
    built_in_tools, guarded_undo, undo_tool, HostCommandExecutor, ARTIFACT_OUTPUT_LIMIT,
    DEFAULT_OUTPUT_LIMIT, MAX_OUTPUT_LIMIT,
};
use serde_json::{json, Value};
use std::{fs, path::Path, sync::Arc, time::Duration};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn setup() -> (tempfile::TempDir, ToolContext) {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("workspace")).unwrap();
    let context = ToolContext {
        workspace: dir.path().join("workspace"),
        artifact_dir: dir.path().join("private"),
        command_timeout_secs: 2,
        output_limit: DEFAULT_OUTPUT_LIMIT,
    };
    (dir, context)
}

fn tool(name: &str) -> Arc<dyn Tool> {
    built_in_tools()
        .into_iter()
        .find(|tool| tool.definition().name == name)
        .unwrap()
}

async fn invoke(name: &str, context: &ToolContext, args: Value) -> bund_core::Result<ToolOutput> {
    let t = tool(name);
    let prepared = t.prepare(context, args).await?;
    let (tx, _rx) = mpsc::channel(8);
    t.execute(context, prepared, tx, CancellationToken::new())
        .await
}

#[tokio::test]
async fn read_list_search_and_patch_undo_roundtrip() {
    let (_dir, context) = setup();
    fs::write(context.workspace.join("hello.txt"), "hello\nsecond line\n").unwrap();
    let result = invoke(
        "read_file",
        &context,
        json!({"path":"hello.txt","start_line":2,"max_lines":1}),
    )
    .await
    .unwrap();
    assert_eq!(result.content, "2: second line\n");
    let result = invoke("list_files", &context, json!({})).await.unwrap();
    assert_eq!(result.content, "hello.txt\n");
    let result = invoke("search", &context, json!({"pattern":"sec.*line"}))
        .await
        .unwrap();
    assert!(result.content.contains("hello.txt:2:second line"));
    let patch = tool("apply_patch");
    let prepared = patch
        .prepare(
            &context,
            json!({"path":"hello.txt","content":"new content\n"}),
        )
        .await
        .unwrap();
    assert!(prepared.preview.contains("--- a/hello.txt"));
    assert!(prepared.preview.contains("-second line"));
    assert!(prepared.preview.contains("+new content"));
    let (tx, _rx) = mpsc::channel(8);
    let result = patch
        .execute(&context, prepared, tx, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(context.workspace.join("hello.txt")).unwrap(),
        "new content\n"
    );
    assert_eq!(result.edits.len(), 1);
    let undo = patch.undo_tool(result.edits[0].clone()).unwrap();
    let prepared = undo.prepare(&context, json!({})).await.unwrap();
    assert!(prepared.preview.contains("+second line"));
    let (tx, _rx) = mpsc::channel(8);
    undo.execute(&context, prepared, tx, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(context.workspace.join("hello.txt")).unwrap(),
        "hello\nsecond line\n"
    );
}

#[tokio::test]
async fn newly_created_file_undo_removes_file_and_never_clobbers_user_changes() {
    let (_dir, context) = setup();
    let output = invoke(
        "apply_patch",
        &context,
        json!({"path":"new.txt","content":"generated\n"}),
    )
    .await
    .unwrap();
    guarded_undo(&context, &output.edits[0]).await.unwrap();
    assert!(!context.workspace.join("new.txt").exists());
    let output = invoke(
        "apply_patch",
        &context,
        json!({"path":"new.txt","content":"generated\n"}),
    )
    .await
    .unwrap();
    fs::write(context.workspace.join("new.txt"), "user work").unwrap();
    assert!(guarded_undo(&context, &output.edits[0]).await.is_err());
    assert_eq!(
        fs::read_to_string(context.workspace.join("new.txt")).unwrap(),
        "user work"
    );
    assert!(built_in_tools()
        .iter()
        .all(|t| t.definition().name != "undo_edit"));
    assert!(undo_tool(output.edits[0].clone())
        .prepare(&context, json!({}))
        .await
        .is_err());
}

#[tokio::test]
async fn stale_patch_preparation_cannot_overwrite_new_bytes() {
    let (_dir, context) = setup();
    fs::write(context.workspace.join("file"), "before").unwrap();
    let patch = tool("apply_patch");
    let prepared = patch
        .prepare(&context, json!({"path":"file","content":"after"}))
        .await
        .unwrap();
    fs::write(context.workspace.join("file"), "new user bytes").unwrap();
    let (tx, _rx) = mpsc::channel(8);
    assert!(patch
        .execute(&context, prepared, tx, CancellationToken::new())
        .await
        .is_err());
    assert_eq!(
        fs::read_to_string(context.workspace.join("file")).unwrap(),
        "new user bytes"
    );
    assert!(fs::read_dir(&context.workspace).unwrap().all(|e| !e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".bund-edit-")));
}

#[tokio::test]
async fn paths_reject_traversal_symlinks_special_files_and_git_metadata_writes() {
    use std::os::unix::fs::symlink;
    let (dir, context) = setup();
    fs::write(dir.path().join("secret"), "outside").unwrap();
    symlink(dir.path().join("secret"), context.workspace.join("link")).unwrap();
    symlink(dir.path(), context.workspace.join("dir-link")).unwrap();
    for path in ["../secret", "/etc/passwd", "link", "dir-link/secret"] {
        assert!(
            tool("read_file")
                .prepare(&context, json!({"path":path}))
                .await
                .is_err(),
            "accepted {path}"
        );
        assert!(
            tool("apply_patch")
                .prepare(&context, json!({"path":path,"content":"bad"}))
                .await
                .is_err(),
            "accepted {path}"
        );
    }
    fs::create_dir(context.workspace.join(".git")).unwrap();
    assert!(tool("apply_patch")
        .prepare(&context, json!({"path":".git/config","content":"bad"}))
        .await
        .is_err());
    let list = invoke("list_files", &context, json!({})).await.unwrap();
    assert!(!list.content.contains("link"));
    assert_eq!(
        fs::read_to_string(dir.path().join("secret")).unwrap(),
        "outside"
    );
    nix::unistd::mkfifo(
        &context.workspace.join("pipe"),
        nix::sys::stat::Mode::S_IRUSR,
    )
    .unwrap();
    assert!(tool("read_file")
        .prepare(&context, json!({"path":"pipe"}))
        .await
        .is_err());
}

#[tokio::test]
async fn symlink_swap_after_approval_and_cancellation_before_write_are_safe() {
    use std::os::unix::fs::symlink;
    let (dir, context) = setup();
    fs::write(context.workspace.join("file"), "before").unwrap();
    fs::write(dir.path().join("outside"), "private").unwrap();
    let t = tool("apply_patch");
    let p = t
        .prepare(&context, json!({"path":"file","content":"after"}))
        .await
        .unwrap();
    fs::remove_file(context.workspace.join("file")).unwrap();
    symlink(dir.path().join("outside"), context.workspace.join("file")).unwrap();
    let (tx, _rx) = mpsc::channel(8);
    assert!(t
        .execute(&context, p, tx, CancellationToken::new())
        .await
        .is_err());
    assert_eq!(
        fs::read_to_string(dir.path().join("outside")).unwrap(),
        "private"
    );
    let p = t
        .prepare(&context, json!({"path":"cancelled","content":"after"}))
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let (tx, _rx) = mpsc::channel(8);
    assert!(matches!(
        t.execute(&context, p, tx, cancel).await,
        Err(Error::Cancelled)
    ));
    assert!(!context.workspace.join("cancelled").exists());
}

#[tokio::test]
async fn atomic_patch_preserves_executable_mode() {
    use std::os::unix::fs::PermissionsExt;
    let (_dir, context) = setup();
    let path = context.workspace.join("executable");
    fs::write(&path, "old").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    invoke(
        "apply_patch",
        &context,
        json!({"path":"executable","content":"new"}),
    )
    .await
    .unwrap();
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

fn spec(context: &ToolContext, command: &str) -> CommandSpec {
    CommandSpec {
        command: command.into(),
        cwd: context.workspace.clone(),
        timeout_secs: context.command_timeout_secs,
        output_limit: context.output_limit,
        artifact_dir: context.artifact_dir.clone(),
    }
}

#[tokio::test]
async fn command_captures_both_streams_failure_and_does_not_inherit_secrets() {
    let (_dir, context) = setup();
    std::env::set_var("BUND_TEST_SECRET_TO_EXCLUDE", "do-not-inherit");
    let (tx, _rx) = mpsc::channel(8);
    let out = HostCommandExecutor.execute(spec(&context, "printf stdout; printf stderr >&2; printf '%s' \"${BUND_TEST_SECRET_TO_EXCLUDE-unset}\"; exit 7"), tx, CancellationToken::new()).await.unwrap();
    assert!(out.is_error);
    assert_eq!(out.exit_code, Some(7));
    assert!(out.content.contains("stdout"));
    assert!(out.content.contains("stderr"));
    assert!(out.content.contains("unset"));
    assert!(!out.content.contains("do-not-inherit"));
}

#[tokio::test]
async fn output_is_bounded_and_slow_subscribers_do_not_block_the_command() {
    use std::os::unix::fs::PermissionsExt;
    let (_dir, mut context) = setup();
    context.command_timeout_secs = 10;
    let (tx, _rx) = mpsc::channel(1);
    let out = HostCommandExecutor
        .execute(
            spec(&context, "yes x | head -c 9000000"),
            tx,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.len() <= DEFAULT_OUTPUT_LIMIT);
    assert!(out.content.contains("output truncated"));
    assert!(out.content.contains("artifact capped at 8 MiB"));
    let artifact = fs::read_dir(&context.artifact_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(
        fs::metadata(&artifact).unwrap().len(),
        ARTIFACT_OUTPUT_LIMIT as u64
    );
    assert_eq!(
        fs::metadata(artifact).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[tokio::test]
async fn larger_configured_output_limit_applies_to_files_and_command_streams() {
    let (_dir, mut context) = setup();
    context.output_limit = 128 * 1024;
    fs::write(context.workspace.join("large.txt"), "x".repeat(200_000)).unwrap();
    let read = invoke("read_file", &context, json!({"path":"large.txt"}))
        .await
        .unwrap();
    assert!(read.content.len() > DEFAULT_OUTPUT_LIMIT);
    assert!(read.content.len() <= context.output_limit);
    let search = invoke("search", &context, json!({"pattern":"x"}))
        .await
        .unwrap();
    assert!(search.content.len() > DEFAULT_OUTPUT_LIMIT);
    assert!(search.content.len() <= context.output_limit);
    let (tx, mut rx) = mpsc::channel(64);
    let out = HostCommandExecutor
        .execute(
            spec(&context, "yes x | head -c 200000"),
            tx,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(!out.is_error);
    assert!(out.content.len() > DEFAULT_OUTPUT_LIMIT);
    assert!(out.content.len() <= context.output_limit);
    let mut streamed = 0;
    while let Ok(bund_core::ToolEvent::Output(text)) = rx.try_recv() {
        streamed += text.len();
    }
    assert_eq!(streamed, context.output_limit);
    context.output_limit = MAX_OUTPUT_LIMIT * 2;
    fs::write(
        context.workspace.join("large.txt"),
        "x".repeat(MAX_OUTPUT_LIMIT + 100_000),
    )
    .unwrap();
    let read = invoke("read_file", &context, json!({"path":"large.txt"}))
        .await
        .unwrap();
    assert!(read.content.len() <= MAX_OUTPUT_LIMIT);
    context.output_limit = 4096;
    let (tx, mut rx) = mpsc::channel(64);
    let out = HostCommandExecutor
        .execute(
            spec(
                &context,
                "head -c 20000 /dev/zero | LC_ALL=C tr '\\000' '\\377'",
            ),
            tx,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(!out.is_error);
    assert!(out.content.len() <= context.output_limit);
    let mut streamed = 0;
    while let Ok(bund_core::ToolEvent::Output(text)) = rx.try_recv() {
        streamed += text.len();
    }
    assert!(
        streamed <= context.output_limit,
        "UTF-8 replacement bytes must also count toward the live limit"
    );
}

#[tokio::test]
async fn timeout_terminates_descendants_including_term_resistant_processes() {
    let (_dir, mut context) = setup();
    context.command_timeout_secs = 1;
    let (tx, _rx) = mpsc::channel(1);
    let out = HostCommandExecutor
        .execute(
            spec(
                &context,
                "sh -c 'trap \"\" TERM; sleep 2; echo escaped > marker' & wait",
            ),
            tx,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(out.content.contains("timed out"));
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(!context.workspace.join("marker").exists());
}

#[tokio::test]
async fn cancellation_and_successful_parent_exit_both_stop_background_children() {
    let (_dir, context) = setup();
    let cancel = CancellationToken::new();
    let next = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        next.cancel();
    });
    let (tx, _rx) = mpsc::channel(1);
    let result = HostCommandExecutor
        .execute(
            spec(
                &context,
                "sh -c 'sleep 1; echo escaped > cancel-marker' & wait",
            ),
            tx,
            cancel,
        )
        .await;
    assert!(matches!(result, Err(Error::Cancelled)));
    let (tx, _rx) = mpsc::channel(1);
    HostCommandExecutor
        .execute(
            spec(
                &context,
                "sh -c 'sleep 1; echo escaped > exit-marker' & exit 0",
            ),
            tx,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(!context.workspace.join("cancel-marker").exists());
    assert!(!context.workspace.join("exit-marker").exists());
}

#[tokio::test]
async fn pre_cancelled_command_never_spawns() {
    let (_dir, context) = setup();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let (tx, _rx) = mpsc::channel(1);
    assert!(matches!(
        HostCommandExecutor
            .execute(spec(&context, "echo bad > marker"), tx, cancel)
            .await,
        Err(Error::Cancelled)
    ));
    assert!(!context.workspace.join("marker").exists());
}

#[tokio::test]
async fn git_tools_ignore_external_diff_and_fsmonitor_helpers() {
    let (_dir, context) = setup();
    fn git(root: &Path, args: &[&str]) {
        assert!(std::process::Command::new("/usr/bin/git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap()
            .status
            .success());
    }
    git(&context.workspace, &["init"]);
    fs::write(context.workspace.join("file"), "before").unwrap();
    git(&context.workspace, &["add", "file"]);
    git(
        &context.workspace,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-m",
            "initial",
        ],
    );
    fs::write(context.workspace.join("file"), "after").unwrap();
    git(
        &context.workspace,
        &["config", "diff.external", "sh -c 'touch helper-ran'"],
    );
    git(
        &context.workspace,
        &["config", "core.fsmonitor", "sh -c 'touch helper-ran'"],
    );
    let status = invoke("git_status", &context, json!({})).await.unwrap();
    assert!(!status.is_error, "{}", status.content);
    assert!(status.content.contains(" M file"));
    let diff = invoke("git_diff", &context, json!({})).await.unwrap();
    assert!(!diff.is_error, "{}", diff.content);
    assert!(diff.content.contains("+after"));
    assert!(!context.workspace.join("helper-ran").exists());
    git(
        &context.workspace,
        &["config", "filter.dangerous.clean", "touch filter-ran"],
    );
    fs::write(
        context.workspace.join(".gitattributes"),
        "file filter=dangerous\n",
    )
    .unwrap();
    assert!(matches!(
        invoke("git_status", &context, json!({})).await,
        Err(Error::Denied(_))
    ));
    assert!(matches!(
        invoke("git_diff", &context, json!({})).await,
        Err(Error::Denied(_))
    ));
    assert!(!context.workspace.join("filter-ran").exists());
}
