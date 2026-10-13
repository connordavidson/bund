use async_trait::async_trait;
use bund_core::*;
use bund_store::SqliteStore;
use serde_json::json;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct FakeProvider {
    responses: Mutex<VecDeque<ProviderResponse>>,
    requests: Mutex<Vec<ProviderRequest>>,
}
impl FakeProvider {
    fn new(responses: Vec<ProviderResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(vec![]),
        }
    }
}
#[async_trait]
impl Provider for FakeProvider {
    fn id(&self) -> &str {
        "fake"
    }
    async fn complete(
        &self,
        request: ProviderRequest,
        events: mpsc::Sender<ProviderEvent>,
        cancel: CancellationToken,
    ) -> Result<ProviderResponse> {
        self.requests.lock().unwrap().push(request);
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| Error::Invalid("fixture exhausted".into()))?;
        let _ = events
            .send(ProviderEvent::TextDelta(response.text.clone()))
            .await;
        Ok(response)
    }
}
fn response(calls: Vec<ToolCall>) -> ProviderResponse {
    ProviderResponse {
        reasoning: vec![],
        text: if calls.is_empty() {
            "Finished".into()
        } else {
            "Working".into()
        },
        tool_calls: calls,
        continuation: None,
        usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
            cached_tokens: 0,
        },
    }
}
fn call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
    }
}
fn engine(store: Arc<SqliteStore>, provider: Arc<FakeProvider>) -> Engine {
    Engine::new(EngineDependencies {
        providers: vec![provider],
        tools: bund_tools::built_in_tools(),
        store,
    })
    .unwrap()
}
async fn finish(
    handle: &SessionHandle,
    mut events: tokio::sync::broadcast::Receiver<SessionEvent>,
    approve: bool,
) -> (RunStatus, Vec<SessionEvent>) {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut all = vec![];
        loop {
            let event = events.recv().await.unwrap();
            if let EventKind::ApprovalRequired(request) = &event.kind {
                handle
                    .answer_approval(
                        &request.id,
                        if approve {
                            ApprovalDecision::ApproveOnce
                        } else {
                            ApprovalDecision::Deny
                        },
                    )
                    .await
                    .unwrap();
            }
            if let EventKind::RunFinished { status, .. } = &event.kind {
                let status = *status;
                all.push(event);
                return (status, all);
            }
            all.push(event);
        }
    })
    .await
    .expect("run should finish")
}

#[tokio::test]
async fn inspect_edit_validate_resume_and_guarded_undo() {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("value.txt"), "old\n").unwrap();
    let provider = Arc::new(FakeProvider::new(vec![
        response(vec![call("r", "read_file", json!({"path":"value.txt"}))]),
        response(vec![call(
            "w",
            "apply_patch",
            json!({"path":"value.txt","content":"new\n"}),
        )]),
        response(vec![]),
        response(vec![]),
    ]));
    let store = Arc::new(SqliteStore::open(data.path()).unwrap());
    let agent = engine(store.clone(), provider.clone());
    let mut options = SessionOptions::new(repo.path().into(), ModelProfile::new("fake", "fixture"));
    options.checks = vec!["test \"$(cat value.txt)\" = new".into()];
    let session = agent.create_session(options).await.unwrap();
    let attachment = session.attach().await.unwrap();
    session
        .submit("Change old to new and verify it".into())
        .await
        .unwrap();
    let (status, events) = finish(&session, attachment.events, true).await;
    assert_eq!(status, RunStatus::Completed);
    assert_eq!(
        std::fs::read_to_string(repo.path().join("value.txt")).unwrap(),
        "new\n"
    );
    assert!(events
        .iter()
        .any(|e| matches!(e.kind, EventKind::ApprovalRequired(_))));
    assert_eq!(
        session.snapshot().await.unwrap().validation,
        ValidationStatus::Passed
    );
    assert!(provider.requests.lock().unwrap().iter().any(|r| r
        .messages
        .iter()
        .any(|m| m.tool_results.iter().any(|t| t.call_id == "r"))));
    let saved = session.snapshot().await.unwrap();
    let reopened = engine(store, provider)
        .open_session(&saved.id)
        .await
        .unwrap();
    assert_eq!(reopened.snapshot().await.unwrap().cursor, saved.cursor);
    let events = reopened.attach().await.unwrap().events;
    reopened.undo().await.unwrap();
    assert_eq!(
        finish(&reopened, events, true).await.0,
        RunStatus::Completed
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("value.txt")).unwrap(),
        "old\n"
    );
}

#[tokio::test]
async fn denied_patch_and_plan_mode_leave_bytes_unchanged() {
    for plan in [false, true] {
        let repo = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::write(repo.path().join("file"), "original").unwrap();
        let provider = Arc::new(FakeProvider::new(vec![
            response(vec![call(
                "w",
                "apply_patch",
                json!({"path":"file","content":"changed"}),
            )]),
            response(vec![]),
        ]));
        let agent = engine(
            Arc::new(SqliteStore::open(data.path()).unwrap()),
            provider.clone(),
        );
        let mut options =
            SessionOptions::new(repo.path().into(), ModelProfile::new("fake", "fixture"));
        if plan {
            options.policy.mode = Mode::Plan;
        }
        let session = agent.create_session(options).await.unwrap();
        let events = session.attach().await.unwrap().events;
        session.submit("Edit file".into()).await.unwrap();
        let (_, log) = finish(&session, events, false).await;
        assert_eq!(
            std::fs::read_to_string(repo.path().join("file")).unwrap(),
            "original"
        );
        assert!(!log
            .iter()
            .any(|e| matches!(e.kind, EventKind::ToolStarted { .. })));
        if plan {
            assert!(provider.requests.lock().unwrap()[0]
                .tools
                .iter()
                .all(|t| t.effect == Effect::Read));
        }
    }
}

#[tokio::test]
async fn stale_approval_fails_and_cannot_be_answered_twice() {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("file"), "original").unwrap();
    let agent = engine(
        Arc::new(SqliteStore::open(data.path()).unwrap()),
        Arc::new(FakeProvider::new(vec![])),
    );
    let session = agent
        .create_session(SessionOptions::new(
            repo.path().into(),
            ModelProfile::new("fake", "fixture"),
        ))
        .await
        .unwrap();
    let mut events = session.attach().await.unwrap().events;
    session
        .run_tool("apply_patch", json!({"path":"file","content":"agent"}))
        .await
        .unwrap();
    loop {
        if let EventKind::ApprovalRequired(request) = events.recv().await.unwrap().kind {
            std::fs::write(repo.path().join("file"), "human").unwrap();
            session
                .answer_approval(&request.id, ApprovalDecision::ApproveOnce)
                .await
                .unwrap();
            assert!(session
                .answer_approval(&request.id, ApprovalDecision::ApproveOnce)
                .await
                .is_err());
            break;
        }
    }
    assert_eq!(finish(&session, events, true).await.0, RunStatus::Failed);
    assert_eq!(
        std::fs::read_to_string(repo.path().join("file")).unwrap(),
        "human"
    );
}

#[tokio::test]
async fn approval_is_cancellable_and_submit_is_busy() {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let agent = engine(
        Arc::new(SqliteStore::open(data.path()).unwrap()),
        Arc::new(FakeProvider::new(vec![])),
    );
    let session = agent
        .create_session(SessionOptions::new(
            repo.path().into(),
            ModelProfile::new("fake", "fixture"),
        ))
        .await
        .unwrap();
    let mut events = session.attach().await.unwrap().events;
    session
        .run_tool("run_command", json!({"command":"touch should-not-exist"}))
        .await
        .unwrap();
    loop {
        if matches!(
            events.recv().await.unwrap().kind,
            EventKind::ApprovalRequired(_)
        ) {
            break;
        }
    }
    assert!(matches!(session.run_checks().await, Err(Error::Busy)));
    let attachment = session.attach().await.unwrap();
    assert!(attachment.snapshot.pending_approval.is_some());
    session.cancel().await.unwrap();
    assert_eq!(
        finish(&session, events, false).await.0,
        RunStatus::Cancelled
    );
    assert!(!repo.path().join("should-not-exist").exists());
}

#[tokio::test]
async fn reopen_unknown_action_closes_protocol_without_reexecution() {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let store = Arc::new(SqliteStore::open(data.path()).unwrap());
    let provider = Arc::new(FakeProvider::new(vec![response(vec![])]));
    let agent = engine(store.clone(), provider.clone());
    let session = agent
        .create_session(SessionOptions::new(
            repo.path().into(),
            ModelProfile::new("fake", "fixture"),
        ))
        .await
        .unwrap();
    let id = session.snapshot().await.unwrap().id;
    let mut state = store.load(&id).await.unwrap();
    let command = call(
        "unknown",
        "run_command",
        json!({"command":"touch repeated"}),
    );
    let mut assistant = Message::text(Role::Assistant, "");
    assistant.tool_calls = vec![command.clone()];
    state.messages.push(assistant);
    state.status = RunStatus::Running;
    state.unfinished_call = Some(command);
    store.commit(&state, &[]).await.unwrap();
    let recovered = engine(store, provider).open_session(&id).await.unwrap();
    let snapshot = recovered.snapshot().await.unwrap();
    assert_eq!(snapshot.status, RunStatus::Interrupted);
    assert!(!repo.path().join("repeated").exists());
    assert!(snapshot.messages.iter().any(|m| m
        .tool_results
        .iter()
        .any(|r| r.call_id == "unknown" && r.is_error)));
}

#[tokio::test]
async fn independent_sessions_cannot_run_in_same_workspace() {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let agent = engine(
        Arc::new(SqliteStore::open(data.path()).unwrap()),
        Arc::new(FakeProvider::new(vec![])),
    );
    let options = SessionOptions::new(repo.path().into(), ModelProfile::new("fake", "fixture"));
    let first = agent.create_session(options.clone()).await.unwrap();
    let second = agent.create_session(options).await.unwrap();
    let events = first.attach().await.unwrap().events;
    first
        .run_tool("run_command", json!({"command":"true"}))
        .await
        .unwrap();
    assert!(second
        .run_tool("read_file", json!({"path":"file"}))
        .await
        .is_err());
    first.cancel().await.unwrap();
    let _ = finish(&first, events, false).await;
}

#[tokio::test]
async fn compaction_retains_actual_requests_and_scoped_guidance() {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::create_dir(repo.path().join("src")).unwrap();
    std::fs::write(
        repo.path().join("src/AGENTS.md"),
        "Preserve the nested public interface.",
    )
    .unwrap();
    std::fs::write(repo.path().join("src/file"), "external observation").unwrap();
    let mut summary = response(vec![]);
    summary.text = "A concise summary without repeating scoped guidance.".into();
    let provider = Arc::new(FakeProvider::new(vec![
        response(vec![call("read", "read_file", json!({"path":"src/file"}))]),
        response(vec![]),
        response(vec![]),
        summary,
        response(vec![]),
    ]));
    let store = Arc::new(SqliteStore::open(data.path()).unwrap());
    let session = engine(store.clone(), provider.clone())
        .create_session(SessionOptions::new(
            repo.path().into(),
            ModelProfile::new("fake", "fixture"),
        ))
        .await
        .unwrap();
    for prompt in ["Keep compatibility. Inspect src/file", "Explain the result"] {
        let events = session.attach().await.unwrap().events;
        session.submit(prompt.into()).await.unwrap();
        assert_eq!(finish(&session, events, true).await.0, RunStatus::Completed);
    }
    let events = session.attach().await.unwrap().events;
    session.compact().await.unwrap();
    let (status, compact_events) = finish(&session, events, true).await;
    assert_eq!(status, RunStatus::Completed);
    assert!(compact_events.iter().any(|event| matches!(
        event.kind,
        EventKind::ResponseStarted {
            purpose: ResponsePurpose::Compaction,
            ..
        }
    )));
    assert!(compact_events.iter().any(|event| matches!(
        event.kind,
        EventKind::ResponseCompleted {
            purpose: ResponsePurpose::Compaction,
            ..
        }
    )));
    assert!(!compact_events.iter().any(|event| matches!(
        event.kind,
        EventKind::TextDelta { .. }
            | EventKind::ReasoningSummary { .. }
            | EventKind::AssistantMessage { .. }
    )));
    assert!(compact_events.iter().any(|event| matches!(&event.kind, EventKind::RunOutput { response_id: None, text } if text == "Context compacted.")));
    let events = session.attach().await.unwrap().events;
    session.submit("Continue".into()).await.unwrap();
    assert_eq!(finish(&session, events, true).await.0, RunStatus::Completed);
    {
        let requests = provider.requests.lock().unwrap();
        let last = requests.last().unwrap();
        assert!(last
            .instructions
            .contains("Preserve the nested public interface"));
        assert!(last
            .messages
            .iter()
            .any(|m| m.text.contains("Keep compatibility")));
    }
    let state = store
        .load(&session.snapshot().await.unwrap().id)
        .await
        .unwrap();
    assert_eq!(state.user_constraints.len(), 3);
    assert!(state
        .user_constraints
        .iter()
        .all(|s| !s.contains("external observation")));
}

#[tokio::test]
async fn changing_only_profile_budgets_is_applied() {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let session = engine(
        Arc::new(SqliteStore::open(data.path()).unwrap()),
        Arc::new(FakeProvider::new(vec![])),
    )
    .create_session(SessionOptions::new(
        repo.path().into(),
        ModelProfile::new("fake", "fixture"),
    ))
    .await
    .unwrap();
    let mut profile = session.snapshot().await.unwrap().profile;
    profile.context_window = 64_000;
    profile.max_output_tokens = 1024;
    session.set_profile(profile.clone()).await.unwrap();
    assert_eq!(session.snapshot().await.unwrap().profile, profile);
}

struct BurstProvider;
#[async_trait]
impl Provider for BurstProvider {
    fn id(&self) -> &str {
        "burst"
    }
    async fn complete(
        &self,
        _: ProviderRequest,
        events: mpsc::Sender<ProviderEvent>,
        _: CancellationToken,
    ) -> Result<ProviderResponse> {
        for _ in 0..350 {
            let _ = events
                .send(ProviderEvent::TextDelta("x".repeat(4096)))
                .await;
        }
        Ok(response(vec![]))
    }
}
#[tokio::test]
async fn lagged_observer_recovers_all_lifecycle_events_from_journal() {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let store = Arc::new(SqliteStore::open(data.path()).unwrap());
    let agent = Engine::new(EngineDependencies {
        providers: vec![Arc::new(BurstProvider)],
        tools: vec![],
        store,
    })
    .unwrap();
    let session = agent
        .create_session(SessionOptions::new(
            repo.path().into(),
            ModelProfile::new("burst", "fixture"),
        ))
        .await
        .unwrap();
    let mut observer = session.attach().await.unwrap().events;
    session.submit("Emit a burst".into()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while session.snapshot().await.unwrap().status.is_active() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        observer.recv().await,
        Err(tokio::sync::broadcast::error::RecvError::Lagged(_))
    ));
    let events = session.events_after(0, 4096).await.unwrap();
    assert!(matches!(
        events.first().unwrap().kind,
        EventKind::RunStarted { .. }
    ));
    assert!(matches!(
        events.last().unwrap().kind,
        EventKind::RunFinished {
            status: RunStatus::Completed,
            ..
        }
    ));
    assert!(events
        .windows(2)
        .all(|pair| pair[1].sequence == pair[0].sequence + 1));
}

#[tokio::test]
async fn inspection_does_not_run_configured_mutating_checks() {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let provider = Arc::new(FakeProvider::new(vec![response(vec![])]));
    let mut options = SessionOptions::new(repo.path().into(), ModelProfile::new("fake", "fixture"));
    options.checks = vec!["touch unexpected".into()];
    options.policy.allowed_commands = options.checks.clone();
    let session = engine(Arc::new(SqliteStore::open(data.path()).unwrap()), provider)
        .create_session(options)
        .await
        .unwrap();
    let events = session.attach().await.unwrap().events;
    session
        .submit("Explain without changes".into())
        .await
        .unwrap();
    let (status, events) = finish(&session, events, false).await;
    assert_eq!(status, RunStatus::Completed);
    assert!(!repo.path().join("unexpected").exists());
    assert!(!events
        .iter()
        .any(|e| matches!(e.kind, EventKind::ToolStarted { .. })));
}

#[tokio::test]
async fn final_queued_stream_delta_is_persisted_before_completed_message() {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let mut answer = response(vec![]);
    answer.reasoning = vec!["Authoritative visible summary".into()];
    let provider = Arc::new(FakeProvider::new(vec![answer]));
    let session = engine(Arc::new(SqliteStore::open(data.path()).unwrap()), provider)
        .create_session(SessionOptions::new(
            repo.path().into(),
            ModelProfile::new("fake", "fixture"),
        ))
        .await
        .unwrap();
    let events = session.attach().await.unwrap().events;
    session.submit("Explain".into()).await.unwrap();
    let (status, live) = finish(&session, events, false).await;
    assert_eq!(status, RunStatus::Completed);
    let saved = session.events_after(0, 100).await.unwrap();
    for events in [live, saved] {
        let delta = events
            .iter()
            .position(|e| matches!(&e.kind, EventKind::TextDelta { text } if text == "Finished"))
            .expect("last progress delta must not be lost on provider completion");
        let message = events
            .iter()
            .position(
                |e| matches!(&e.kind, EventKind::AssistantMessage { text } if text == "Finished"),
            )
            .unwrap();
        assert!(delta < message);
        let (response_id, completed) = events
            .iter()
            .find_map(|event| match &event.kind {
                EventKind::ResponseCompleted {
                    id,
                    purpose: ResponsePurpose::Conversation,
                    text,
                    reasoning,
                    tool_calls,
                } => {
                    assert_eq!(text, "Finished");
                    assert_eq!(reasoning, &["Authoritative visible summary"]);
                    assert!(tool_calls.is_empty());
                    Some((id, event.sequence))
                }
                _ => None,
            })
            .unwrap();
        assert!(events.iter().any(|event| matches!(&event.kind, EventKind::ResponseStarted { id, purpose: ResponsePurpose::Conversation } if id == response_id) && event.sequence < completed));
        assert!(events.iter().any(|event| matches!(&event.kind, EventKind::RunOutput { response_id: Some(id), text } if id == response_id && text == "Finished") && event.sequence > completed));
        let mut transcript = Transcript::default();
        for event in &events {
            transcript.apply(event);
        }
        let run = transcript.runs().back().unwrap();
        assert_eq!(run.responses.len(), 1);
        assert!(run.responses[0].is_final);
        assert_eq!(
            run.responses[0].reasoning,
            ["Authoritative visible summary"]
        );
    }
}

#[tokio::test]
async fn explicit_local_tool_has_authoritative_output_without_model_response() {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("file"), "local output").unwrap();
    let session = engine(
        Arc::new(SqliteStore::open(data.path()).unwrap()),
        Arc::new(FakeProvider::new(vec![])),
    )
    .create_session(SessionOptions::new(
        repo.path().into(),
        ModelProfile::new("fake", "fixture"),
    ))
    .await
    .unwrap();
    let events = session.attach().await.unwrap().events;
    session
        .run_tool("read_file", json!({"path":"file"}))
        .await
        .unwrap();
    let (status, events) = finish(&session, events, true).await;
    assert_eq!(status, RunStatus::Completed);
    assert!(events.iter().any(|event| matches!(&event.kind, EventKind::RunOutput { response_id: None, text } if text.contains("local output"))));
    assert!(!events
        .iter()
        .any(|event| matches!(event.kind, EventKind::ResponseStarted { .. })));
}

#[tokio::test]
async fn failing_validation_retains_the_final_authoritative_explanation() {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let mut explanation = response(vec![]);
    explanation.text = "The edit is present, but validation failed.".into();
    let provider = Arc::new(FakeProvider::new(vec![
        response(vec![call(
            "edit",
            "apply_patch",
            json!({"path":"file","content":"new"}),
        )]),
        response(vec![]),
        explanation,
    ]));
    let mut options = SessionOptions::new(repo.path().into(), ModelProfile::new("fake", "fixture"));
    options.checks = vec!["false".into()];
    let session = engine(Arc::new(SqliteStore::open(data.path()).unwrap()), provider)
        .create_session(options)
        .await
        .unwrap();
    let events = session.attach().await.unwrap().events;
    session
        .submit("Make the edit and validate".into())
        .await
        .unwrap();
    let (status, events) = finish(&session, events, true).await;
    assert_eq!(status, RunStatus::Failed);
    let mut transcript = Transcript::default();
    for event in &events {
        transcript.apply(event);
    }
    let run = transcript.runs().back().unwrap();
    assert_eq!(
        run.output.as_deref(),
        Some("The edit is present, but validation failed.")
    );
    assert!(run.responses.last().unwrap().is_final);
    assert_eq!(run.validation, ValidationStatus::Failed);
    assert!(run.error.as_deref().unwrap().contains("failing validation"));
}
