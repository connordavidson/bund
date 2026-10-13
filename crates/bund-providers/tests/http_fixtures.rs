//! No real API keys, model calls, or network access beyond a loopback fixture.
use bund_core::{
    Continuation, Effect, Error, Message, ModelProfile, Provider, ProviderErrorKind, ProviderEvent,
    ProviderRequest, ProviderResponse, Role, ToolDefinition, ToolResult,
};
use bund_providers::{AnthropicProvider, OpenAiProvider};
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::mpsc,
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

struct Reply {
    status: u16,
    headers: &'static str,
    body: String,
    stall: bool,
}

impl Reply {
    fn sse(body: String) -> Self {
        Self {
            status: 200,
            headers: "Content-Type: text/event-stream\r\n",
            body,
            stall: false,
        }
    }
    fn error(status: u16, headers: &'static str, body: Value) -> Self {
        Self {
            status,
            headers,
            body: body.to_string(),
            stall: false,
        }
    }
}

async fn server(replies: Vec<Reply>) -> (String, mpsc::Receiver<Value>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = mpsc::channel(32);
    let task = tokio::spawn(async move {
        for reply in replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let header_end = loop {
                let mut chunk = [0u8; 4096];
                let size = socket.read(&mut chunk).await.unwrap();
                assert!(size > 0);
                bytes.extend_from_slice(&chunk[..size]);
                if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            assert!(headers.starts_with("POST /v1/"));
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
            let mut remaining = vec![0u8; length.saturating_sub(bytes.len() - header_end)];
            socket.read_exact(&mut remaining).await.unwrap();
            bytes.extend_from_slice(&remaining);
            sender
                .send(serde_json::from_slice(&bytes[header_end..]).unwrap())
                .await
                .unwrap();
            let response_headers = format!(
                "HTTP/1.1 {} Fixture\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n",
                reply.status,
                reply.headers,
                reply.body.len() + usize::from(reply.stall)
            );
            socket.write_all(response_headers.as_bytes()).await.unwrap();
            // Split UTF-8/JSON/event boundaries independently of the parser.
            for chunk in reply.body.as_bytes().chunks(7) {
                if socket.write_all(chunk).await.is_err() {
                    break;
                }
                tokio::task::yield_now().await;
            }
            if reply.stall {
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        }
    });
    (format!("http://{address}/v1"), receiver, task)
}

fn event(value: Value) -> String {
    format!(
        "event: {}\r\ndata: {}\r\n\r\n",
        value["type"].as_str().unwrap(),
        value
    )
}

fn openai(turn: usize, calls: &[(&str, Value)], text: &str) -> String {
    let id = format!("resp_{turn}");
    let mut items = vec![
        json!({"type":"reasoning","id":format!("rs_{turn}"),"summary":[],"encrypted_content":format!("opaque_{turn}")}),
    ];
    if !text.is_empty() {
        items.push(json!({"type":"message","id":format!("msg_{turn}"),"role":"assistant","status":"completed", "content":[{"type":"output_text","text":text,"annotations":[]}]}));
    }
    for (index, (name, args)) in calls.iter().enumerate() {
        items.push(json!({"type":"function_call","id":format!("fc_{turn}_{index}"),"call_id":format!("call_{turn}_{index}"),"name":name,"arguments":args.to_string(),"status":"completed"}));
    }
    let mut stream =
        event(json!({"type":"response.created","response":{"id":id,"status":"in_progress"}}));
    for (index, item) in items.iter().enumerate() {
        let mut added = item.clone();
        if item["type"] == "function_call" {
            added["arguments"] = json!("");
            added["status"] = json!("in_progress");
        }
        stream.push_str(&event(
            json!({"type":"response.output_item.added","output_index":index,"item":added}),
        ));
        if item["type"] == "message" {
            stream.push_str(&event(json!({"type":"response.output_text.delta","output_index":index,"item_id":item["id"],"delta":text})));
        }
        if item["type"] == "function_call" {
            for part in item["arguments"].as_str().unwrap().as_bytes().chunks(3) {
                stream.push_str(&event(json!({"type":"response.function_call_arguments.delta","output_index":index,"item_id":item["id"],"delta":std::str::from_utf8(part).unwrap()})));
            }
        }
        stream.push_str(&event(
            json!({"type":"response.output_item.done","output_index":index,"item":item}),
        ));
    }
    stream.push_str(&event(json!({"type":"response.completed","response":{"id":id,"status":"completed","output":items,"usage":{"input_tokens":12,"output_tokens":11,"input_tokens_details":{"cached_tokens":3}}}})));
    stream
}

fn anthropic(turn: usize, calls: &[(&str, Value)], text: &str) -> String {
    let mut stream = event(
        json!({"type":"message_start","message":{"id":format!("msg_{turn}"),"role":"assistant","content":[],"usage":{"input_tokens":7,"output_tokens":1,"cache_read_input_tokens":3,"cache_creation_input_tokens":2}}}),
    );
    stream.push_str(&event(json!({"type":"ping"})));
    stream.push_str(&event(json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}})));
    stream.push_str(&event(json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"A brief summary."}})));
    stream.push_str(&event(json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":format!("signed_{turn}")}})));
    stream.push_str(&event(json!({"type":"content_block_stop","index":0})));
    let mut index = 1;
    if !text.is_empty() {
        stream.push_str(&event(json!({"type":"content_block_start","index":index,"content_block":{"type":"text","text":""}})));
        stream.push_str(&event(json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":text}})));
        stream.push_str(&event(json!({"type":"content_block_stop","index":index})));
        index += 1;
    }
    for (call_index, (name, args)) in calls.iter().enumerate() {
        stream.push_str(&event(json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":format!("call_{turn}_{call_index}"),"name":name,"input":{}}})));
        for part in args.to_string().as_bytes().chunks(3) {
            stream.push_str(&event(json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":std::str::from_utf8(part).unwrap()}})));
        }
        stream.push_str(&event(json!({"type":"content_block_stop","index":index})));
        index += 1;
    }
    stream.push_str(&event(json!({"type":"message_delta","delta":{"stop_reason":if calls.is_empty() {"end_turn"} else {"tool_use"}},"usage":{"output_tokens":11}})));
    stream.push_str(&event(json!({"type":"message_stop"})));
    stream
}

fn request(provider: &str, endpoint: &str) -> ProviderRequest {
    let mut profile = ModelProfile::new(provider, "fixture-model");
    profile.endpoint = Some(endpoint.to_owned());
    ProviderRequest {
        profile,
        instructions: "Follow the project instructions".into(),
        messages: vec![Message::text(Role::User, "Inspect, edit, and test")],
        tools: vec![
            tool("read_file", Effect::Read),
            tool("apply_patch", Effect::Write),
            tool("run_command", Effect::Execute),
        ],
    }
}

fn tool(name: &str, effect: Effect) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: "Fixture tool".into(),
        input_schema: json!({"type":"object","properties":{"value":{"type":"string"}},"required":["value"],"additionalProperties":false}),
        effect,
    }
}

async fn complete(
    provider: &dyn Provider,
    request: ProviderRequest,
) -> Result<ProviderResponse, Error> {
    let (events, _receiver) = mpsc::channel(128);
    tokio::time::timeout(
        Duration::from_secs(5),
        provider.complete(request, events, CancellationToken::new()),
    )
    .await
    .expect("fixture must terminate")
}

async fn inspect_edit_test(
    provider: &dyn Provider,
    fixture: fn(usize, &[(&str, Value)], &str) -> String,
) {
    let tools = ["read_file", "apply_patch", "run_command"];
    let mut replies: Vec<_> = tools
        .iter()
        .enumerate()
        .map(|(i, name)| {
            Reply::sse(fixture(
                i,
                &[(name, json!({"value":format!("step{i}")}))],
                "Working…",
            ))
        })
        .collect();
    replies.push(Reply::sse(fixture(3, &[], "All checks passed.")));
    let (endpoint, mut requests, server) = server(replies).await;
    let mut history = request(provider.id(), &endpoint);
    for (index, name) in tools.iter().enumerate() {
        let response = complete(provider, history.clone()).await.unwrap();
        assert_eq!(response.text, "Working…");
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, *name);
        assert_eq!(
            response.tool_calls[0].arguments,
            json!({"value":format!("step{index}")})
        );
        assert_eq!(response.usage.input_tokens, 12);
        assert_eq!(response.usage.output_tokens, 11);
        assert_eq!(response.usage.cached_tokens, 3);
        let mut assistant = Message::text(Role::Assistant, &response.text);
        assistant.tool_calls = response.tool_calls.clone();
        assistant.continuation = response.continuation;
        history.messages.push(assistant);
        let mut results = Message::text(Role::Tool, "");
        results.tool_results.push(ToolResult {
            call_id: response.tool_calls[0].id.clone(),
            name: (*name).into(),
            content: "ok".into(),
            is_error: false,
        });
        history.messages.push(results);
    }
    assert_eq!(
        complete(provider, history).await.unwrap().text,
        "All checks passed."
    );
    let first = requests.recv().await.unwrap();
    let second = requests.recv().await.unwrap();
    let third = requests.recv().await.unwrap();
    let fourth = requests.recv().await.unwrap();
    assert_eq!(first["stream"], true);
    if provider.id() == "openai" {
        assert_eq!(first["store"], false);
        assert_eq!(first["tools"][0]["strict"], false);
        assert_eq!(second["input"][1]["encrypted_content"], "opaque_0");
        assert_eq!(second["input"][4]["type"], "function_call_output");
        assert_eq!(second["input"][4]["call_id"], "call_0_0");
        assert_eq!(third["input"][5]["encrypted_content"], "opaque_1");
        assert_eq!(fourth["input"][9]["encrypted_content"], "opaque_2");
    } else {
        assert_eq!(second["messages"][1]["content"][0]["signature"], "signed_0");
        assert_eq!(second["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(
            second["messages"][2]["content"][0]["tool_use_id"],
            "call_0_0"
        );
        assert_eq!(third["messages"][3]["content"][0]["signature"], "signed_1");
        assert_eq!(fourth["messages"][5]["content"][0]["signature"], "signed_2");
    }
    server.await.unwrap();
}

#[tokio::test]
async fn openai_native_inspect_edit_test_roundtrip() {
    inspect_edit_test(&OpenAiProvider::new("fixture-key".into()), openai).await;
}

#[tokio::test]
async fn anthropic_native_inspect_edit_test_roundtrip() {
    inspect_edit_test(&AnthropicProvider::new("fixture-key".into()), anthropic).await;
}

#[tokio::test]
async fn both_reject_missing_terminal_even_after_complete_tools() {
    for (provider, fixture, terminal) in [
        (
            Arc::new(OpenAiProvider::new("fixture-key".into())) as Arc<dyn Provider>,
            openai as fn(usize, &[(&str, Value)], &str) -> String,
            "event: response.completed",
        ),
        (
            Arc::new(AnthropicProvider::new("fixture-key".into())) as Arc<dyn Provider>,
            anthropic as fn(usize, &[(&str, Value)], &str) -> String,
            "event: message_stop",
        ),
    ] {
        let stream = fixture(0, &[("apply_patch", json!({"value":"change"}))], "");
        let truncated = stream.split(terminal).next().unwrap().to_owned();
        let (endpoint, _requests, task) = server(vec![Reply::sse(truncated)]).await;
        assert!(matches!(
            complete(provider.as_ref(), request(provider.id(), &endpoint)).await,
            Err(Error::Provider {
                kind: ProviderErrorKind::Protocol,
                ..
            })
        ));
        task.await.unwrap();
    }
}

#[tokio::test]
async fn both_reject_malformed_arguments_and_truncation() {
    let malformed = anthropic(0, &[("apply_patch", json!({"value":"x"}))], "")
        .replace("\"partial_json\":\"{\\\"v\"", "\"partial_json\":\"!\\\"v\"");
    let output = openai(0, &[("apply_patch", json!({"value":"x"}))], "").replace(
        "\"arguments\":\"{\\\"value\\\":\\\"x\\\"}\"",
        "\"arguments\":\"not json\"",
    );
    for (provider, stream) in [
        (
            Arc::new(OpenAiProvider::new("fixture-key".into())) as Arc<dyn Provider>,
            output,
        ),
        (
            Arc::new(AnthropicProvider::new("fixture-key".into())) as Arc<dyn Provider>,
            malformed,
        ),
    ] {
        let (endpoint, _requests, task) = server(vec![Reply::sse(stream)]).await;
        assert!(matches!(
            complete(provider.as_ref(), request(provider.id(), &endpoint)).await,
            Err(Error::Provider {
                kind: ProviderErrorKind::Protocol,
                ..
            })
        ));
        task.await.unwrap();
    }
    let stream = anthropic(0, &[("apply_patch", json!({"value":"x"}))], "").replace(
        "\"stop_reason\":\"tool_use\"",
        "\"stop_reason\":\"max_tokens\"",
    );
    let (endpoint, _requests, task) = server(vec![Reply::sse(stream)]).await;
    assert!(matches!(
        complete(
            &AnthropicProvider::new("fixture-key".into()),
            request("anthropic", &endpoint)
        )
        .await,
        Err(Error::Provider {
            kind: ProviderErrorKind::Context,
            ..
        })
    ));
    task.await.unwrap();
}

#[tokio::test]
async fn rate_limit_retries_but_quota_does_not() {
    let (endpoint, mut requests, task) = server(vec![
        Reply::error(
            429,
            "Retry-After: 0\r\n",
            json!({"error":{"type":"rate_limit_error","message":"slow down"}}),
        ),
        Reply::sse(openai(0, &[], "done")),
    ])
    .await;
    let (events, mut receiver) = mpsc::channel(128);
    OpenAiProvider::new("fixture-key".into())
        .complete(
            request("openai", &endpoint),
            events,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(matches!(
        receiver.recv().await,
        Some(ProviderEvent::Retrying { attempt: 1 })
    ));
    requests.recv().await.unwrap();
    requests.recv().await.unwrap();
    task.await.unwrap();
    let (endpoint, mut requests, task) = server(vec![Reply::error(
        429,
        "",
        json!({"error":{"code":"insufficient_quota","message":"fixture-key exhausted"}}),
    )])
    .await;
    let error = complete(
        &OpenAiProvider::new("fixture-key".into()),
        request("openai", &endpoint),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        Error::Provider {
            kind: ProviderErrorKind::Quota,
            ..
        }
    ));
    assert!(!error.to_string().contains("fixture-key"));
    requests.recv().await.unwrap();
    task.await.unwrap();
    assert!(requests.recv().await.is_none());
}

#[tokio::test]
async fn cancellation_interrupts_stream_and_backoff() {
    for reply in [
        Reply {
            status: 200,
            headers: "Content-Type: text/event-stream\r\n",
            body: event(
                json!({"type":"response.created","response":{"id":"r","status":"in_progress"}}),
            ),
            stall: true,
        },
        Reply::error(
            429,
            "Retry-After: 30\r\n",
            json!({"error":{"type":"rate_limit_error"}}),
        ),
    ] {
        let (endpoint, mut requests, task) = server(vec![reply]).await;
        let cancel = CancellationToken::new();
        let child = cancel.clone();
        let (events, _receiver) = mpsc::channel(16);
        let operation = tokio::spawn(async move {
            OpenAiProvider::new("fixture-key".into())
                .complete(request("openai", &endpoint), events, child)
                .await
        });
        requests.recv().await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), operation)
                .await
                .unwrap()
                .unwrap(),
            Err(Error::Cancelled)
        ));
        task.abort();
    }
}

#[tokio::test]
async fn identities_and_versions_cannot_cross_provider_boundaries() {
    for provider in [
        Arc::new(OpenAiProvider::new("fixture-key".into())) as Arc<dyn Provider>,
        Arc::new(AnthropicProvider::new("fixture-key".into())) as Arc<dyn Provider>,
    ] {
        let mut req = request(provider.id(), "http://127.0.0.1:1/v1");
        let mut assistant = Message::text(Role::Assistant, "previous");
        assistant.continuation = Some(Continuation {
            identity: "another-provider".into(),
            format_version: 1,
            payload: b"[]".to_vec(),
        });
        req.messages.push(assistant);
        assert!(matches!(
            complete(provider.as_ref(), req).await,
            Err(Error::Provider {
                kind: ProviderErrorKind::Unsupported,
                ..
            })
        ));
    }
}

#[tokio::test]
async fn in_stream_errors_do_not_retry_or_authorize_tools() {
    for (provider, mut stream) in [
        (
            Arc::new(OpenAiProvider::new("fixture-key".into())) as Arc<dyn Provider>,
            event(json!({"type":"response.created","response":{"id":"r"}})),
        ),
        (
            Arc::new(AnthropicProvider::new("fixture-key".into())) as Arc<dyn Provider>,
            event(
                json!({"type":"message_start","message":{"id":"m","role":"assistant","content":[],"usage":{"input_tokens":1,"output_tokens":1}}}),
            ),
        ),
    ] {
        stream.push_str(&event(
            json!({"type":"error","error":{"type":"overloaded_error","message":"unavailable"}}),
        ));
        let (endpoint, mut requests, task) = server(vec![Reply::sse(stream)]).await;
        assert!(matches!(
            complete(provider.as_ref(), request(provider.id(), &endpoint)).await,
            Err(Error::Provider {
                kind: ProviderErrorKind::Transport,
                ..
            })
        ));
        requests.recv().await.unwrap();
        task.await.unwrap();
        assert!(requests.recv().await.is_none());
    }
}

#[tokio::test]
async fn cancellation_before_request_spends_nothing() {
    let token = CancellationToken::new();
    token.cancel();
    let (events, _receiver) = mpsc::channel(1);
    let result = OpenAiProvider::new("fixture-key".into())
        .complete(request("openai", "http://127.0.0.1:1/v1"), events, token)
        .await;
    assert!(matches!(result, Err(Error::Cancelled)));
}
