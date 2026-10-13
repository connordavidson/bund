use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{path::PathBuf, sync::Arc};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("a run is already active")]
    Busy,
    #[error("cancelled")]
    Cancelled,
    #[error("{0}")]
    Invalid(String),
    #[error("permission denied: {0}")]
    Denied(String),
    #[error("storage: {0}")]
    Storage(String),
    #[error("provider {kind:?}: {message}")]
    Provider {
        kind: ProviderErrorKind,
        message: String,
    },
    #[error("context limit: {0}")]
    ContextLimit(String),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorKind {
    Authentication,
    Quota,
    RateLimit,
    Transport,
    Protocol,
    Context,
    Unsupported,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelProfile {
    pub provider: String,
    pub model: String,
    pub endpoint: Option<String>,
    pub context_window: usize,
    pub max_output_tokens: u32,
    pub reasoning: Option<String>,
}

impl ModelProfile {
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            endpoint: None,
            context_window: 128_000,
            max_output_tokens: 8192,
            reasoning: None,
        }
    }
    pub fn identity(&self) -> String {
        format!(
            "{}|{}|{}|{:?}",
            self.provider,
            self.endpoint.as_deref().unwrap_or("default"),
            self.model,
            self.reasoning
        )
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Plan,
    #[default]
    Act,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Read,
    Write,
    Execute,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub name: String,
    pub content: String,
    pub is_error: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub effect: Effect,
}

/// Provider-owned, versioned continuation bytes. Never render these as text or
/// forward them to a different provider identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Continuation {
    pub identity: String,
    pub format_version: u32,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub text: String,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default)]
    pub tool_results: Vec<ToolResult>,
    #[serde(default)]
    pub continuation: Option<Continuation>,
}

impl Message {
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            text: text.into(),
            tool_calls: vec![],
            tool_results: vec![],
            continuation: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProviderRequest {
    pub profile: ModelProfile,
    pub instructions: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
}

#[derive(Debug, Clone)]
pub enum ProviderEvent {
    TextDelta(String),
    ReasoningSummary(String),
    Retrying { attempt: u32 },
}

#[derive(Debug, Clone)]
pub struct ProviderResponse {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub continuation: Option<Continuation>,
    pub usage: Usage,
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;
    /// Returns only a fully validated terminal response. Partial output is
    /// display-only and may never authorize tool execution.
    async fn complete(
        &self,
        request: ProviderRequest,
        events: mpsc::Sender<ProviderEvent>,
        cancel: CancellationToken,
    ) -> Result<ProviderResponse>;
}

#[derive(Debug, Clone)]
pub struct ToolContext {
    pub workspace: PathBuf,
    pub artifact_dir: PathBuf,
    pub command_timeout_secs: u64,
    pub output_limit: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreparedTool {
    pub arguments: Value,
    pub preview: String,
    /// Binds normalized arguments and observed preconditions. Re-prepare just
    /// before executing and reject changes instead of reusing old approval.
    pub fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditRecord {
    pub path: PathBuf,
    pub before: Option<Vec<u8>>,
    pub after_hash: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    pub edits: Vec<EditRecord>,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone)]
pub enum ToolEvent {
    Output(String),
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    /// Optional host-only inverse operation; it is never advertised to models.
    fn undo_tool(&self, _record: EditRecord) -> Option<Arc<dyn Tool>> {
        None
    }
    async fn prepare(&self, context: &ToolContext, arguments: Value) -> Result<PreparedTool>;
    async fn execute(
        &self,
        context: &ToolContext,
        prepared: PreparedTool,
        events: mpsc::Sender<ToolEvent>,
        cancel: CancellationToken,
    ) -> Result<ToolOutput>;
}

#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub command: String,
    pub cwd: PathBuf,
    pub timeout_secs: u64,
    pub output_limit: usize,
    pub artifact_dir: PathBuf,
}

#[async_trait]
pub trait CommandExecutor: Send + Sync {
    async fn execute(
        &self,
        spec: CommandSpec,
        events: mpsc::Sender<ToolEvent>,
        cancel: CancellationToken,
    ) -> Result<ToolOutput>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Limits {
    pub max_rounds: u32,
    pub run_timeout_secs: u64,
    pub command_timeout_secs: u64,
    pub output_limit: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_rounds: 30,
            run_timeout_secs: 900,
            command_timeout_secs: 120,
            output_limit: 65536,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Policy {
    pub mode: Mode,
    pub accept_edits: bool,
    /// Full exact command strings, never prefixes or model-supplied grants.
    pub allowed_commands: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionOptions {
    pub workspace: PathBuf,
    pub profile: ModelProfile,
    pub policy: Policy,
    pub limits: Limits,
    pub checks: Vec<String>,
}
impl SessionOptions {
    pub fn new(workspace: PathBuf, profile: ModelProfile) -> Self {
        Self {
            workspace,
            profile,
            policy: Policy::default(),
            limits: Limits::default(),
            checks: vec![],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub id: String,
    pub run_id: String,
    pub tool_call: ToolCall,
    pub preview: String,
    pub fingerprint: String,
    pub workspace: PathBuf,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    ApproveOnce,
    Deny,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    #[default]
    Idle,
    Running,
    AwaitingApproval,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}
impl RunStatus {
    pub fn is_active(self) -> bool {
        matches!(self, Self::Running | Self::AwaitingApproval)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ValidationStatus {
    #[default]
    NotRun,
    Passed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum EventKind {
    RunStarted {
        prompt: String,
    },
    TextDelta {
        text: String,
    },
    ReasoningSummary {
        text: String,
    },
    ToolPrepared {
        call: ToolCall,
        preview: String,
    },
    ApprovalRequired(ApprovalRequest),
    ApprovalResolved {
        id: String,
        decision: ApprovalDecision,
    },
    ToolStarted {
        call: ToolCall,
    },
    ToolOutput {
        call_id: String,
        text: String,
    },
    ToolFinished(ToolResult),
    AssistantMessage {
        text: String,
    },
    ContextCompacted {
        summary: String,
    },
    SettingsChanged,
    RunFinished {
        status: RunStatus,
        message: String,
        validation: ValidationStatus,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEvent {
    pub version: u32,
    pub session_id: String,
    pub sequence: u64,
    pub run_id: Option<String>,
    pub kind: EventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub policy: Policy,
    pub id: String,
    pub workspace: PathBuf,
    pub profile: ModelProfile,
    pub status: RunStatus,
    pub mode: Mode,
    pub cursor: u64,
    pub run_id: Option<String>,
    pub pending_approval: Option<ApprovalRequest>,
    pub messages: Vec<Message>,
    pub usage: Usage,
    pub validation: ValidationStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredSession {
    pub version: u32,
    pub id: String,
    pub options: SessionOptions,
    pub status: RunStatus,
    pub cursor: u64,
    pub run_id: Option<String>,
    pub messages: Vec<Message>,
    pub summary: Option<String>,
    #[serde(default)]
    pub instruction_paths: Vec<PathBuf>,
    #[serde(default)]
    pub user_constraints: Vec<String>,
    pub usage: Usage,
    pub validation: ValidationStatus,
    pub pending_approval: Option<ApprovalRequest>,
    pub unfinished_call: Option<ToolCall>,
    pub edits: Vec<EditRecord>,
}

#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn create(&self, session: &StoredSession) -> Result<()>;
    async fn load(&self, id: &str) -> Result<StoredSession>;
    async fn list(&self) -> Result<Vec<SessionSnapshot>>;
    /// Preserve the full pre-compaction context (including native state).
    async fn archive_context(&self, session: &StoredSession) -> Result<()>;
    /// Atomically save the snapshot and append its new events.
    async fn commit(&self, session: &StoredSession, events: &[SessionEvent]) -> Result<()>;
    async fn events_after(&self, id: &str, cursor: u64, limit: usize) -> Result<Vec<SessionEvent>>;
    fn artifact_dir(&self, session_id: &str) -> PathBuf;
    /// The returned guard holds a cross-process, workspace-scoped lock.
    fn lock_workspace(&self, workspace: &std::path::Path) -> Result<Box<dyn WorkspaceGuard>>;
}
pub trait WorkspaceGuard: Send + Sync {}

pub struct EngineDependencies {
    pub providers: Vec<Arc<dyn Provider>>,
    pub tools: Vec<Arc<dyn Tool>>,
    pub store: Arc<dyn SessionStore>,
}

impl StoredSession {
    pub fn snapshot(&self) -> SessionSnapshot {
        let mut messages = self.messages.clone();
        for message in &mut messages {
            message.continuation = None;
        }
        SessionSnapshot {
            policy: self.options.policy.clone(),
            id: self.id.clone(),
            workspace: self.options.workspace.clone(),
            profile: self.options.profile.clone(),
            status: self.status,
            mode: self.options.policy.mode,
            cursor: self.cursor,
            run_id: self.run_id.clone(),
            pending_approval: self.pending_approval.clone(),
            messages,
            usage: self.usage.clone(),
            validation: self.validation,
        }
    }
}
