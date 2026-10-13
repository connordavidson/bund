use async_trait::async_trait;
use bund_core::*;
use bund_store::SqliteStore;
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

fn engine(store: Arc<SqliteStore>) -> Engine {
    Engine::new(EngineDependencies {
        providers: vec![],
        tools: bund_tools::built_in_tools(),
        store,
    })
    .unwrap()
}

async fn session(agent: &Engine, workspace: &std::path::Path) -> SessionHandle {
    agent
        .create_session(SessionOptions::new(
            workspace.into(),
            ModelProfile::new("offline", "fixture"),
        ))
        .await
        .unwrap()
}

async fn finish(
    handle: &SessionHandle,
    mut events: broadcast::Receiver<SessionEvent>,
    mut decision: impl FnMut(&ApprovalRequest) -> ApprovalDecision,
) -> Vec<SessionEvent> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut log = Vec::new();
        loop {
            let event = events.recv().await.unwrap();
            if let EventKind::ApprovalRequired(request) = &event.kind {
                handle
                    .answer_approval(&request.id, decision(request))
                    .await
                    .unwrap();
            }
            let done = matches!(event.kind, EventKind::RunFinished { .. });
            log.push(event);
            if done {
                return log;
            }
        }
    })
    .await
    .expect("local action must finish")
}

async fn run(
    handle: &SessionHandle,
    name: &str,
    args: Value,
    decision: impl FnMut(&ApprovalRequest) -> ApprovalDecision,
) -> Vec<SessionEvent> {
    let events = handle.attach().await.unwrap().events;
    handle.run_tool(name, args).await.unwrap();
    finish(handle, events, decision).await
}

fn no_approval(_: &ApprovalRequest) -> ApprovalDecision {
    panic!("matching saved grant must not prompt again")
}

fn status(log: &[SessionEvent]) -> RunStatus {
    match &log.last().unwrap().kind {
        EventKind::RunFinished { status, .. } => *status,
        _ => panic!("missing terminal event"),
    }
}

#[tokio::test]
async fn edit_grant_covers_other_edits_resumes_and_does_not_leak_to_other_sessions_or_plan() {
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let store = Arc::new(SqliteStore::open(data.path()).unwrap());
    let agent = engine(store.clone());
    let handle = session(&agent, workspace.path()).await;
    let log = run(
        &handle,
        "apply_patch",
        json!({"path":"first","content":"one"}),
        |request| {
            assert_eq!(
                request.session_scope,
                Some(SessionApprovalScope::WorkspaceEdits)
            );
            assert!(request
                .preview
                .starts_with("Approve for this session: allow workspace edits"));
            ApprovalDecision::ApproveSession
        },
    )
    .await;
    assert_eq!(status(&log), RunStatus::Completed);
    let saved_grant = log
        .iter()
        .position(|e| matches!(e.kind, EventKind::SettingsChanged))
        .unwrap();
    let start = log
        .iter()
        .position(|e| matches!(e.kind, EventKind::ToolStarted { .. }))
        .unwrap();
    assert!(saved_grant < start);
    assert!(handle.snapshot().await.unwrap().policy.accept_edits);
    assert_eq!(
        status(
            &run(
                &handle,
                "apply_patch",
                json!({"path":"second","content":"two"}),
                no_approval
            )
            .await
        ),
        RunStatus::Completed
    );
    let id = handle.snapshot().await.unwrap().id;
    let reopened = engine(store.clone()).open_session(&id).await.unwrap();
    assert!(reopened.snapshot().await.unwrap().policy.accept_edits);
    assert_eq!(
        status(
            &run(
                &reopened,
                "apply_patch",
                json!({"path":"third","content":"three"}),
                no_approval
            )
            .await
        ),
        RunStatus::Completed
    );

    let other = session(&agent, workspace.path()).await;
    assert!(!other.snapshot().await.unwrap().policy.accept_edits);
    assert_eq!(
        status(
            &run(
                &other,
                "apply_patch",
                json!({"path":"first","content":"unexpected"}),
                |_| ApprovalDecision::Deny
            )
            .await
        ),
        RunStatus::Failed
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("first")).unwrap(),
        "one"
    );
    reopened.set_mode(Mode::Plan).await.unwrap();
    assert_eq!(
        status(
            &run(
                &reopened,
                "apply_patch",
                json!({"path":"first","content":"unexpected"}),
                no_approval
            )
            .await
        ),
        RunStatus::Failed
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("first")).unwrap(),
        "one"
    );
}

#[tokio::test]
async fn command_grants_match_exact_command_and_normalized_directory_only() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::create_dir(workspace.path().join("sub")).unwrap();
    let data = tempfile::tempdir().unwrap();
    let store = Arc::new(SqliteStore::open(data.path()).unwrap());
    let agent = engine(store.clone());
    let handle = session(&agent, workspace.path()).await;
    let command = "printf first >> log";
    let log = run(
        &handle,
        "run_command",
        json!({"command":command,"cwd":"."}),
        |request| {
            assert_eq!(
                request.session_scope,
                Some(SessionApprovalScope::Command {
                    command: command.into(),
                    cwd: ".".into()
                })
            );
            assert!(request
                .preview
                .starts_with("Approve for this session: allow only this exact command"));
            ApprovalDecision::ApproveSession
        },
    )
    .await;
    assert_eq!(status(&log), RunStatus::Completed);
    assert_eq!(
        status(
            &run(
                &handle,
                "run_command",
                json!({"command":command,"cwd":"./"}),
                no_approval
            )
            .await
        ),
        RunStatus::Completed
    );
    let mut prompts = 0;
    for args in [
        json!({"command":format!("{command}; printf extra >> log"),"cwd":"."}),
        json!({"command":command,"cwd":"sub"}),
    ] {
        let log = run(&handle, "run_command", args, |_| {
            prompts += 1;
            ApprovalDecision::Deny
        })
        .await;
        assert_eq!(status(&log), RunStatus::Failed);
    }
    assert_eq!(prompts, 2);
    assert!(!workspace.path().join("sub/log").exists());
    assert_eq!(
        status(
            &run(
                &handle,
                "run_command",
                json!({"command":command,"cwd":"sub/."}),
                |request| {
                    assert_eq!(
                        request.session_scope,
                        Some(SessionApprovalScope::Command {
                            command: command.into(),
                            cwd: "sub".into()
                        })
                    );
                    ApprovalDecision::ApproveSession
                }
            )
            .await
        ),
        RunStatus::Completed
    );
    assert_eq!(
        status(
            &run(
                &handle,
                "run_command",
                json!({"command":command,"cwd":"sub"}),
                no_approval
            )
            .await
        ),
        RunStatus::Completed
    );
    let snapshot = handle.snapshot().await.unwrap();
    assert_eq!(snapshot.policy.session_commands.len(), 2);
    let reopened = engine(store).open_session(&snapshot.id).await.unwrap();
    assert_eq!(
        reopened.snapshot().await.unwrap().policy.session_commands,
        snapshot.policy.session_commands
    );
    assert_eq!(
        status(
            &run(
                &reopened,
                "run_command",
                json!({"command":command}),
                no_approval
            )
            .await
        ),
        RunStatus::Completed
    );
    let other = session(&agent, workspace.path()).await;
    assert_eq!(
        status(
            &run(&other, "run_command", json!({"command":command}), |_| {
                ApprovalDecision::Deny
            })
            .await
        ),
        RunStatus::Failed
    );
    reopened.set_mode(Mode::Plan).await.unwrap();
    assert_eq!(
        status(
            &run(
                &reopened,
                "run_command",
                json!({"command":command}),
                no_approval
            )
            .await
        ),
        RunStatus::Failed
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("log")).unwrap(),
        "firstfirstfirst"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("sub/log")).unwrap(),
        "firstfirst"
    );
}

#[tokio::test]
async fn stale_or_cancelled_request_never_installs_a_session_grant() {
    for cancel in [false, true] {
        let workspace = tempfile::tempdir().unwrap();
        let file = workspace.path().join("file");
        std::fs::write(&file, "original").unwrap();
        let data = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteStore::open(data.path()).unwrap());
        let handle = session(&engine(store.clone()), workspace.path()).await;
        let mut events = handle.attach().await.unwrap().events;
        handle
            .run_tool("apply_patch", json!({"path":"file","content":"agent"}))
            .await
            .unwrap();
        let request = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let EventKind::ApprovalRequired(request) = events.recv().await.unwrap().kind {
                    break request;
                }
            }
        })
        .await
        .unwrap();
        if cancel {
            handle.cancel().await.unwrap();
        } else {
            std::fs::write(&file, "human").unwrap();
        }
        // Cancellation may close the sender first; either ordering must stay safe.
        let decision = handle
            .answer_approval(&request.id, ApprovalDecision::ApproveSession)
            .await;
        if !cancel {
            decision.unwrap();
        }
        let log = finish(&handle, events, no_approval).await;
        assert_eq!(
            status(&log),
            if cancel {
                RunStatus::Cancelled
            } else {
                RunStatus::Failed
            }
        );
        assert!(!log.iter().any(|event| matches!(
            event.kind,
            EventKind::SettingsChanged | EventKind::ToolStarted { .. }
        )));
        let snapshot = handle.snapshot().await.unwrap();
        assert!(!snapshot.policy.accept_edits);
        assert!(
            !store
                .load(&snapshot.id)
                .await
                .unwrap()
                .options
                .policy
                .accept_edits
        );
        assert_eq!(
            std::fs::read_to_string(file).unwrap(),
            if cancel { "original" } else { "human" }
        );
    }
}

struct CustomExecute;
#[async_trait]
impl Tool for CustomExecute {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "custom_execute".into(),
            description: "fixture".into(),
            input_schema: json!({"type":"object"}),
            effect: Effect::Execute,
        }
    }
    async fn prepare(&self, _: &ToolContext, arguments: Value) -> Result<PreparedTool> {
        Ok(PreparedTool {
            arguments,
            preview: "custom".into(),
            fingerprint: "fixed".into(),
        })
    }
    async fn execute(
        &self,
        _: &ToolContext,
        _: PreparedTool,
        _: mpsc::Sender<ToolEvent>,
        _: CancellationToken,
    ) -> Result<ToolOutput> {
        panic!("denied unsupported command must not run")
    }
}

#[tokio::test]
async fn unsupported_scope_is_rejected_without_consuming_approval() {
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let agent = Engine::new(EngineDependencies {
        providers: vec![],
        tools: vec![Arc::new(CustomExecute)],
        store: Arc::new(SqliteStore::open(data.path()).unwrap()),
    })
    .unwrap();
    let handle = session(&agent, workspace.path()).await;
    let mut events = handle.attach().await.unwrap().events;
    handle.run_tool("custom_execute", json!({})).await.unwrap();
    let request = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let EventKind::ApprovalRequired(request) = events.recv().await.unwrap().kind {
                break request;
            }
        }
    })
    .await
    .unwrap();
    assert!(request.session_scope.is_none());
    assert!(matches!(
        handle
            .answer_approval(&request.id, ApprovalDecision::ApproveSession)
            .await,
        Err(Error::Invalid(_))
    ));
    assert_eq!(
        handle
            .snapshot()
            .await
            .unwrap()
            .pending_approval
            .unwrap()
            .id,
        request.id
    );
    handle
        .answer_approval(&request.id, ApprovalDecision::Deny)
        .await
        .unwrap();
    assert_eq!(
        status(&finish(&handle, events, no_approval).await),
        RunStatus::Failed
    );
}

#[test]
fn legacy_policy_request_and_approval_decisions_still_deserialize() {
    let policy: Policy = serde_json::from_value(
        json!({"mode":"act","accept_edits":false,"allowed_commands":["cargo test"]}),
    )
    .unwrap();
    assert!(policy.session_commands.is_empty());
    assert_eq!(policy.allowed_commands, ["cargo test"]);
    let request: ApprovalRequest = serde_json::from_value(json!({"id":"a","run_id":"r","tool_call":{"id":"c","name":"apply_patch","arguments":{}},"preview":"old","fingerprint":"f","workspace":"/tmp"})).unwrap();
    assert!(request.session_scope.is_none());
    for (encoded, decision) in [
        (json!("approve_once"), ApprovalDecision::ApproveOnce),
        (json!("deny"), ApprovalDecision::Deny),
        (json!("approve_session"), ApprovalDecision::ApproveSession),
    ] {
        assert_eq!(
            serde_json::from_value::<ApprovalDecision>(encoded.clone()).unwrap(),
            decision
        );
        assert_eq!(serde_json::to_value(decision).unwrap(), encoded);
    }
}
