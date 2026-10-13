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

pub struct OpenAiProvider {
    transport: Transport,
}

impl OpenAiProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            transport: Transport::new(api_key),
        }
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn id(&self) -> &str {
        "openai"
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
                    .unwrap_or("https://api.openai.com/v1"),
                WireProtocol::OpenAi,
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
    let mut input = Vec::new();
    for message in &request.messages {
        match message.role {
            Role::User => {
                if message.continuation.is_some() {
                    return Err(protocol("user messages cannot contain native continuation"));
                }
                input.push(json!({"role":"user", "content":message.text}));
            }
            Role::Assistant => {
                if let Some(items) = native_items(message, &request.profile)? {
                    input.extend(items);
                } else {
                    if !message.text.is_empty() {
                        input.push(json!({"role":"assistant", "content":message.text}));
                    }
                    validate_calls(&message.tool_calls)?;
                    for call in &message.tool_calls {
                        input.push(json!({"type":"function_call", "call_id":call.id, "name":call.name, "arguments":serde_json::to_string(&call.arguments)?}));
                    }
                }
            }
            Role::Tool => {
                if message.continuation.is_some() {
                    return Err(protocol("tool messages cannot contain native continuation"));
                }
                for result in &message.tool_results {
                    input.push(json!({"type":"function_call_output", "call_id":result.call_id,
                        "output": if result.is_error { format!("Tool execution failed: {}", result.content) } else { result.content.clone() }}));
                }
            }
        }
    }
    let tools: Vec<Value> = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type":"function", "name":tool.name, "description":tool.description,
                // Explicitly avoid Responses' automatic schema normalization, which
                // changes optional fields into required nullable fields. The core
                // validates the original schema before any execution.
                "parameters":tool.input_schema, "strict":false
            })
        })
        .collect();
    let mut body = json!({"model":request.profile.model, "instructions":request.instructions, "input":input,
        "tools":tools, "stream":true, "store":false, "include":["reasoning.encrypted_content"],
        "parallel_tool_calls":false, "max_output_tokens":request.profile.max_output_tokens});
    if let Some(effort) = &request.profile.reasoning {
        body["reasoning"] = json!({"effort":effort});
    }
    Ok(body)
}

#[derive(Default)]
struct State {
    id: Option<String>,
    items: BTreeMap<usize, (String, bool)>,
    completed: Option<Value>,
}

impl State {
    fn accept(
        &mut self,
        event: &str,
        data: &str,
        events: &mpsc::Sender<ProviderEvent>,
    ) -> Result<()> {
        if data == "[DONE]" {
            if self.completed.is_none() {
                return Err(protocol("SSE ended without response.completed"));
            }
            return Ok(());
        }
        let value: Value =
            serde_json::from_str(data).map_err(|_| protocol("invalid JSON in Responses stream"))?;
        let kind = required_str(&value, "type")?;
        if !event.is_empty() && event != kind {
            return Err(protocol("SSE event name/type mismatch"));
        }
        if self.completed.is_some() {
            return Err(protocol("unexpected event after response.completed"));
        }
        if kind == "error" {
            return Err(stream_error(&value));
        }
        match kind {
            "response.created" => {
                if self.id.is_some() {
                    return Err(protocol("duplicate response.created"));
                }
                self.id = Some(required_str(&value["response"], "id")?.to_owned());
            }
            "response.output_item.added" => {
                self.require_started()?;
                let index = index(&value)?;
                if self.items.len() >= 1024 {
                    return Err(protocol("too many output items"));
                }
                let id = required_str(&value["item"], "id")?.to_owned();
                if self.items.insert(index, (id, false)).is_some() {
                    return Err(protocol("duplicate output item index"));
                }
            }
            "response.output_item.done" => {
                let index = index(&value)?;
                let (id, done) = self
                    .items
                    .get_mut(&index)
                    .ok_or_else(|| protocol("output item completed before it started"))?;
                if *done || required_str(&value["item"], "id")? != id {
                    return Err(protocol("invalid output item completion"));
                }
                *done = true;
            }
            "response.output_text.delta" => {
                self.require_started()?;
                let text = value["delta"]
                    .as_str()
                    .ok_or_else(|| protocol("invalid text delta"))?;
                let _ = events.try_send(ProviderEvent::TextDelta(text.to_owned()));
            }
            "response.reasoning_summary_text.delta" => {
                self.require_started()?;
                let text = value["delta"]
                    .as_str()
                    .ok_or_else(|| protocol("invalid reasoning summary delta"))?;
                let _ = events.try_send(ProviderEvent::ReasoningSummary(text.to_owned()));
            }
            "response.completed" => {
                self.require_started()?;
                let response = &value["response"];
                if response["status"] != "completed"
                    || required_str(response, "id")? != self.id.as_deref().unwrap_or("")
                {
                    return Err(protocol("invalid terminal response status or ID"));
                }
                let output = response["output"]
                    .as_array()
                    .ok_or_else(|| protocol("terminal response has no output array"))?;
                if output.len() != self.items.len() {
                    return Err(protocol("terminal output does not match streamed items"));
                }
                for (index, item) in output.iter().enumerate() {
                    if !self
                        .items
                        .get(&index)
                        .is_some_and(|(id, done)| *done && item["id"].as_str() == Some(id))
                    {
                        return Err(protocol(
                            "terminal response contains an unfinished output item",
                        ));
                    }
                }
                self.completed = Some(response.clone());
            }
            "response.failed" => {
                return Err(stream_error(&json!({"error":value["response"]["error"]})))
            }
            "response.incomplete" => {
                let reason = value["response"]["incomplete_details"]["reason"]
                    .as_str()
                    .unwrap_or("unknown reason");
                return Err(failure(
                    if reason == "max_output_tokens" {
                        ProviderErrorKind::Context
                    } else {
                        ProviderErrorKind::Unsupported
                    },
                    format!("response incomplete: {reason}"),
                ));
            }
            // Additional nonterminal metadata, argument, annotation, and
            // lifecycle events do not authorize execution. The final response
            // is authoritative for exact text, arguments, and continuation.
            _ => {
                self.require_started()?;
            }
        }
        Ok(())
    }

    fn require_started(&self) -> Result<()> {
        if self.id.is_none() {
            Err(protocol("Responses stream has not started"))
        } else {
            Ok(())
        }
    }

    fn finish(self, request: &ProviderRequest) -> Result<ProviderResponse> {
        let response = self.completed.ok_or_else(|| {
            protocol("stream ended without response.completed; no tool calls accepted")
        })?;
        let output = response["output"]
            .as_array()
            .ok_or_else(|| protocol("invalid response output"))?;
        let mut text = String::new();
        let mut calls = Vec::new();
        for item in output {
            match required_str(item, "type")? {
                "message" => {
                    if item["status"] != "completed" || item["role"] != "assistant" {
                        return Err(protocol("invalid completed assistant message"));
                    }
                    for content in item["content"]
                        .as_array()
                        .ok_or_else(|| protocol("invalid assistant content"))?
                    {
                        match required_str(content, "type")? {
                            "output_text" => text.push_str(
                                content["text"]
                                    .as_str()
                                    .ok_or_else(|| protocol("invalid output text"))?,
                            ),
                            "refusal" => {
                                return Err(failure(
                                    ProviderErrorKind::Unsupported,
                                    "model declined the request",
                                ))
                            }
                            _ => {
                                return Err(failure(
                                    ProviderErrorKind::Unsupported,
                                    "unsupported assistant output content",
                                ))
                            }
                        }
                    }
                }
                "function_call" => {
                    if item.get("status").is_some_and(|v| v != "completed") {
                        return Err(protocol("incomplete function call"));
                    }
                    calls.push(ToolCall {
                        id: required_str(item, "call_id")?.to_owned(),
                        name: required_str(item, "name")?.to_owned(),
                        arguments: serde_json::from_str(required_str(item, "arguments")?)
                            .map_err(|_| protocol("invalid function-call JSON arguments"))?,
                    });
                }
                "reasoning" => {} // Preserved, never flattened into visible assistant text.
                _ => {
                    return Err(failure(
                        ProviderErrorKind::Unsupported,
                        "unsupported Responses output item",
                    ))
                }
            }
        }
        validate_calls(&calls)?;
        let usage = &response["usage"];
        let input_tokens = usage_number(usage, "input_tokens")?;
        let cached_tokens = match usage
            .get("input_tokens_details")
            .and_then(|v| v.get("cached_tokens"))
        {
            Some(value) => value
                .as_u64()
                .ok_or_else(|| protocol("invalid cached token usage"))?,
            None => 0,
        };
        if cached_tokens > input_tokens {
            return Err(protocol("cached tokens exceed total input usage"));
        }
        Ok(ProviderResponse {
            text,
            tool_calls: calls,
            continuation: Some(continuation(&request.profile, output)?),
            usage: Usage {
                input_tokens,
                output_tokens: usage_number(usage, "output_tokens")?,
                cached_tokens,
            },
        })
    }
}

fn index(value: &Value) -> Result<usize> {
    value["output_index"]
        .as_u64()
        .and_then(|i| usize::try_from(i).ok())
        .filter(|i| *i < 1024)
        .ok_or_else(|| protocol("invalid output item index"))
}
