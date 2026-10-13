//! Offline SDK demonstration: no API key, network connection, or CLI crate.
//! Run: cargo run -p bund-store --example sdk
use async_trait::async_trait;
use bund_core::{
    ApprovalDecision, ApprovalRequest, Attachment, Engine, EngineDependencies, Error, EventKind,
    ModelProfile, Provider, ProviderEvent, ProviderRequest, ProviderResponse, Result, RunStatus,
    SessionEvent, SessionHandle, SessionOptions, ToolCall, Usage, ValidationStatus,
};
use bund_store::SqliteStore;
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

const CHECK: &str = "test \"$(cat answer.txt)\" = 42";
const CANCEL_COMMAND: &str = "printf 'command started\\n'; sleep 30; touch should-not-exist";

/// A deterministic provider fixture exercises the same engine interface as a
/// real API adapter. It checks tool results before advancing to its next step.
#[derive(Default)]
struct ExampleProvider {
    step: AtomicUsize,
}

#[async_trait]
impl Provider for ExampleProvider {
    fn id(&self) -> &str {
        "example"
    }

    async fn complete(
        &self,
        request: ProviderRequest,
        events: mpsc::Sender<ProviderEvent>,
        cancel: CancellationToken,
    ) -> Result<ProviderResponse> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let results: Vec<_> = request
            .messages
            .iter()
            .flat_map(|message| &message.tool_results)
            .collect();
        let (text, tool_calls) = match self.step.fetch_add(1, Ordering::Relaxed) {
            0 => (
                "Inspecting the existing value.",
                vec![ToolCall {
                    id: "inspect-answer".into(),
                    name: "read_file".into(),
                    arguments: json!({"path":"answer.txt"}),
                }],
            ),
            1 => {
                require(
                    results.iter().any(|result| {
                        result.call_id == "inspect-answer"
                            && !result.is_error
                            && result.content.contains("41")
                    }),
                    "the provider should receive the actual read result",
                )?;
                (
                    "Changing the value to 42.",
                    vec![ToolCall {
                        id: "update-answer".into(),
                        name: "apply_patch".into(),
                        arguments: json!({"path":"answer.txt","content":"42\n"}),
                    }],
                )
            }
            2 => {
                require(
                    results
                        .iter()
                        .any(|result| result.call_id == "update-answer" && !result.is_error),
                    "the approved patch should have completed",
                )?;
                ("The edit is ready for configured validation.", vec![])
            }
            3 => {
                require(
                    results.iter().any(|result| {
                        result.name == "run_command"
                            && !result.is_error
                            && result.content.contains("exit code: 0")
                    }),
                    "the final answer must follow the real validation result",
                )?;
                (
                    "answer.txt contains 42 and its configured check passed.",
                    vec![],
                )
            }
            _ => return Err(Error::Invalid("example provider script exhausted".into())),
        };
        let _ = events.send(ProviderEvent::TextDelta(text.into())).await;
        Ok(ProviderResponse {
            text: text.into(),
            tool_calls,
            continuation: None,
            usage: Usage::default(),
        })
    }
}

fn require(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(Error::Invalid(message.into()))
    }
}

fn engine(store: Arc<SqliteStore>, provider: Arc<ExampleProvider>) -> Result<Engine> {
    Engine::new(EngineDependencies {
        providers: vec![provider],
        tools: bund_tools::built_in_tools(),
        store,
    })
}

/// This example deliberately authorizes only its known fixture operations.
/// A desktop host would present the preview to a person and send their choice.
fn approve_fixture(request: &ApprovalRequest) -> bool {
    match request.tool_call.name.as_str() {
        "apply_patch" => {
            request.tool_call.arguments == json!({"path":"answer.txt","content":"42\n"})
        }
        "run_command" => request
            .tool_call
            .arguments
            .get("command")
            .is_some_and(|command| command == CHECK || command == CANCEL_COMMAND),
        _ => false,
    }
}

/// Treat broadcasts as wakeups and read committed events by cursor. A slow UI
/// can recover from a Lagged notification without losing approvals/results.
async fn next_event(
    session: &SessionHandle,
    attachment: &mut Attachment,
    cursor: u64,
) -> Result<SessionEvent> {
    loop {
        if let Some(event) = session.events_after(cursor, 1).await?.into_iter().next() {
            return Ok(event);
        }
        match attachment.events.recv().await {
            Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => {
                return Err(Error::Invalid("session event stream closed".into()));
            }
        }
    }
}

async fn drive(
    session: &SessionHandle,
    mut attachment: Attachment,
    run_id: &str,
    cancel_command: bool,
) -> Result<RunStatus> {
    let mut cursor = attachment.snapshot.cursor;
    loop {
        let event = tokio::time::timeout(
            Duration::from_secs(10),
            next_event(session, &mut attachment, cursor),
        )
        .await
        .map_err(|_| Error::Invalid("example timed out waiting for an event".into()))??;
        cursor = event.sequence;
        if event.run_id.as_deref() != Some(run_id) {
            continue;
        }
        match event.kind {
            EventKind::ApprovalRequired(request) => {
                let approved = approve_fixture(&request);
                println!("Approval preview:\n{}", request.preview);
                session
                    .answer_approval(
                        &request.id,
                        if approved {
                            ApprovalDecision::ApproveOnce
                        } else {
                            ApprovalDecision::Deny
                        },
                    )
                    .await?;
            }
            EventKind::ToolStarted { call } => {
                println!("Started {}", call.name);
                if cancel_command && call.name == "run_command" {
                    // Allow the child to start so cancellation exercises the
                    // executor's process cleanup, not just pending approval.
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    session.cancel().await?;
                }
            }
            EventKind::ToolFinished(result) => {
                println!("Finished {} (error: {})", result.name, result.is_error);
            }
            EventKind::AssistantMessage { text } => println!("Agent: {text}"),
            EventKind::RunFinished {
                status, validation, ..
            } => {
                println!("Run finished: {status:?}; validation: {validation:?}");
                return Ok(status);
            }
            _ => {}
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    std::fs::write(workspace.path().join("answer.txt"), "41\n")?;

    let provider = Arc::new(ExampleProvider::default());
    let store = Arc::new(SqliteStore::open(state.path())?);
    let agent = engine(store.clone(), provider.clone())?;
    let mut options = SessionOptions::new(
        workspace.path().into(),
        ModelProfile::new("example", "offline-fixture"),
    );
    options.checks = vec![CHECK.into()];
    options.limits.command_timeout_secs = 60;
    let session = agent.create_session(options).await?;

    // Attach before submission so the initial snapshot and subscription form
    // one observation point; live updates then follow its journal cursor.
    let attachment = session.attach().await?;
    let run_id = session
        .submit("Inspect answer.txt, change 41 to 42, and verify the result.".into())
        .await?;
    assert_eq!(
        drive(&session, attachment, &run_id, false).await?,
        RunStatus::Completed
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("answer.txt"))?,
        "42\n"
    );
    let saved = session.snapshot().await?;
    assert_eq!(saved.validation, ValidationStatus::Passed);

    // A new engine can reopen the same SQLite-backed conversation. The next
    // host operation needs no model request and still follows approvals/events.
    let resumed = engine(store, provider)?.open_session(&saved.id).await?;
    assert_eq!(resumed.snapshot().await?.cursor, saved.cursor);
    let attachment = resumed.attach().await?;
    let run_id = resumed
        .run_tool("run_command", json!({"command":CANCEL_COMMAND}))
        .await?;
    assert_eq!(
        drive(&resumed, attachment, &run_id, true).await?,
        RunStatus::Cancelled
    );
    assert!(!workspace.path().join("should-not-exist").exists());
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("answer.txt"))?,
        "42\n"
    );
    println!("SDK example passed: inspect, approved edit/check, durable resume, and cancellation.");
    Ok(())
}
