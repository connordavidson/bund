//! Full-stack, deterministic smoke tests: native HTTP protocol -> engine ->
//! real workspace tools -> durable SQLite -> a reopened engine. No live LLMs.
use bund_core::*;
use bund_providers::{AnthropicProvider, OpenAiProvider};
use bund_store::SqliteStore;
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::{broadcast, mpsc},
    task::JoinHandle,
};

const CHECK: &str = "test \"$(cat value.txt)\" = new && printf 'CHECK_PASSED\\n'";

#[derive(Clone, Copy)]
enum Wire {
    OpenAi,
    Anthropic,
}

impl Wire {
    fn id(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
        }
    }
    fn provider(self) -> Arc<dyn Provider> {
        match self {
            Self::OpenAi => Arc::new(OpenAiProvider::new("fake-key".into())),
            Self::Anthropic => Arc::new(AnthropicProvider::new("fake-key".into())),
        }
    }
}

fn event(value: Value) -> String {
    format!(
        "event: {}\ndata: {value}\n\n",
        value["type"].as_str().unwrap()
    )
}

fn response(wire: Wire, turn: usize, tool: Option<(&str, Value)>) -> String {
    let text = if tool.is_some() {
        "Working."
    } else {
        "Verified current file contents."
    };
    match wire {
        Wire::OpenAi => {
            let mut output = vec![
                json!({"type":"reasoning","id":format!("rs_{turn}"),"summary":[],"encrypted_content":format!("opaque_{turn}")}),
                json!({"type":"message","id":format!("msg_{turn}"),"role":"assistant","status":"completed","content":[{"type":"output_text","text":text,"annotations":[]}]}),
            ];
            if let Some((name, args)) = tool {
                output.push(json!({"type":"function_call","id":format!("fc_{turn}"),"call_id":format!("call_{turn}"),"name":name,"arguments":args.to_string(),"status":"completed"}));
            }
            let mut stream = event(
                json!({"type":"response.created","response":{"id":format!("resp_{turn}"),"status":"in_progress"}}),
            );
            for (index, item) in output.iter().enumerate() {
                stream.push_str(&event(
                    json!({"type":"response.output_item.added","output_index":index,"item":item}),
                ));
                if item["type"] == "message" {
                    stream.push_str(&event(json!({"type":"response.output_text.delta","output_index":index,"item_id":item["id"],"delta":text})));
                }
                stream.push_str(&event(
                    json!({"type":"response.output_item.done","output_index":index,"item":item}),
                ));
            }
            stream.push_str(&event(json!({"type":"response.completed","response":{"id":format!("resp_{turn}"),"status":"completed","output":output,"usage":{"input_tokens":12,"output_tokens":11,"input_tokens_details":{"cached_tokens":3}}}})));
            stream
        }
        Wire::Anthropic => {
            let mut stream = event(
                json!({"type":"message_start","message":{"id":format!("msg_{turn}"),"role":"assistant","content":[],"usage":{"input_tokens":7,"output_tokens":1,"cache_read_input_tokens":3,"cache_creation_input_tokens":2}}}),
            );
            let mut content = vec![
                json!({"type":"thinking","thinking":"Brief summary.","signature":format!("signed_{turn}")}),
                json!({"type":"text","text":text}),
            ];
            let stop = if tool.is_some() {
                "tool_use"
            } else {
                "end_turn"
            };
            if let Some((name, args)) = tool {
                content.push(
                    json!({"type":"tool_use","id":format!("call_{turn}"),"name":name,"input":args}),
                );
            }
            for (index, block) in content.into_iter().enumerate() {
                stream.push_str(&event(
                    json!({"type":"content_block_start","index":index,"content_block":block}),
                ));
                stream.push_str(&event(json!({"type":"content_block_stop","index":index})));
            }
            stream.push_str(&event(json!({"type":"message_delta","delta":{"stop_reason":stop},"usage":{"output_tokens":11}})));
            stream.push_str(&event(json!({"type":"message_stop"})));
            stream
        }
    }
}

async fn fixture(wire: Wire) -> (String, mpsc::Receiver<Value>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let (sender, receiver) = mpsc::channel(16);
    let task = tokio::spawn(async move {
        let calls = [
            Some(("read_file", json!({"path":"value.txt"}))),
            Some(("apply_patch", json!({"path":"value.txt","content":"new\n"}))),
            None, // Engine now runs its configured host check.
            None, // Report the recorded check result.
            Some(("read_file", json!({"path":"value.txt"}))), // Reopened Plan run.
            None,
        ];
        for (turn, tool) in calls.into_iter().enumerate() {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let end = loop {
                let mut chunk = [0u8; 4096];
                let count = socket.read(&mut chunk).await.unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&chunk[..count]);
                if let Some(end) = bytes.windows(4).position(|value| value == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let headers = String::from_utf8_lossy(&bytes[..end]);
            let route = match wire {
                Wire::OpenAi => "POST /v1/responses ",
                Wire::Anthropic => "POST /v1/messages ",
            };
            assert!(headers.starts_with(route));
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .map(str::to_owned)
                })
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let mut remaining = vec![0u8; length.saturating_sub(bytes.len() - end)];
            socket.read_exact(&mut remaining).await.unwrap();
            bytes.extend_from_slice(&remaining);
            sender
                .send(serde_json::from_slice(&bytes[end..]).unwrap())
                .await
                .unwrap();
            let body = response(wire, turn, tool);
            let headers = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len());
            socket.write_all(headers.as_bytes()).await.unwrap();
            for chunk in body.as_bytes().chunks(13) {
                socket.write_all(chunk).await.unwrap();
                tokio::task::yield_now().await;
            }
        }
    });
    (endpoint, receiver, task)
}

fn engine(store: Arc<SqliteStore>, wire: Wire) -> Engine {
    Engine::new(EngineDependencies {
        providers: vec![wire.provider()],
        tools: bund_tools::built_in_tools(),
        store,
    })
    .unwrap()
}

async fn finish(
    session: &SessionHandle,
    store: &SqliteStore,
    mut events: broadcast::Receiver<SessionEvent>,
) -> Vec<SessionEvent> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut log = Vec::new();
        loop {
            let event = events.recv().await.unwrap();
            if let EventKind::ApprovalRequired(approval) = &event.kind {
                let saved = store.load(&event.session_id).await.unwrap();
                assert_eq!(saved.status, RunStatus::AwaitingApproval);
                assert_eq!(saved.pending_approval.as_ref().unwrap().id, approval.id);
                assert_eq!(
                    saved.pending_approval.as_ref().unwrap().fingerprint,
                    approval.fingerprint
                );
                assert!(!approval.preview.is_empty());
                session
                    .answer_approval(&approval.id, ApprovalDecision::ApproveOnce)
                    .await
                    .unwrap();
            }
            let finished = if let EventKind::RunFinished {
                status, message, ..
            } = &event.kind
            {
                assert_eq!(*status, RunStatus::Completed, "{message}");
                true
            } else {
                false
            };
            log.push(event);
            if finished {
                return log;
            }
        }
    })
    .await
    .expect("complete workflow should finish")
}

fn native_values(wire: Wire, request: &Value) -> Vec<&Value> {
    match wire {
        Wire::OpenAi => request["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["type"] == "reasoning")
            .map(|item| &item["encrypted_content"])
            .collect(),
        Wire::Anthropic => request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|message| message["content"].as_array())
            .flatten()
            .filter(|block| block["type"] == "thinking")
            .map(|block| &block["signature"])
            .collect(),
    }
}

fn tool_results(wire: Wire, request: &Value) -> Vec<(&str, &str)> {
    match wire {
        Wire::OpenAi => request["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["type"] == "function_call_output")
            .map(|item| {
                (
                    item["call_id"].as_str().unwrap(),
                    item["output"].as_str().unwrap(),
                )
            })
            .collect(),
        Wire::Anthropic => request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|message| message["content"].as_array())
            .flatten()
            .filter(|block| block["type"] == "tool_result")
            .map(|block| {
                (
                    block["tool_use_id"].as_str().unwrap(),
                    block["content"].as_str().unwrap(),
                )
            })
            .collect(),
    }
}

async fn workflow(wire: Wire) {
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("value.txt"), "old\n").unwrap();
    std::fs::write(workspace.path().join("AGENTS.md"), "Keep changes focused.").unwrap();
    let (endpoint, mut requests, server) = fixture(wire).await;
    let store = Arc::new(SqliteStore::open(data.path()).unwrap());
    let initial_engine = engine(store.clone(), wire);
    let mut profile = ModelProfile::new(wire.id(), "fixture-model");
    profile.endpoint = Some(endpoint);
    let mut options = SessionOptions::new(workspace.path().into(), profile);
    options.checks = vec![CHECK.into()];
    let session = initial_engine.create_session(options).await.unwrap();
    let id = session.snapshot().await.unwrap().id;
    let attachment = session.attach().await.unwrap();
    session
        .submit("Inspect value.txt, replace old with new, and verify the change.".into())
        .await
        .unwrap();
    let first_log = finish(&session, &store, attachment.events).await;
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("value.txt")).unwrap(),
        "new\n"
    );
    let approvals: Vec<_> = first_log
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::ApprovalRequired(approval) => Some(approval.tool_call.name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(approvals, vec!["apply_patch", "run_command"]);
    let saved = store.load(&id).await.unwrap();
    assert_eq!(saved.validation, ValidationStatus::Passed);
    assert!(saved.pending_approval.is_none() && saved.unfinished_call.is_none());
    assert_eq!(saved.usage.input_tokens, 48);
    assert_eq!(saved.usage.output_tokens, 44);
    assert_eq!(saved.usage.cached_tokens, 12);
    let native: Vec<_> = saved
        .messages
        .iter()
        .filter_map(|message| message.continuation.as_ref())
        .collect();
    assert_eq!(native.len(), 4);
    for (turn, continuation) in native.iter().enumerate() {
        assert_eq!(continuation.identity, saved.options.profile.identity());
        let value: Value = serde_json::from_slice(&continuation.payload).unwrap();
        let field = match wire {
            Wire::OpenAi => "encrypted_content",
            Wire::Anthropic => "signature",
        };
        let prefix = match wire {
            Wire::OpenAi => "opaque",
            Wire::Anthropic => "signed",
        };
        assert_eq!(value[0][field], format!("{prefix}_{turn}"));
    }
    assert!(session
        .snapshot()
        .await
        .unwrap()
        .messages
        .iter()
        .all(|message| message.continuation.is_none()));
    // Drop all old engine/session handles, open a fresh SQLite connection, and
    // exercise the public Plan API while preserving the original native state.
    drop(session);
    drop(initial_engine);
    drop(store);
    let reopened_store = Arc::new(SqliteStore::open(data.path()).unwrap());
    let reopened_engine = engine(reopened_store.clone(), wire);
    let reopened = reopened_engine.open_session(&id).await.unwrap();
    reopened.set_mode(Mode::Plan).await.unwrap();
    let attachment = reopened.attach().await.unwrap();
    reopened
        .submit("Read the current file and explain its contents without changes.".into())
        .await
        .unwrap();
    let plan_log = finish(&reopened, &reopened_store, attachment.events).await;
    assert!(!plan_log
        .iter()
        .any(|event| matches!(event.kind, EventKind::ApprovalRequired(_))));
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("value.txt")).unwrap(),
        "new\n"
    );
    let final_state = reopened_store.load(&id).await.unwrap();
    assert_eq!(final_state.usage.input_tokens, 72);
    assert_eq!(final_state.usage.output_tokens, 66);
    assert_eq!(final_state.usage.cached_tokens, 18);
    for turn in 0..6 {
        let request = requests.recv().await.unwrap();
        let native = native_values(wire, &request);
        assert_eq!(native.len(), turn);
        for (past, item) in native.iter().enumerate() {
            let prefix = match wire {
                Wire::OpenAi => "opaque",
                Wire::Anthropic => "signed",
            };
            assert_eq!(**item, json!(format!("{prefix}_{past}")));
        }
        let results = tool_results(wire, &request);
        if turn > 0 {
            assert!(results
                .iter()
                .any(|(id, output)| *id == "call_0" && output.contains("old")));
        }
        if turn > 2 {
            assert!(results
                .iter()
                .any(|(_, output)| output.contains("CHECK_PASSED")));
        }
        if turn == 5 {
            assert!(results
                .iter()
                .any(|(id, output)| *id == "call_4" && output.contains("new")));
        }
        if turn >= 4 {
            let names: Vec<_> = request["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["name"].as_str().unwrap())
                .collect();
            assert!(!names.contains(&"apply_patch") && !names.contains(&"run_command"));
            assert!(names.contains(&"read_file"));
        }
    }
    server.await.unwrap();
    let persisted = reopened_store.events_after(&id, 0, 4096).await.unwrap();
    assert_eq!(persisted.len() as u64, final_state.cursor);
    for (index, event) in persisted.iter().enumerate() {
        assert_eq!(event.sequence, index as u64 + 1);
    }
    for call in final_state
        .messages
        .iter()
        .flat_map(|message| &message.tool_calls)
    {
        assert!(final_state
            .messages
            .iter()
            .flat_map(|message| &message.tool_results)
            .any(|result| result.call_id == call.id));
    }
}

#[tokio::test]
async fn openai_engine_tools_sqlite_roundtrip() {
    workflow(Wire::OpenAi).await;
}

#[tokio::test]
async fn anthropic_engine_tools_sqlite_roundtrip() {
    workflow(Wire::Anthropic).await;
}
