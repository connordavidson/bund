//! Native streaming adapters. Endpoints are API base URLs (including `/v1`
//! where appropriate), not individual resource URLs. Credentials are supplied
//! by the caller and are never read from the environment or serialized.
//!
//! The returned continuation contains provider-native, completed output items.
//! It is only valid for the exact model profile identity that produced it.
//! Progress events are best-effort display data; only `complete` can authorize
//! a subsequent tool call. Automatic retries happen only before a stream opens.
//!
//! Usage counts total input tokens, including cached input; `cached_tokens`
//! counts cache reads (not cache creation). Output includes thinking/reasoning.

mod anthropic;
mod openai;
mod sse;
mod transport;

pub use anthropic::AnthropicProvider;
pub use openai::OpenAiProvider;

use bund_core::{
    Continuation, Error, Message, ModelProfile, ProviderErrorKind, ProviderRequest, Result,
    ToolCall,
};
use serde_json::Value;
use std::collections::HashSet;

fn failure(kind: ProviderErrorKind, message: impl Into<String>) -> Error {
    Error::Provider {
        kind,
        message: message.into(),
    }
}

fn protocol(message: impl Into<String>) -> Error {
    failure(ProviderErrorKind::Protocol, message)
}

fn required_str<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| protocol(format!("missing or invalid {key}")))
}

fn usage_number(value: &Value, key: &str) -> Result<u64> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| protocol(format!("missing or invalid usage.{key}")))
}

fn native_items(message: &Message, profile: &ModelProfile) -> Result<Option<Vec<Value>>> {
    let Some(native) = &message.continuation else {
        return Ok(None);
    };
    if native.identity != profile.identity() || native.format_version != 1 {
        return Err(failure(ProviderErrorKind::Unsupported,
            "native continuation does not match this provider/model profile; create a handoff first"));
    }
    let items: Value = serde_json::from_slice(&native.payload)
        .map_err(|_| protocol("invalid stored native continuation"))?;
    Ok(Some(
        items
            .as_array()
            .ok_or_else(|| protocol("native continuation is not an array"))?
            .clone(),
    ))
}

fn continuation(profile: &ModelProfile, items: &[Value]) -> Result<Continuation> {
    Ok(Continuation {
        identity: profile.identity(),
        format_version: 1,
        payload: serde_json::to_vec(items)?,
    })
}

fn validate_request(request: &ProviderRequest, id: &str) -> Result<()> {
    if request.profile.provider != id {
        return Err(failure(
            ProviderErrorKind::Unsupported,
            "provider/profile mismatch",
        ));
    }
    if request.profile.model.trim().is_empty() || request.profile.max_output_tokens == 0 {
        return Err(protocol(
            "model and positive maximum output tokens are required",
        ));
    }
    let mut names = HashSet::new();
    for tool in &request.tools {
        if tool.name.is_empty() || !names.insert(&tool.name) || !tool.input_schema.is_object() {
            return Err(protocol(
                "tool names must be unique and schemas must be objects",
            ));
        }
    }
    Ok(())
}

fn validate_calls(calls: &[ToolCall]) -> Result<()> {
    if calls.len() > 128 {
        return Err(protocol("too many tool calls in one response"));
    }
    let mut ids = HashSet::new();
    for call in calls {
        if call.id.is_empty()
            || call.name.is_empty()
            || !call.arguments.is_object()
            || !ids.insert(&call.id)
        {
            return Err(protocol(
                "tool calls require unique IDs, names, and object arguments",
            ));
        }
    }
    Ok(())
}
