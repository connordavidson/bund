use super::*;
use bund_core::{
    Engine, EngineDependencies, ModelProfile, ResponsePurpose, SessionOptions, SessionStore,
    ValidationStatus,
};
use std::sync::Arc;

struct Fixture {
    _workspace: tempfile::TempDir,
    _data: tempfile::TempDir,
    store: Arc<bund_store::SqliteStore>,
    handle: SessionHandle,
}

async fn fixture() -> Fixture {
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let store = Arc::new(bund_store::SqliteStore::open(data.path()).unwrap());
    let engine = Engine::new(EngineDependencies {
        providers: vec![],
        tools: bund_tools::built_in_tools(),
        store: store.clone(),
    })
    .unwrap();
    let handle = engine
        .create_session(SessionOptions::new(
            workspace.path().into(),
            ModelProfile::new("fixture", "offline"),
        ))
        .await
        .unwrap();
    Fixture {
        _workspace: workspace,
        _data: data,
        store,
        handle,
    }
}

async fn screen(fixture: &Fixture) -> Screen {
    Screen::new(
        fixture.handle.snapshot().await.unwrap(),
        History {
            path: fixture._data.path().join("history"),
            entries: vec![],
            position: None,
            draft: String::new(),
        },
    )
}

async fn save_runs(fixture: &Fixture, count: usize, settings_first: bool) -> Vec<SessionEvent> {
    let id = fixture.handle.snapshot().await.unwrap().id;
    let mut stored = fixture.store.load(&id).await.unwrap();
    let mut events = Vec::new();
    if settings_first {
        events.push(SessionEvent {
            version: 1,
            session_id: id.clone(),
            sequence: 1,
            run_id: None,
            kind: EventKind::SettingsChanged,
        });
    }
    for n in 0..count {
        let response_id = format!("response-{n}");
        let answer = "x".repeat(65_536);
        let kinds = [
            EventKind::RunStarted {
                prompt: format!("Prompt {n}"),
            },
            EventKind::ResponseStarted {
                id: response_id.clone(),
                purpose: ResponsePurpose::Conversation,
            },
            EventKind::ResponseCompleted {
                id: response_id.clone(),
                purpose: ResponsePurpose::Conversation,
                text: answer.clone(),
                reasoning: vec!["r".repeat(65_536)],
                tool_calls: vec![],
            },
            EventKind::RunOutput {
                response_id: Some(response_id),
                text: answer.clone(),
            },
            EventKind::RunFinished {
                status: RunStatus::Completed,
                message: answer,
                validation: ValidationStatus::NotRun,
            },
        ];
        for kind in kinds {
            events.push(SessionEvent {
                version: 1,
                session_id: id.clone(),
                sequence: events.len() as u64 + 1,
                run_id: Some(format!("run-{n}")),
                kind,
            });
        }
    }
    stored.cursor = events.len() as u64;
    fixture.store.commit(&stored, &events).await.unwrap();
    events
}

async fn finish_load(screen: &mut Screen) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while screen
            .history_task
            .as_ref()
            .is_some_and(|task| !task.is_finished())
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn forward_pages_over_byte_limit_never_skip_adjacent_runs() {
    let fixture = fixture().await;
    let events = save_runs(&fixture, 8, false).await;
    let latest = events.last().unwrap().sequence;
    let mut after = 0;
    let mut visited = Vec::new();
    while after < latest {
        let loaded = forward_history(&fixture.handle, after, latest)
            .await
            .unwrap();
        assert!(loaded.ceiling > after);
        assert!(
            loaded.transcript.runs().len() < 8,
            "fixture must exceed byte capacity before run capacity"
        );
        visited.extend(loaded.transcript.runs().iter().map(|run| run.id.clone()));
        assert!(loaded
            .transcript
            .runs()
            .iter()
            .all(|run| run.status == RunStatus::Completed));
        after = loaded.ceiling;
    }
    assert_eq!(
        visited,
        (0..8).map(|n| format!("run-{n}")).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn first_scroll_freezes_the_window_against_live_eviction() {
    let fixture = fixture().await;
    let events = save_runs(&fixture, 2, false).await;
    let mut screen = screen(&fixture).await;
    for event in &events {
        screen.transcript.apply(event);
    }
    screen.top = 10;
    screen.viewport_height = 5;
    let before = screen.transcript.cursor();
    page_history(&fixture.handle, &mut screen, false)
        .await
        .unwrap();
    assert_eq!(screen.history_ceiling, Some(before));
    assert!(!screen.follow);
    for sequence in before + 1..before + 100 {
        apply_event(
            &fixture.handle,
            &mut screen,
            SessionEvent {
                version: 1,
                session_id: "fixture".into(),
                sequence,
                run_id: Some(format!("new-{sequence}")),
                kind: EventKind::RunStarted {
                    prompt: "New work".into(),
                },
            },
        )
        .await
        .unwrap();
    }
    assert_eq!(screen.transcript.cursor(), before);
    assert_eq!(screen.transcript.runs().front().unwrap().id, "run-0");
    assert_eq!(
        screen.snapshot.status,
        RunStatus::Running,
        "live status stays independent of history"
    );
}

#[tokio::test]
async fn settings_before_first_run_do_not_replace_history_with_an_empty_window() {
    let fixture = fixture().await;
    let events = save_runs(&fixture, 1, true).await;
    let mut screen = screen(&fixture).await;
    for event in &events {
        screen.transcript.apply(event);
    }
    page_history(&fixture.handle, &mut screen, false)
        .await
        .unwrap();
    finish_load(&mut screen).await;
    poll_history(&fixture.handle, &mut screen, events.len() as u64)
        .await
        .unwrap();
    assert_eq!(screen.transcript.runs().len(), 1);
    assert_eq!(screen.transcript.runs()[0].id, "run-0");
}

#[tokio::test]
async fn latest_load_reconciles_events_received_while_background_replay_was_running() {
    let fixture = fixture().await;
    let events = save_runs(&fixture, 2, false).await;
    let mut screen = screen(&fixture).await;
    let old = replay(&fixture.handle, 3).await.unwrap();
    start_history_load(&mut screen, async move {
        Ok(HistoryLoad {
            transcript: old,
            ceiling: 3,
            target: HistoryTarget::Latest,
        })
    });
    finish_load(&mut screen).await;
    poll_history(&fixture.handle, &mut screen, events.len() as u64)
        .await
        .unwrap();
    assert!(screen.history_ceiling.is_none());
    assert!(screen.follow);
    assert_eq!(screen.transcript.cursor(), events.len() as u64);
    assert_eq!(
        screen.transcript.runs().back().unwrap().status,
        RunStatus::Completed
    );
}

#[tokio::test]
async fn cancellation_and_drafts_remain_responsive_while_history_is_loading() {
    let fixture = fixture().await;
    let mut events = fixture.handle.attach().await.unwrap().events;
    fixture
        .handle
        .run_tool("run_command", serde_json::json!({"command":"sleep 5"}))
        .await
        .unwrap();
    let mut screen = screen(&fixture).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = events.recv().await.unwrap();
            let approval = matches!(event.kind, EventKind::ApprovalRequired(_));
            apply_event(&fixture.handle, &mut screen, event)
                .await
                .unwrap();
            if approval {
                break;
            }
        }
    })
    .await
    .unwrap();
    screen.set_draft("keep this draft");
    start_history_load(&mut screen, std::future::pending());
    tokio::time::timeout(
        Duration::from_millis(100),
        key_action(
            &fixture.handle,
            &mut screen,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(screen.draft(), "keep this draft");
    assert!(screen
        .history_task
        .as_ref()
        .is_some_and(|task| !task.is_finished()));
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let EventKind::RunFinished { status, .. } = events.recv().await.unwrap().kind {
                assert_eq!(status, RunStatus::Cancelled);
                break;
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn forward_from_a_frozen_active_run_reloads_its_complete_context() {
    let fixture = fixture().await;
    let events = save_runs(&fixture, 2, false).await;
    let id = fixture.handle.snapshot().await.unwrap().id;
    let handle = Engine::new(EngineDependencies {
        providers: vec![],
        tools: vec![],
        store: fixture.store.clone(),
    })
    .unwrap()
    .open_session(&id)
    .await
    .unwrap();
    let mut screen = screen(&fixture).await;
    for event in events.iter().take(3) {
        screen.transcript.apply(event);
    }
    screen.history_ceiling = Some(3);
    screen.follow = false;
    page_history(&handle, &mut screen, true).await.unwrap();
    finish_load(&mut screen).await;
    poll_history(&handle, &mut screen, events.len() as u64)
        .await
        .unwrap();
    let run = &screen.transcript.runs()[0];
    assert_eq!(run.id, "run-0");
    assert_eq!(run.prompt, "Prompt 0");
    assert_eq!(run.start_sequence, 1);
    assert_eq!(run.status, RunStatus::Completed);
    assert!(run.responses[0].is_final);
}
