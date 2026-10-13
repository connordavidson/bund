//! A journal failure must stop execution without dropping executor cleanup.
use async_trait::async_trait;
use bund_core::*;
use bund_store::SqliteStore;
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy)]
enum FailurePoint {
    ToolOutput,
    ToolStarted,
}

struct FaultStore {
    inner: SqliteStore,
    point: FailurePoint,
    failed: AtomicBool,
    persistent: bool,
}

#[async_trait]
impl SessionStore for FaultStore {
    async fn create(&self, session: &StoredSession) -> Result<()> {
        self.inner.create(session).await
    }
    async fn load(&self, id: &str) -> Result<StoredSession> {
        self.inner.load(id).await
    }
    async fn list(&self) -> Result<Vec<SessionSnapshot>> {
        self.inner.list().await
    }
    async fn archive_context(&self, session: &StoredSession) -> Result<()> {
        self.inner.archive_context(session).await
    }
    async fn commit(&self, session: &StoredSession, events: &[SessionEvent]) -> Result<()> {
        let trigger = events.iter().any(|event| {
            matches!(
                (&self.point, &event.kind),
                (FailurePoint::ToolOutput, EventKind::ToolOutput { .. })
                    | (FailurePoint::ToolStarted, EventKind::ToolStarted { .. })
            )
        });
        let first_failure = trigger && !self.failed.swap(true, Ordering::SeqCst);
        // Simulate a persistently full/unavailable journal, including failure
        // of the final diagnostic, instead of failing just one convenient write.
        if first_failure || (self.persistent && self.failed.load(Ordering::SeqCst)) {
            return Err(Error::Storage("injected journal unavailable".into()));
        }
        self.inner.commit(session, events).await
    }
    async fn events_after(&self, id: &str, cursor: u64, limit: usize) -> Result<Vec<SessionEvent>> {
        self.inner.events_after(id, cursor, limit).await
    }
    fn artifact_dir(&self, session_id: &str) -> PathBuf {
        self.inner.artifact_dir(session_id)
    }
    fn lock_workspace(&self, workspace: &Path) -> Result<Box<dyn WorkspaceGuard>> {
        self.inner.lock_workspace(workspace)
    }
}

#[derive(Default)]
struct ExecutionState {
    first_started: AtomicUsize,
    cancellation_observed: AtomicBool,
    cleanup_complete: AtomicBool,
    second_started: AtomicUsize,
    provider_calls: AtomicUsize,
}

struct TwoCalls(Arc<ExecutionState>);

#[async_trait]
impl Provider for TwoCalls {
    fn id(&self) -> &str {
        "fixture"
    }
    async fn complete(
        &self,
        _request: ProviderRequest,
        _events: mpsc::Sender<ProviderEvent>,
        _cancel: CancellationToken,
    ) -> Result<ProviderResponse> {
        self.0.provider_calls.fetch_add(1, Ordering::SeqCst);
        Ok(ProviderResponse {
            reasoning: vec![],
            text: "Run the two proposed actions.".into(),
            tool_calls: vec![
                ToolCall {
                    id: "first".into(),
                    name: "long_running".into(),
                    arguments: json!({}),
                },
                ToolCall {
                    id: "second".into(),
                    name: "must_not_run".into(),
                    arguments: json!({}),
                },
            ],
            continuation: None,
            usage: Usage::default(),
        })
    }
}

struct InstrumentedTool {
    state: Arc<ExecutionState>,
    long: bool,
}

#[async_trait]
impl Tool for InstrumentedTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: if self.long {
                "long_running"
            } else {
                "must_not_run"
            }
            .into(),
            description: "Fault-injection test operation".into(),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
            effect: Effect::Execute,
        }
    }
    async fn prepare(&self, _context: &ToolContext, arguments: Value) -> Result<PreparedTool> {
        Ok(PreparedTool {
            arguments,
            preview: "Run the exact fixture operation".into(),
            fingerprint: self.definition().name,
        })
    }
    async fn execute(
        &self,
        _context: &ToolContext,
        _prepared: PreparedTool,
        events: mpsc::Sender<ToolEvent>,
        cancel: CancellationToken,
    ) -> Result<ToolOutput> {
        if !self.long {
            self.state.second_started.fetch_add(1, Ordering::SeqCst);
            return Ok(ToolOutput::default());
        }
        self.state.first_started.fetch_add(1, Ordering::SeqCst);
        events
            .send(ToolEvent::Output(
                "A running process produced output.".into(),
            ))
            .await
            .map_err(|_| Error::Invalid("output receiver dropped".into()))?;
        cancel.cancelled().await;
        self.state
            .cancellation_observed
            .store(true, Ordering::SeqCst);
        // Make cleanup observably asynchronous: the engine must await it,
        // rather than just signal cancellation and publish RunFinished.
        tokio::time::sleep(Duration::from_millis(40)).await;
        self.state.cleanup_complete.store(true, Ordering::SeqCst);
        Err(Error::Cancelled)
    }
}

async fn fault_run(point: FailurePoint, persistent: bool) {
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let store = Arc::new(FaultStore {
        inner: SqliteStore::open(data.path()).unwrap(),
        point,
        failed: AtomicBool::new(false),
        persistent,
    });
    let state = Arc::new(ExecutionState::default());
    let engine = Engine::new(EngineDependencies {
        providers: vec![Arc::new(TwoCalls(state.clone()))],
        tools: vec![
            Arc::new(InstrumentedTool {
                state: state.clone(),
                long: true,
            }),
            Arc::new(InstrumentedTool {
                state: state.clone(),
                long: false,
            }),
        ],
        store: store.clone(),
    })
    .unwrap();
    let session = engine
        .create_session(SessionOptions::new(
            workspace.path().into(),
            ModelProfile::new("fixture", "fixture"),
        ))
        .await
        .unwrap();
    let id = session.snapshot().await.unwrap().id;
    let mut events = session.attach().await.unwrap().events;
    session
        .submit("Perform the fixture workflow.".into())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = events.recv().await.unwrap();
            match event.kind {
                EventKind::ApprovalRequired(approval) => {
                    assert_eq!(
                        approval.tool_call.name, "long_running",
                        "no later action may be proposed after a journal failure"
                    );
                    session
                        .answer_approval(&approval.id, ApprovalDecision::ApproveOnce)
                        .await
                        .unwrap();
                }
                EventKind::RunFinished {
                    status, message, ..
                } => {
                    assert_eq!(status, RunStatus::Failed);
                    if persistent {
                        assert!(message.contains("Journal failure"));
                        assert!(message.contains("could not be persisted"));
                    } else {
                        assert!(message.contains("injected journal unavailable"));
                    }
                    match point {
                        FailurePoint::ToolOutput => {
                            assert_eq!(state.first_started.load(Ordering::SeqCst), 1);
                            assert!(
                                state.cancellation_observed.load(Ordering::SeqCst),
                                "tool must see cancellation before terminal notification"
                            );
                            assert!(
                                state.cleanup_complete.load(Ordering::SeqCst),
                                "terminal notification must wait for asynchronous cleanup"
                            );
                        }
                        FailurePoint::ToolStarted => {
                            assert_eq!(
                                state.first_started.load(Ordering::SeqCst),
                                0,
                                "failed start record must prevent side effects"
                            );
                        }
                    }
                    break;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("journal failure must terminate instead of hanging");
    assert_eq!(state.second_started.load(Ordering::SeqCst), 0);
    assert_eq!(state.provider_calls.load(Ordering::SeqCst), 1);
    assert!(store.failed.load(Ordering::SeqCst));
    let durable = store.events_after(&id, 0, 4096).await.unwrap();
    let has_terminal = durable
        .iter()
        .any(|event| matches!(event.kind, EventKind::RunFinished { .. }));
    assert_eq!(
        has_terminal, !persistent,
        "a recovered journal can persist the terminal failure"
    );
    let started = durable
        .iter()
        .filter(|event| matches!(event.kind, EventKind::ToolStarted { .. }))
        .count();
    assert_eq!(
        started,
        usize::from(matches!(point, FailurePoint::ToolOutput))
    );
    // The workspace must be available after the failure diagnostic.
    let _lock = store.lock_workspace(workspace.path()).unwrap();
}

#[tokio::test]
async fn output_journal_failure_cancels_and_awaits_cleanup_before_failed_notification() {
    fault_run(FailurePoint::ToolOutput, true).await;
}

#[tokio::test]
async fn failed_tool_started_commit_prevents_side_effects() {
    fault_run(FailurePoint::ToolStarted, true).await;
}

#[tokio::test]
async fn transient_output_commit_failure_still_reports_failure_after_cleanup() {
    fault_run(FailurePoint::ToolOutput, false).await;
}

#[tokio::test]
async fn transient_tool_started_commit_failure_does_not_continue_tools() {
    fault_run(FailurePoint::ToolStarted, false).await;
}
