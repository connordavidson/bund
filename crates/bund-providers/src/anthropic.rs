use crate::{
    continuation, failure, native_items, protocol, required_str,
    transport::{stream_error, Transport, WireProtocol},
    usage_number, validate_calls, validate_request,
};
use async_trait::async_trait;
use bund_core::{
    Provider, ProviderErrorKind, ProviderEvent, ProviderRequest, ProviderResponse, Result, Role,
    ToolCall, Usage,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub struct AnthropicProvider {
    transport: Transport,
}

impl AnthropicProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            transport: Transport::new(api_key),
        }
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn id(&self) -> &str {
        "anthropic"
    }

    async fn complete(
        &self,
        request: ProviderRequest,
        events: mpsc::Sender<ProviderEvent>,
        cancel: CancellationToken,
    ) -> Result<ProviderResponse> {
        validate_request(&request, self.id())?;
        let body = request_body(&request)?;
        let mut state = State::default();
        self.transport
            .stream(
                request
                    .profile
                    .endpoint
                    .as_deref()
                    .unwrap_or("https://api.anthropic.com/v1"),
                WireProtocol::Anthropic,
                body,
                &events,
                &cancel,
                |event, data| state.accept(event, data, &events),
            )
            .await?;
        if cancel.is_cancelled() {
            return Err(bund_core::Error::Cancelled);
        }
        state.finish(&request)
    }
}

fn request_body(request: &ProviderRequest) -> Result<Value> {
    let mut messages = Vec::new();
    for message in &request.messages {
        match message.role {
            Role::User => {
                if message.continuation.is_some() {
                    return Err(protocol("user messages cannot contain native continuation"));
                }
                messages.push(json!({"role":"user", "content":message.text}));
            }
            Role::Assistant => {
                let content = if let Some(items) = native_items(message, &request.profile)? {
                    items
                } else {
                    let mut items = Vec::new();
                    if !message.text.is_empty() {
                        items.push(json!({"type":"text", "text":message.text}));
                    }
                    validate_calls(&message.tool_calls)?;
                    for call in &message.tool_calls {
                        items.push(json!({"type":"tool_use", "id":call.id, "name":call.name, "input":call.arguments}));
                    }
                    items
                };
                if content.is_empty() {
                    return Err(protocol(
                        "empty assistant history is not valid for Messages",
                    ));
                }
                messages.push(json!({"role":"assistant", "content":content}));
            }
            Role::Tool => {
                if message.continuation.is_some() {
                    return Err(protocol("tool messages cannot contain native continuation"));
                }
                let results: Vec<Value> = message
                    .tool_results
                    .iter()
                    .map(|result| {
                        json!({"type":"tool_result", "tool_use_id":result.call_id,
                    "content":result.content, "is_error":result.is_error})
                    })
                    .collect();
                if results.is_empty() {
                    return Err(protocol("tool history has no tool results"));
                }
                messages.push(json!({"role":"user", "content":results}));
            }
        }
    }
    let tools: Vec<Value> = request.tools.iter().map(|tool| json!({"name":tool.name,"description":tool.description,"input_schema":tool.input_schema})).collect();
    let mut body = json!({"model":request.profile.model, "system":request.instructions,"messages":messages,
        "tools":tools, "stream":true,"max_tokens":request.profile.max_output_tokens});
    if !request.tools.is_empty() {
        body["tool_choice"] = json!({"type":"auto","disable_parallel_tool_use":true});
    }
    if let Some(effort) = &request.profile.reasoning {
        // The model profile opts into adaptive thinking; legacy budget-based
        // thinking requires a future explicit capability rather than guessing.
        body["thinking"] = json!({"type":"adaptive", "display":"summarized"});
        body["output_config"] = json!({"effort":effort});
    }
    Ok(body)
}

struct Block {
    value: Value,
    arguments: String,
    saw_argument_delta: bool,
    done: bool,
}

#[derive(Default)]
struct State {
    started: bool,
    stopped: bool,
    blocks: BTreeMap<usize, Block>,
    stop_reason: Option<String>,
    usage: Option<Value>,
    saw_final_usage: bool,
}

impl State {
    fn accept(
        &mut self,
        event: &str,
        data: &str,
        events: &mpsc::Sender<ProviderEvent>,
    ) -> Result<()> {
        let value: Value =
            serde_json::from_str(data).map_err(|_| protocol("invalid JSON in Messages stream"))?;
        let kind = required_str(&value, "type")?;
        if !event.is_empty() && event != kind {
            return Err(protocol("SSE event name/type mismatch"));
        }
        if kind == "ping" {
            return Ok(());
        }
        if self.stopped {
            return Err(protocol("unexpected event after message_stop"));
        }
        if kind == "error" {
            return Err(stream_error(&value));
        }
        match kind {
            "message_start" => {
                if self.started {
                    return Err(protocol("duplicate message_start"));
                }
                let message = &value["message"];
                required_str(message, "id")?;
                if message["role"] != "assistant"
                    || !message["content"].as_array().is_some_and(Vec::is_empty)
                {
                    return Err(protocol("invalid message_start"));
                }
                self.started = true;
                self.usage = Some(message["usage"].clone());
            }
            "content_block_start" => {
                self.require_started()?;
                if self.stop_reason.is_some() {
                    return Err(protocol("content started after stop reason"));
                }
                let index = index(&value)?;
                if index != self.blocks.len() {
                    return Err(protocol("content block indices are not contiguous"));
                }
                let block = value["content_block"].clone();
                match required_str(&block, "type")? {
                    "text" | "thinking" | "redacted_thinking" | "tool_use" => {}
                    _ => {
                        return Err(failure(
                            ProviderErrorKind::Unsupported,
                            "unsupported Messages content block",
                        ))
                    }
                }
                self.blocks.insert(
                    index,
                    Block {
                        value: block,
                        arguments: String::new(),
                        saw_argument_delta: false,
                        done: false,
                    },
                );
            }
            "content_block_delta" => {
                let block = self
                    .blocks
                    .get_mut(&index(&value)?)
                    .ok_or_else(|| protocol("content delta before block start"))?;
                if block.done {
                    return Err(protocol("content delta after block stop"));
                }
                let delta = &value["delta"];
                match required_str(delta, "type")? {
                    "text_delta" => {
                        append(block, "text", "text", delta, "text")?;
                        let _ = events.try_send(ProviderEvent::TextDelta(
                            delta["text"].as_str().unwrap_or("").to_owned(),
                        ));
                    }
                    "thinking_delta" => {
                        append(block, "thinking", "thinking", delta, "thinking")?;
                        let _ = events.try_send(ProviderEvent::ReasoningSummary(
                            delta["thinking"].as_str().unwrap_or("").to_owned(),
                        ));
                    }
                    "signature_delta" => {
                        append(block, "thinking", "signature", delta, "signature")?
                    }
                    "input_json_delta" => {
                        if block.value["type"] != "tool_use" {
                            return Err(protocol("tool argument delta on a non-tool block"));
                        }
                        block.arguments.push_str(
                            delta["partial_json"]
                                .as_str()
                                .ok_or_else(|| protocol("invalid tool arguments delta"))?,
                        );
                        block.saw_argument_delta = true;
                    }
                    _ => {
                        return Err(failure(
                            ProviderErrorKind::Unsupported,
                            "unsupported Messages content delta",
                        ))
                    }
                }
            }
            "content_block_stop" => {
                let block = self
                    .blocks
                    .get_mut(&index(&value)?)
                    .ok_or_else(|| protocol("content stop before block start"))?;
                if block.done {
                    return Err(protocol("duplicate content block stop"));
                }
                if block.value["type"] == "tool_use" && block.saw_argument_delta {
                    block.value["input"] = serde_json::from_str(&block.arguments)
                        .map_err(|_| protocol("invalid completed tool argument JSON"))?;
                }
                block.done = true;
            }
            "message_delta" => {
                self.require_started()?;
                if self.blocks.values().any(|block| !block.done) {
                    return Err(protocol("message delta arrived before content completed"));
                }
                if let Some(reason) = value["delta"].get("stop_reason").filter(|v| !v.is_null()) {
                    let reason = reason
                        .as_str()
                        .ok_or_else(|| protocol("invalid stop reason"))?;
                    if self.stop_reason.replace(reason.to_owned()).is_some() {
                        return Err(protocol("duplicate stop reason"));
                    }
                }
                if let Some(update) = value.get("usage") {
                    let update = update
                        .as_object()
                        .ok_or_else(|| protocol("invalid usage update"))?;
                    let usage = self
                        .usage
                        .as_mut()
                        .and_then(Value::as_object_mut)
                        .ok_or_else(|| protocol("missing initial usage"))?;
                    // The API reports cumulative values, not per-event deltas.
                    for (key, value) in update {
                        usage.insert(key.clone(), value.clone());
                    }
                    self.saw_final_usage |= update.contains_key("output_tokens");
                }
            }
            "message_stop" => {
                self.require_started()?;
                if self.stop_reason.is_none()
                    || self.blocks.values().any(|block| !block.done)
                    || !self.saw_final_usage
                {
                    return Err(protocol(
                        "message_stop without complete content, stop reason, and usage",
                    ));
                }
                self.stopped = true;
            }
            _ => {} // Forward-compatible metadata never authorizes tools.
        }
        Ok(())
    }

    fn require_started(&self) -> Result<()> {
        if self.started {
            Ok(())
        } else {
            Err(protocol("Messages stream has not started"))
        }
    }

    fn finish(self, request: &ProviderRequest) -> Result<ProviderResponse> {
        if !self.stopped {
            return Err(protocol(
                "stream ended without message_stop; no tool calls accepted",
            ));
        }
        match self.stop_reason.as_deref() {
            Some("end_turn" | "tool_use") => {}
            Some("max_tokens" | "model_context_window_exceeded") => {
                return Err(failure(
                    ProviderErrorKind::Context,
                    "model output was truncated; no tool calls accepted",
                ))
            }
            Some("refusal") => {
                return Err(failure(
                    ProviderErrorKind::Unsupported,
                    "model declined the request",
                ))
            }
            _ => {
                return Err(failure(
                    ProviderErrorKind::Unsupported,
                    "unsupported Messages stop reason",
                ))
            }
        }
        let content: Vec<Value> = self.blocks.into_values().map(|block| block.value).collect();
        let mut text = String::new();
        let mut reasoning = Vec::new();
        let mut calls = Vec::new();
        for block in &content {
            match required_str(block, "type")? {
                "text" => text.push_str(
                    block["text"]
                        .as_str()
                        .ok_or_else(|| protocol("invalid text block"))?,
                ),
                "tool_use" => calls.push(ToolCall {
                    id: required_str(block, "id")?.to_owned(),
                    name: required_str(block, "name")?.to_owned(),
                    arguments: block["input"].clone(),
                }),
                "thinking" => {
                    required_str(block, "signature")?;
                    let summary = block["thinking"]
                        .as_str()
                        .ok_or_else(|| protocol("invalid thinking summary text"))?;
                    if !summary.trim().is_empty() {
                        reasoning.push(summary.to_owned());
                    }
                }
                "redacted_thinking" => {
                    required_str(block, "data")?;
                }
                _ => unreachable!("block types validated at start"),
            }
        }
        validate_calls(&calls)?;
        if (self.stop_reason.as_deref() == Some("tool_use")) == calls.is_empty() {
            return Err(protocol("stop reason does not match tool calls"));
        }
        let usage = self.usage.ok_or_else(|| protocol("missing token usage"))?;
        let cached_tokens = optional_usage(&usage, "cache_read_input_tokens")?;
        let cache_creation_tokens = optional_usage(&usage, "cache_creation_input_tokens")?;
        let input_tokens = usage_number(&usage, "input_tokens")?
            .checked_add(cached_tokens)
            .and_then(|value| value.checked_add(cache_creation_tokens))
            .ok_or_else(|| protocol("invalid or overflowing input token usage"))?;
        Ok(ProviderResponse {
            text,
            reasoning,
            tool_calls: calls,
            continuation: Some(continuation(&request.profile, &content)?),
            usage: Usage {
                input_tokens,
                output_tokens: usage_number(&usage, "output_tokens")?,
                cached_tokens,
            },
        })
    }
}

fn index(value: &Value) -> Result<usize> {
    value["index"]
        .as_u64()
        .and_then(|i| usize::try_from(i).ok())
        .filter(|i| *i < 1024)
        .ok_or_else(|| protocol("invalid content block index"))
}

fn append(
    block: &mut Block,
    expected: &str,
    target: &str,
    delta: &Value,
    field: &str,
) -> Result<()> {
    if block.value["type"] != expected {
        return Err(protocol("content delta type does not match block type"));
    }
    let delta = delta[field]
        .as_str()
        .ok_or_else(|| protocol("invalid string delta"))?;
    let Value::String(existing) = &mut block.value[target] else {
        return Err(protocol("invalid initial content field"));
    };
    existing.push_str(delta);
    Ok(())
}

fn optional_usage(usage: &Value, key: &str) -> Result<u64> {
    match usage.get(key) {
        Some(value) => value
            .as_u64()
            .ok_or_else(|| protocol(format!("invalid usage.{key}"))),
        None => Ok(0),
    }
}
