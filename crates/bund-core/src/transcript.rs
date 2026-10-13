//! Bounded presentation state reconstructed from the durable event journal.
//! This module contains no terminal types. Consumers can page older runs from
//! `SessionHandle::events_after` and reduce them into a separate history window.
use crate::{
    ApprovalDecision, EventKind, ResponsePurpose, RunStatus, SessionEvent, ToolCall,
    ValidationStatus,
};
use std::collections::VecDeque;

#[derive(Debug, Clone, Copy)]
pub struct TranscriptLimits {
    pub max_runs: usize,
    pub max_items_per_run: usize,
    pub max_detail_bytes: usize,
    pub max_total_bytes: usize,
}

impl Default for TranscriptLimits {
    fn default() -> Self {
        Self {
            max_runs: 64,
            max_items_per_run: 256,
            max_detail_bytes: 65_536,
            max_total_bytes: 4 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TranscriptResponse {
    pub id: String,
    pub start_sequence: u64,
    pub purpose: ResponsePurpose,
    pub text: String,
    pub reasoning: Vec<String>,
    pub tool_calls: Vec<ToolCall>,
    pub complete: bool,
    pub is_final: bool,
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptToolStatus {
    Proposed,
    AwaitingApproval,
    Running,
    Completed,
    Failed,
    Denied,
    Interrupted,
}

#[derive(Debug, Clone)]
pub struct TranscriptTool {
    pub call: ToolCall,
    pub start_sequence: u64,
    pub preview: String,
    pub output: String,
    pub status: TranscriptToolStatus,
    pub truncated: bool,
    pub approval_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TranscriptRun {
    pub id: String,
    pub start_sequence: u64,
    pub end_sequence: u64,
    pub prompt: String,
    pub status: RunStatus,
    pub responses: Vec<TranscriptResponse>,
    pub tools: Vec<TranscriptTool>,
    /// Final output, including explicit local operations such as `/diff`.
    pub output: Option<String>,
    pub final_response_id: Option<String>,
    /// Always visible run failure/cancellation information, never tool details.
    pub error: Option<String>,
    pub validation: ValidationStatus,
    pub truncated: bool,
    /// Incremented for each applied event; useful for invalidating render caches.
    pub revision: u64,
    lifecycle: bool,
}

#[derive(Debug, Default, Clone)]
pub struct Transcript {
    runs: VecDeque<TranscriptRun>,
    limits: TranscriptLimits,
    cursor: u64,
}

impl Transcript {
    pub fn with_limits(limits: TranscriptLimits) -> Self {
        Self {
            limits: TranscriptLimits {
                max_runs: limits.max_runs.max(1),
                max_items_per_run: limits.max_items_per_run.max(1),
                max_detail_bytes: limits.max_detail_bytes.max(1),
                max_total_bytes: limits.max_total_bytes.max(1),
            },
            ..Self::default()
        }
    }

    pub fn limits(&self) -> TranscriptLimits {
        self.limits
    }

    pub fn runs(&self) -> &VecDeque<TranscriptRun> {
        &self.runs
    }

    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    pub fn clear(&mut self) {
        self.runs.clear();
        self.cursor = 0;
    }

    /// Events must be applied in journal order. Duplicate/replayed sequence
    /// numbers are ignored. A new reducer can start at any paged run boundary.
    pub fn apply(&mut self, event: &SessionEvent) {
        if event.sequence <= self.cursor {
            return;
        }
        self.cursor = event.sequence;
        if matches!(event.kind, EventKind::SettingsChanged) {
            return;
        }
        let Some(id) = &event.run_id else {
            return;
        };
        let limit = self
            .limits
            .max_detail_bytes
            .min(self.limits.max_total_bytes);
        let index = self.runs.iter().position(|run| run.id == *id);
        let index = index.unwrap_or_else(|| {
            self.runs.push_back(TranscriptRun {
                id: id.clone(),
                start_sequence: event.sequence,
                end_sequence: event.sequence,
                prompt: String::new(),
                status: RunStatus::Running,
                responses: vec![],
                tools: vec![],
                output: None,
                final_response_id: None,
                error: None,
                validation: ValidationStatus::NotRun,
                truncated: false,
                revision: 0,
                lifecycle: false,
            });
            self.runs.len() - 1
        });
        let run = &mut self.runs[index];
        run.end_sequence = event.sequence;
        run.revision += 1;
        match &event.kind {
            EventKind::RunStarted { prompt } => {
                run.prompt = bounded(prompt, limit, &mut run.truncated);
            }
            EventKind::ResponseStarted { id, purpose } => {
                run.lifecycle = true;
                response(run, id, *purpose, event.sequence);
            }
            EventKind::ResponseTextDelta { id, text } => {
                run.lifecycle = true;
                let response = response(run, id, ResponsePurpose::Conversation, event.sequence);
                if !response.complete {
                    append(&mut response.text, text, limit, &mut response.truncated);
                }
            }
            EventKind::ResponseReasoningDelta { id, text } => {
                run.lifecycle = true;
                let response = response(run, id, ResponsePurpose::Conversation, event.sequence);
                if !response.complete {
                    if response.reasoning.is_empty() {
                        response.reasoning.push(String::new());
                    }
                    append(
                        &mut response.reasoning[0],
                        text,
                        limit,
                        &mut response.truncated,
                    );
                }
            }
            EventKind::ResponseCompleted {
                id,
                purpose,
                text,
                reasoning,
                tool_calls,
            } => {
                run.lifecycle = true;
                let response = response(run, id, *purpose, event.sequence);
                response.purpose = *purpose;
                response.truncated = false;
                response.text = bounded(text, limit, &mut response.truncated);
                response.reasoning = bounded_reasoning(reasoning, limit, &mut response.truncated);
                response.tool_calls = tool_calls
                    .iter()
                    .take(32)
                    .map(|call| {
                        let (call, truncated) = bounded_call(call, limit);
                        response.truncated |= truncated;
                        call
                    })
                    .collect();
                response.truncated |= tool_calls.len() > 32;
                response.complete = true;
                if *purpose == ResponsePurpose::Conversation {
                    for call in tool_calls.iter().take(32) {
                        tool(run, call, event.sequence, limit);
                    }
                }
            }
            EventKind::TextDelta { text } if !run.lifecycle => {
                let response = legacy_response(run, event.sequence);
                append(&mut response.text, text, limit, &mut response.truncated);
            }
            EventKind::ReasoningSummary { text } if !run.lifecycle => {
                let response = legacy_response(run, event.sequence);
                if response.reasoning.is_empty() {
                    response.reasoning.push(String::new());
                }
                append(
                    &mut response.reasoning[0],
                    text,
                    limit,
                    &mut response.truncated,
                );
            }
            EventKind::AssistantMessage { text } if !run.lifecycle => {
                let response = legacy_response(run, event.sequence);
                response.text = bounded(text, limit, &mut response.truncated);
                response.complete = true;
            }
            EventKind::ContextCompacted { summary } if !run.lifecycle => {
                let response = legacy_response(run, event.sequence);
                response.purpose = ResponsePurpose::Compaction;
                response.text = bounded(summary, limit, &mut response.truncated);
                response.complete = true;
            }
            EventKind::ToolPrepared { call, preview } => {
                let tool = tool(run, call, event.sequence, limit);
                tool.preview = bounded(preview, limit, &mut tool.truncated);
            }
            EventKind::ApprovalRequired(request) => {
                let tool = tool(run, &request.tool_call, event.sequence, limit);
                tool.preview = bounded(&request.preview, limit, &mut tool.truncated);
                tool.status = TranscriptToolStatus::AwaitingApproval;
                tool.approval_id = Some(request.id.clone());
                run.status = RunStatus::AwaitingApproval;
            }
            EventKind::ApprovalResolved { id, decision } => {
                if let Some(tool) = run
                    .tools
                    .iter_mut()
                    .find(|tool| tool.approval_id.as_ref() == Some(id))
                {
                    tool.status = if *decision == ApprovalDecision::Deny {
                        TranscriptToolStatus::Denied
                    } else {
                        TranscriptToolStatus::Proposed
                    };
                    tool.approval_id = None;
                }
                run.status = RunStatus::Running;
            }
            EventKind::ToolStarted { call } => {
                tool(run, call, event.sequence, limit).status = TranscriptToolStatus::Running;
            }
            EventKind::ToolOutput { call_id, text } => {
                if let Some(tool) = run.tools.iter_mut().find(|tool| tool.call.id == *call_id) {
                    append(&mut tool.output, text, limit, &mut tool.truncated);
                }
            }
            EventKind::ToolFinished(result) => {
                let call = ToolCall {
                    id: result.call_id.clone(),
                    name: result.name.clone(),
                    arguments: serde_json::Value::Null,
                };
                let tool = tool(run, &call, event.sequence, limit);
                tool.output = bounded(&result.content, limit, &mut tool.truncated);
                tool.approval_id = None;
                if tool.status != TranscriptToolStatus::Denied {
                    tool.status = if result.is_error {
                        TranscriptToolStatus::Failed
                    } else {
                        TranscriptToolStatus::Completed
                    };
                }
            }
            EventKind::RunOutput { response_id, text } => {
                run.output = Some(bounded(text, limit, &mut run.truncated));
                run.final_response_id = response_id.clone();
                for response in &mut run.responses {
                    response.is_final = response_id.as_ref() == Some(&response.id);
                }
            }
            EventKind::RunFinished {
                status,
                message,
                validation,
            } => {
                run.status = *status;
                run.validation = *validation;
                if *status == RunStatus::Completed {
                    if run.output.is_none() {
                        run.output = Some(bounded(message, limit, &mut run.truncated));
                        if let Some(response) = run.responses.iter_mut().rev().find(|response| {
                            response.purpose == ResponsePurpose::Conversation
                                && response.complete
                                && response.text == *message
                        }) {
                            response.is_final = true;
                            run.final_response_id = Some(response.id.clone());
                        }
                    }
                } else {
                    run.error = Some(bounded(message, limit, &mut run.truncated));
                }
                for tool in &mut run.tools {
                    if matches!(
                        tool.status,
                        TranscriptToolStatus::Running
                            | TranscriptToolStatus::Proposed
                            | TranscriptToolStatus::AwaitingApproval
                    ) {
                        tool.status = TranscriptToolStatus::Interrupted;
                        tool.approval_id = None;
                    }
                }
            }
            _ => {}
        }
        self.enforce_limits();
    }

    fn enforce_limits(&mut self) {
        for run in &mut self.runs {
            while run.responses.len() + run.tools.len() > self.limits.max_items_per_run {
                remove_oldest_item(run);
            }
        }
        while self.runs.len() > self.limits.max_runs
            || (self.runs.len() > 1 && self.total_bytes() > self.limits.max_total_bytes)
        {
            self.runs.pop_front();
        }
        while self.total_bytes() > self.limits.max_total_bytes {
            let run = self
                .runs
                .front_mut()
                .expect("nonempty transcript exceeds byte limit");
            if !run.responses.is_empty() || !run.tools.is_empty() {
                remove_oldest_item(run);
            } else {
                let limit = self.limits.max_total_bytes / 3;
                run.prompt = bounded(&run.prompt, limit, &mut run.truncated);
                if let Some(text) = &mut run.output {
                    *text = bounded(text, limit, &mut run.truncated);
                }
                if let Some(text) = &mut run.error {
                    *text = bounded(text, limit, &mut run.truncated);
                }
                break;
            }
        }
    }

    fn total_bytes(&self) -> usize {
        self.runs
            .iter()
            .map(|run| {
                run.prompt.len()
                    + run.output.as_ref().map_or(0, String::len)
                    + run.error.as_ref().map_or(0, String::len)
                    + run
                        .responses
                        .iter()
                        .map(|r| {
                            r.text.len()
                                + r.reasoning.iter().map(String::len).sum::<usize>()
                                + r.tool_calls.iter().map(call_bytes).sum::<usize>()
                        })
                        .sum::<usize>()
                    + run
                        .tools
                        .iter()
                        .map(|t| t.preview.len() + t.output.len() + call_bytes(&t.call))
                        .sum::<usize>()
            })
            .sum()
    }
}

fn response<'a>(
    run: &'a mut TranscriptRun,
    id: &str,
    purpose: ResponsePurpose,
    sequence: u64,
) -> &'a mut TranscriptResponse {
    let index = run
        .responses
        .iter()
        .position(|response| response.id == id)
        .unwrap_or_else(|| {
            run.responses.push(TranscriptResponse {
                id: id.into(),
                start_sequence: sequence,
                purpose,
                text: String::new(),
                reasoning: vec![],
                tool_calls: vec![],
                complete: false,
                is_final: false,
                truncated: false,
            });
            run.responses.len() - 1
        });
    &mut run.responses[index]
}

fn legacy_response(run: &mut TranscriptRun, sequence: u64) -> &mut TranscriptResponse {
    let id = run
        .responses
        .last()
        .filter(|response| !response.complete)
        .map(|response| response.id.clone())
        .unwrap_or_else(|| format!("legacy-{sequence}"));
    response(run, &id, ResponsePurpose::Conversation, sequence)
}

fn tool<'a>(
    run: &'a mut TranscriptRun,
    call: &ToolCall,
    sequence: u64,
    limit: usize,
) -> &'a mut TranscriptTool {
    let index = run
        .tools
        .iter()
        .position(|tool| tool.call.id == call.id)
        .unwrap_or_else(|| {
            let (call, truncated) = bounded_call(call, limit);
            run.tools.push(TranscriptTool {
                call,
                start_sequence: sequence,
                preview: String::new(),
                output: String::new(),
                status: TranscriptToolStatus::Proposed,
                truncated,
                approval_id: None,
            });
            run.tools.len() - 1
        });
    &mut run.tools[index]
}

fn bounded_call(call: &ToolCall, limit: usize) -> (ToolCall, bool) {
    let mut call = call.clone();
    let arguments = call.arguments.to_string();
    let truncated = arguments.len() > limit;
    if truncated {
        call.arguments = serde_json::Value::String(bounded(&arguments, limit, &mut false));
    }
    (call, truncated)
}

fn call_bytes(call: &ToolCall) -> usize {
    call.name.len() + call.id.len() + call.arguments.to_string().len()
}

fn bounded_reasoning(reasoning: &[String], limit: usize, truncated: &mut bool) -> Vec<String> {
    *truncated |= reasoning.len() > 128;
    let mut remaining = limit;
    reasoning
        .iter()
        .take(128)
        .map(|text| {
            let text = bounded(text, remaining, truncated);
            remaining = remaining.saturating_sub(text.len());
            text
        })
        .collect()
}

fn bounded(text: &str, limit: usize, truncated: &mut bool) -> String {
    let mut end = text.len().min(limit);
    *truncated |= end < text.len();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].into()
}

fn append(target: &mut String, text: &str, limit: usize, truncated: &mut bool) {
    target.push_str(&bounded(
        text,
        limit.saturating_sub(target.len()),
        truncated,
    ));
}

fn remove_oldest_item(run: &mut TranscriptRun) {
    if run
        .responses
        .first()
        .map(|r| r.start_sequence)
        .unwrap_or(u64::MAX)
        <= run
            .tools
            .first()
            .map(|t| t.start_sequence)
            .unwrap_or(u64::MAX)
    {
        run.responses.remove(0);
    } else {
        run.tools.remove(0);
    }
    run.truncated = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolResult;

    fn apply(transcript: &mut Transcript, run_id: &str, kind: EventKind) {
        transcript.apply(&SessionEvent {
            version: 1,
            session_id: "session".into(),
            sequence: transcript.cursor() + 1,
            run_id: Some(run_id.into()),
            kind,
        });
    }

    #[test]
    fn authoritative_completion_reconciles_gaps_and_legacy_duplicates() {
        let mut transcript = Transcript::default();
        apply(
            &mut transcript,
            "run",
            EventKind::RunStarted {
                prompt: "Explain".into(),
            },
        );
        apply(
            &mut transcript,
            "run",
            EventKind::ResponseStarted {
                id: "response".into(),
                purpose: ResponsePurpose::Conversation,
            },
        );
        apply(
            &mut transcript,
            "run",
            EventKind::ResponseTextDelta {
                id: "response".into(),
                text: "partial gap".into(),
            },
        );
        apply(
            &mut transcript,
            "run",
            EventKind::TextDelta {
                text: "partial gap".into(),
            },
        );
        apply(
            &mut transcript,
            "run",
            EventKind::ResponseReasoningDelta {
                id: "response".into(),
                text: "partial summary".into(),
            },
        );
        apply(
            &mut transcript,
            "run",
            EventKind::ResponseCompleted {
                id: "response".into(),
                purpose: ResponsePurpose::Conversation,
                text: "Complete answer".into(),
                reasoning: vec!["Complete summary".into()],
                tool_calls: vec![],
            },
        );
        apply(
            &mut transcript,
            "run",
            EventKind::AssistantMessage {
                text: "Complete answer".into(),
            },
        );
        apply(
            &mut transcript,
            "run",
            EventKind::RunOutput {
                response_id: Some("response".into()),
                text: "Complete answer".into(),
            },
        );
        apply(
            &mut transcript,
            "run",
            EventKind::RunFinished {
                status: RunStatus::Completed,
                message: "Complete answer".into(),
                validation: ValidationStatus::Passed,
            },
        );
        let run = &transcript.runs()[0];
        assert_eq!(run.responses.len(), 1);
        assert_eq!(run.responses[0].text, "Complete answer");
        assert_eq!(run.responses[0].reasoning, ["Complete summary"]);
        assert!(run.responses[0].is_final);
        assert_eq!(run.final_response_id.as_deref(), Some("response"));
        assert_eq!(run.validation, ValidationStatus::Passed);
    }

    #[test]
    fn legacy_serialized_events_remain_readable_and_replay_is_idempotent() {
        let lines = [
            r#"{"version":1,"session_id":"s","sequence":1,"run_id":"r","kind":{"type":"run_started","data":{"prompt":"Hello"}}}"#,
            r#"{"version":1,"session_id":"s","sequence":2,"run_id":"r","kind":{"type":"text_delta","data":{"text":"Part"}}}"#,
            r#"{"version":1,"session_id":"s","sequence":3,"run_id":"r","kind":{"type":"assistant_message","data":{"text":"Complete"}}}"#,
            r#"{"version":1,"session_id":"s","sequence":4,"run_id":"r","kind":{"type":"run_finished","data":{"status":"completed","message":"Complete","validation":"not_run"}}}"#,
        ];
        let mut transcript = Transcript::default();
        for _ in 0..2 {
            for line in lines {
                transcript.apply(&serde_json::from_str(line).unwrap());
            }
        }
        assert_eq!(transcript.runs().len(), 1);
        let run = &transcript.runs()[0];
        assert_eq!(run.responses.len(), 1);
        assert_eq!(run.responses[0].text, "Complete");
        assert!(run.responses[0].is_final);
        assert_eq!(run.revision, 4);
        assert_eq!(run.start_sequence, 1);
        assert_eq!(run.end_sequence, 4);
    }

    #[test]
    fn compaction_is_separate_from_conversation_for_current_and_legacy_events() {
        for lifecycle in [true, false] {
            let mut transcript = Transcript::default();
            apply(
                &mut transcript,
                "run",
                EventKind::RunStarted {
                    prompt: "Continue".into(),
                },
            );
            if lifecycle {
                apply(
                    &mut transcript,
                    "run",
                    EventKind::ResponseStarted {
                        id: "summary".into(),
                        purpose: ResponsePurpose::Compaction,
                    },
                );
                apply(
                    &mut transcript,
                    "run",
                    EventKind::ResponseTextDelta {
                        id: "summary".into(),
                        text: "Internal summary".into(),
                    },
                );
                apply(
                    &mut transcript,
                    "run",
                    EventKind::ResponseCompleted {
                        id: "summary".into(),
                        purpose: ResponsePurpose::Compaction,
                        text: "Internal summary".into(),
                        reasoning: vec![],
                        tool_calls: vec![],
                    },
                );
            } else {
                apply(
                    &mut transcript,
                    "run",
                    EventKind::TextDelta {
                        text: "Internal summary".into(),
                    },
                );
            }
            apply(
                &mut transcript,
                "run",
                EventKind::ContextCompacted {
                    summary: "Internal summary".into(),
                },
            );
            if lifecycle {
                apply(
                    &mut transcript,
                    "run",
                    EventKind::ResponseStarted {
                        id: "answer".into(),
                        purpose: ResponsePurpose::Conversation,
                    },
                );
                apply(
                    &mut transcript,
                    "run",
                    EventKind::ResponseCompleted {
                        id: "answer".into(),
                        purpose: ResponsePurpose::Conversation,
                        text: "Answer".into(),
                        reasoning: vec![],
                        tool_calls: vec![],
                    },
                );
            } else {
                apply(
                    &mut transcript,
                    "run",
                    EventKind::AssistantMessage {
                        text: "Answer".into(),
                    },
                );
            }
            let run = &transcript.runs()[0];
            assert_eq!(run.responses.len(), 2);
            assert_eq!(run.responses[0].purpose, ResponsePurpose::Compaction);
            assert_eq!(run.responses[1].purpose, ResponsePurpose::Conversation);
            assert_eq!(run.responses[1].text, "Answer");
        }
    }

    #[test]
    fn local_output_and_tool_failures_survive_without_assistant_response() {
        let mut transcript = Transcript::default();
        apply(
            &mut transcript,
            "local",
            EventKind::RunStarted {
                prompt: "User requested git_diff".into(),
            },
        );
        apply(
            &mut transcript,
            "local",
            EventKind::RunOutput {
                response_id: None,
                text: "diff --git a/file b/file".into(),
            },
        );
        apply(
            &mut transcript,
            "local",
            EventKind::RunFinished {
                status: RunStatus::Completed,
                message: "diff --git a/file b/file".into(),
                validation: ValidationStatus::NotRun,
            },
        );
        assert_eq!(
            transcript.runs()[0].output.as_deref(),
            Some("diff --git a/file b/file")
        );
        apply(
            &mut transcript,
            "failed",
            EventKind::ToolFinished(ToolResult {
                call_id: "call".into(),
                name: "run_command".into(),
                content: "test failed".into(),
                is_error: true,
            }),
        );
        apply(
            &mut transcript,
            "failed",
            EventKind::RunFinished {
                status: RunStatus::Failed,
                message: "validation failed".into(),
                validation: ValidationStatus::Failed,
            },
        );
        let failed = &transcript.runs()[1];
        assert_eq!(failed.tools[0].status, TranscriptToolStatus::Failed);
        assert_eq!(failed.tools[0].output, "test failed");
        assert_eq!(failed.error.as_deref(), Some("validation failed"));
    }

    #[test]
    fn bounds_keep_utf8_and_history_page_cursors() {
        let limits = TranscriptLimits {
            max_runs: 2,
            max_items_per_run: 2,
            max_detail_bytes: 13,
            max_total_bytes: 80,
        };
        let mut transcript = Transcript::with_limits(limits);
        for id in ["one", "two", "three"] {
            apply(
                &mut transcript,
                id,
                EventKind::RunStarted {
                    prompt: "世界世界世界世界世界".into(),
                },
            );
            apply(
                &mut transcript,
                id,
                EventKind::AssistantMessage {
                    text: "世界世界世界世界世界".into(),
                },
            );
            apply(
                &mut transcript,
                id,
                EventKind::RunFinished {
                    status: RunStatus::Completed,
                    message: "世界世界世界世界世界".into(),
                    validation: ValidationStatus::NotRun,
                },
            );
        }
        assert_eq!(transcript.runs().len(), 2);
        assert_eq!(transcript.runs()[0].id, "two");
        assert_eq!(transcript.runs()[0].start_sequence, 4);
        assert!(transcript.total_bytes() <= 80);
        assert!(transcript
            .runs()
            .iter()
            .all(|run| run.truncated && run.prompt.len() <= 13));
        for _ in 0..8 {
            apply(
                &mut transcript,
                "three",
                EventKind::AssistantMessage {
                    text: "answer".into(),
                },
            );
        }
        assert!(transcript.runs().back().unwrap().responses.len() <= 2);
    }

    #[test]
    fn clipped_tool_arguments_are_disclosed_on_response_and_tool() {
        let mut transcript = Transcript::with_limits(TranscriptLimits {
            max_detail_bytes: 16,
            ..TranscriptLimits::default()
        });
        apply(
            &mut transcript,
            "run",
            EventKind::ResponseCompleted {
                id: "r".into(),
                purpose: ResponsePurpose::Conversation,
                text: String::new(),
                reasoning: vec![],
                tool_calls: vec![ToolCall {
                    id: "tool".into(),
                    name: "run_command".into(),
                    arguments: serde_json::json!({"command":"x".repeat(1000)}),
                }],
            },
        );
        let run = &transcript.runs()[0];
        assert!(run.responses[0].truncated);
        assert!(run.tools[0].truncated);
        assert!(run.tools[0].call.arguments.as_str().unwrap().len() <= 16);
    }
}
