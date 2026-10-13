//! Workspace tools and explicitly approved host execution for Bund.
//!
//! Host processes are not sandboxed. The permission gate belongs to bund-core;
//! direct callers of these low-level tools must provide their own authorization.
mod command;
mod files;
mod git;
mod paths;

pub use command::HostCommandExecutor;
pub use files::{guarded_undo, undo_tool};

use bund_core::{Error, PreparedTool, Result, Tool, ToolContext, ToolDefinition};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::Arc;

pub const DEFAULT_OUTPUT_LIMIT: usize = 64 * 1024;
pub const MAX_OUTPUT_LIMIT: usize = 1024 * 1024;
pub const ARTIFACT_OUTPUT_LIMIT: usize = 8 * 1024 * 1024;

/// The host-only undo operation is intentionally absent from the model catalog.
pub fn built_in_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(files::ReadFile),
        Arc::new(files::ListFiles),
        Arc::new(files::Search),
        Arc::new(files::ApplyPatch),
        Arc::new(command::RunCommand),
        Arc::new(git::GitStatus),
        Arc::new(git::GitDiff),
    ]
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn prepared(arguments: Value, preview: String, preconditions: Value) -> Result<PreparedTool> {
    let fingerprint = hash(&serde_json::to_vec(&serde_json::json!({
        "arguments": arguments, "preconditions": preconditions,
    }))?);
    Ok(PreparedTool {
        arguments,
        preview,
        fingerprint,
    })
}

fn check_prepared(expected: &PreparedTool, actual: &PreparedTool) -> Result<()> {
    if expected.fingerprint != actual.fingerprint {
        return Err(Error::Invalid(
            "operation changed after preparation; request fresh approval".into(),
        ));
    }
    Ok(())
}

fn definition(
    name: &str,
    description: &str,
    effect: bund_core::Effect,
    properties: Value,
    required: &[&str],
) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: description.into(),
        effect,
        input_schema: serde_json::json!({"type":"object", "properties":properties,
            "required":required,"additionalProperties":false}),
    }
}

fn bounded_text(mut text: String, context: &ToolContext) -> String {
    let limit = context.output_limit.clamp(1, MAX_OUTPUT_LIMIT);
    if text.len() > limit {
        let marker = "\n[output truncated]";
        let mut end = limit.saturating_sub(marker.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        if marker.len() <= limit {
            text.push_str(marker);
        }
    }
    text
}
