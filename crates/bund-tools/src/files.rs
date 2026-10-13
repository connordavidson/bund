use crate::{bounded_text, check_prepared, definition, hash, paths, prepared};
use async_trait::async_trait;
use bund_core::{
    EditRecord, Effect, Error, PreparedTool, Result, Tool, ToolContext, ToolDefinition, ToolEvent,
    ToolOutput,
};
use regex::RegexBuilder;
use serde::Deserialize;
use serde_json::{json, Value};
use similar::TextDiff;
use std::{path::PathBuf, sync::Arc};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub(crate) struct ReadFile;
pub(crate) struct ListFiles;
pub(crate) struct Search;
pub(crate) struct ApplyPatch;

fn cancel_check(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}
fn default_path() -> String {
    ".".into()
}
fn one() -> usize {
    1
}
fn two_hundred() -> usize {
    200
}
fn normalize(context: &ToolContext, path: &str) -> Result<(PathBuf, String)> {
    let path = paths::relative(path)?;
    let root = paths::root(context)?.display().to_string();
    Ok((path, root))
}
fn text(bytes: Vec<u8>) -> Result<String> {
    String::from_utf8(bytes).map_err(|_| Error::Invalid("file is not UTF-8 text".into()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: String,
    #[serde(default = "one")]
    start_line: usize,
    #[serde(default = "two_hundred")]
    max_lines: usize,
}

#[async_trait]
impl Tool for ReadFile {
    fn definition(&self) -> ToolDefinition {
        definition("read_file", "Read UTF-8 text in the workspace, with line numbers. Symlinks and files over 2 MiB are refused.", Effect::Read,
            json!({"path":{"type":"string"},"start_line":{"type":"integer","minimum":1},"max_lines":{"type":"integer","minimum":1,"maximum":2000}}), &["path"])
    }
    async fn prepare(&self, context: &ToolContext, arguments: Value) -> Result<PreparedTool> {
        let a: ReadArgs = serde_json::from_value(arguments)?;
        if a.start_line == 0 || !(1..=2000).contains(&a.max_lines) {
            return Err(Error::Invalid("invalid line range".into()));
        }
        let (path, root) = normalize(context, &a.path)?;
        let bytes = paths::read(context, &path)?
            .ok_or_else(|| Error::Invalid("file does not exist".into()))?;
        prepared(
            json!({"path":path,"start_line":a.start_line,"max_lines":a.max_lines}),
            format!("Read {} from line {}", path.display(), a.start_line),
            json!({"root":root,"hash":hash(&bytes)}),
        )
    }
    async fn execute(
        &self,
        context: &ToolContext,
        prepared: PreparedTool,
        _events: mpsc::Sender<ToolEvent>,
        cancel: CancellationToken,
    ) -> Result<ToolOutput> {
        cancel_check(&cancel)?;
        let a: ReadArgs = serde_json::from_value(prepared.arguments)?;
        let path = paths::relative(&a.path)?;
        let content = text(
            paths::read(context, &path)?
                .ok_or_else(|| Error::Invalid("file does not exist".into()))?,
        )?;
        let result = content
            .lines()
            .enumerate()
            .skip(a.start_line - 1)
            .take(a.max_lines)
            .map(|(i, line)| format!("{}: {}\n", i + 1, line))
            .collect::<String>();
        Ok(ToolOutput {
            content: bounded_text(result, context),
            ..Default::default()
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    #[serde(default = "default_path")]
    path: String,
}

#[async_trait]
impl Tool for ListFiles {
    fn definition(&self) -> ToolDefinition {
        definition("list_files", "List workspace files recursively, excluding symlinks, .git, target and node_modules; at most 5000 files.", Effect::Read,
            json!({"path":{"type":"string"}}), &[])
    }
    async fn prepare(&self, context: &ToolContext, arguments: Value) -> Result<PreparedTool> {
        let a: ListArgs = serde_json::from_value(arguments)?;
        let (path, root) = normalize(context, &a.path)?;
        paths::directory(context, &path)?;
        prepared(
            json!({"path":path}),
            format!("List files under {}", path.display()),
            json!({"root":root}),
        )
    }
    async fn execute(
        &self,
        context: &ToolContext,
        prepared: PreparedTool,
        _events: mpsc::Sender<ToolEvent>,
        cancel: CancellationToken,
    ) -> Result<ToolOutput> {
        cancel_check(&cancel)?;
        let a: ListArgs = serde_json::from_value(prepared.arguments)?;
        let files = paths::list(context, &paths::relative(&a.path)?, 5000)?;
        let mut content = files
            .iter()
            .map(|p| format!("{}\n", p.display()))
            .collect::<String>();
        if files.len() == 5000 {
            content.push_str("[file limit reached]\n");
        }
        Ok(ToolOutput {
            content: bounded_text(content, context),
            ..Default::default()
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    pattern: String,
    #[serde(default = "default_path")]
    path: String,
}

#[async_trait]
impl Tool for Search {
    fn definition(&self) -> ToolDefinition {
        definition("search", "Search UTF-8 workspace files with a Rust regular expression; returns path:line:text. Scans at most 16 MiB and skips binary/large files and build directories.", Effect::Read,
            json!({"pattern":{"type":"string"},"path":{"type":"string"}}), &["pattern"])
    }
    async fn prepare(&self, context: &ToolContext, arguments: Value) -> Result<PreparedTool> {
        let a: SearchArgs = serde_json::from_value(arguments)?;
        if a.pattern.len() > 1024 {
            return Err(Error::Invalid("search pattern exceeds 1024 bytes".into()));
        }
        RegexBuilder::new(&a.pattern)
            .size_limit(1024 * 1024)
            .build()
            .map_err(|e| Error::Invalid(e.to_string()))?;
        let (path, root) = normalize(context, &a.path)?;
        paths::directory(context, &path)?;
        prepared(
            json!({"pattern":a.pattern,"path":path}),
            format!("Search {} for {:?}", path.display(), a.pattern),
            json!({"root":root}),
        )
    }
    async fn execute(
        &self,
        context: &ToolContext,
        prepared: PreparedTool,
        _events: mpsc::Sender<ToolEvent>,
        cancel: CancellationToken,
    ) -> Result<ToolOutput> {
        let a: SearchArgs = serde_json::from_value(prepared.arguments)?;
        let regex = RegexBuilder::new(&a.pattern)
            .size_limit(1024 * 1024)
            .build()
            .map_err(|e| Error::Invalid(e.to_string()))?;
        let files = paths::list(context, &paths::relative(&a.path)?, 5000)?;
        let mut output = String::new();
        let mut scanned = 0;
        let limit = context.output_limit.clamp(1, crate::MAX_OUTPUT_LIMIT);
        for path in files {
            cancel_check(&cancel)?;
            let bytes = match paths::read(context, &path) {
                Ok(Some(b)) => b,
                _ => continue,
            };
            scanned += bytes.len();
            if scanned > 16 * 1024 * 1024 {
                output.push_str("[scan limit reached]\n");
                break;
            }
            let Ok(content) = String::from_utf8(bytes) else {
                continue;
            };
            for (i, line) in content.lines().enumerate() {
                if regex.is_match(line) {
                    output.push_str(&format!("{}:{}:{}\n", path.display(), i + 1, line));
                    if output.len() > limit {
                        break;
                    }
                }
            }
            if output.len() > limit {
                break;
            }
        }
        Ok(ToolOutput {
            content: bounded_text(output, context),
            ..Default::default()
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchArgs {
    path: String,
    content: String,
}

fn diff(path: &std::path::Path, before: Option<&[u8]>, after: Option<&[u8]>) -> Result<String> {
    let old = std::str::from_utf8(before.unwrap_or_default())
        .map_err(|_| Error::Invalid("binary files cannot be patched".into()))?;
    let new = std::str::from_utf8(after.unwrap_or_default())
        .map_err(|_| Error::Invalid("binary files cannot be patched".into()))?;
    let from = if before.is_some() {
        format!("a/{}", path.display())
    } else {
        "/dev/null".into()
    };
    let to = if after.is_some() {
        format!("b/{}", path.display())
    } else {
        "/dev/null".into()
    };
    Ok(TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(&from, &to)
        .to_string())
}

fn prepare_patch(
    context: &ToolContext,
    arguments: Value,
) -> Result<(PreparedTool, PathBuf, Option<Vec<u8>>, String)> {
    let a: PatchArgs = serde_json::from_value(arguments)?;
    if a.content.len() > 256 * 1024 {
        return Err(Error::Invalid(
            "new file content exceeds 256 KiB patch limit".into(),
        ));
    }
    let (path, root) = normalize(context, &a.path)?;
    let before = paths::Entry::new(context, &path, true)?.read()?;
    let preview = diff(&path, before.as_deref(), Some(a.content.as_bytes()))?;
    let p = prepared(
        json!({"path":path,"content":a.content}),
        preview,
        json!({"root":root,"base_hash":before.as_deref().map(hash)}),
    )?;
    Ok((p, path, before, a.content))
}

#[async_trait]
impl Tool for ApplyPatch {
    fn definition(&self) -> ToolDefinition {
        definition("apply_patch", "Create or replace one UTF-8 file atomically. 'content' is the complete new file, not unified-diff syntax. Existing parent directory required; new content at most 256 KiB. A unified diff is reviewed before writing.", Effect::Write,
            json!({"path":{"type":"string"},"content":{"type":"string"}}), &["path","content"])
    }
    async fn prepare(&self, context: &ToolContext, arguments: Value) -> Result<PreparedTool> {
        Ok(prepare_patch(context, arguments)?.0)
    }
    fn undo_tool(&self, record: EditRecord) -> Option<Arc<dyn Tool>> {
        Some(undo_tool(record))
    }
    async fn execute(
        &self,
        context: &ToolContext,
        p: PreparedTool,
        _events: mpsc::Sender<ToolEvent>,
        cancel: CancellationToken,
    ) -> Result<ToolOutput> {
        cancel_check(&cancel)?;
        let (actual, path, before, content) = prepare_patch(context, p.arguments.clone())?;
        check_prepared(&p, &actual)?;
        let entry = paths::Entry::new(context, &path, true)?;
        cancel_check(&cancel)?;
        entry.replace(before.as_deref(), Some(content.as_bytes()))?;
        Ok(ToolOutput {
            content: format!("Updated {}", path.display()),
            edits: vec![EditRecord {
                path,
                before,
                after_hash: Some(hash(content.as_bytes())),
            }],
            ..Default::default()
        })
    }
}

struct UndoEdit {
    record: EditRecord,
}

/// Construct an approved host operation from a host-owned edit checkpoint.
/// Never deserialize the record from model tool arguments.
pub fn undo_tool(record: EditRecord) -> Arc<dyn Tool> {
    Arc::new(UndoEdit { record })
}

fn undo_state(context: &ToolContext, record: &EditRecord) -> Result<(PathBuf, Option<Vec<u8>>)> {
    let path = paths::relative(
        record
            .path
            .to_str()
            .ok_or_else(|| Error::Invalid("invalid checkpoint path".into()))?,
    )?;
    let current = paths::Entry::new(context, &path, true)?.read()?;
    if current.as_deref().map(hash) != record.after_hash {
        return Err(Error::Invalid(
            "file has changed since Bund's edit; undo refused".into(),
        ));
    }
    Ok((path, current))
}

/// Low-level guarded restoration. Call only after host authorization.
pub async fn guarded_undo(context: &ToolContext, record: &EditRecord) -> Result<ToolOutput> {
    let (path, current) = undo_state(context, record)?;
    paths::Entry::new(context, &path, true)?
        .replace(current.as_deref(), record.before.as_deref())?;
    Ok(ToolOutput {
        content: format!("Restored {}", path.display()),
        ..Default::default()
    })
}

#[async_trait]
impl Tool for UndoEdit {
    fn definition(&self) -> ToolDefinition {
        definition(
            "undo_edit",
            "Restore a host-owned edit checkpoint if the file still matches Bund's last write.",
            Effect::Write,
            json!({}),
            &[],
        )
    }
    async fn prepare(&self, context: &ToolContext, arguments: Value) -> Result<PreparedTool> {
        if arguments != json!({}) {
            return Err(Error::Invalid("undo takes no model arguments".into()));
        }
        let (path, current) = undo_state(context, &self.record)?;
        prepared(
            json!({}),
            diff(&path, current.as_deref(), self.record.before.as_deref())?,
            json!({"root":paths::root(context)?,"record":self.record}),
        )
    }
    async fn execute(
        &self,
        context: &ToolContext,
        p: PreparedTool,
        _events: mpsc::Sender<ToolEvent>,
        cancel: CancellationToken,
    ) -> Result<ToolOutput> {
        cancel_check(&cancel)?;
        check_prepared(&p, &self.prepare(context, p.arguments.clone()).await?)?;
        guarded_undo(context, &self.record).await
    }
}
