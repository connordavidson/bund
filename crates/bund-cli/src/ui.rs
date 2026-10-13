use crate::{
    config::{require_model, update_profile},
    output::{clean, FinalText, Sanitizer, StreamedText},
};
use anyhow::{Context, Result};
use bund_core::{
    ApprovalDecision, ApprovalRequest, EventKind, Mode, Policy, Role, RunStatus, SessionEvent,
    SessionHandle, SessionSnapshot,
};
use crossterm::{
    event::{DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
};
use ratatui::{
    layout::{Constraint, Layout},
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame, TerminalOptions, Viewport,
};
use ratatui_textarea::TextArea;
use std::{
    collections::{HashSet, VecDeque},
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, IsTerminal, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const COMMANDS: &[&str] = &[
    "/help",
    "/plan",
    "/act",
    "/model",
    "/diff",
    "/undo",
    "/compact",
    "/check",
    "/skill",
    "/accept-edits",
    "/exit",
];
const HELP: &str = "Commands: /plan /act /model [provider] MODEL /diff /undo /compact /check /skill PATH /accept-edits on|off /exit\nEnter submits; Alt+Enter or Ctrl+J inserts a newline; Tab completes commands and @paths.\nEsc or Ctrl+C cancels work. Approval: Ctrl+Y approves once; Ctrl+N or Esc denies.\nType while work runs; busy submission keeps your draft. Up/Down recall history on a single line.\n@file references tell the agent which workspace files to inspect.\nPermissions are application checks, not an operating-system sandbox. Shell actions cannot be undone.";

/// All mutable screen state lives here; approval controls never borrow the draft buffer.
struct Screen {
    editor: TextArea<'static>,
    snapshot: SessionSnapshot,
    pending: Option<ApprovalRequest>,
    tail: String,
    sanitizer: Sanitizer,
    streamed: StreamedText,
    output_calls: HashSet<String>,
    hint: String,
    history: History,
}

impl Screen {
    fn new(snapshot: SessionSnapshot, history: History) -> Self {
        let mut editor = TextArea::default();
        editor.set_cursor_line_style(Style::default());
        editor.set_wrap_mode(ratatui_textarea::WrapMode::WordOrGlyph);
        editor.set_placeholder_text("Ask bund about this workspace…");
        Self {
            pending: snapshot.pending_approval.clone(),
            snapshot,
            editor,
            tail: String::new(),
            sanitizer: Sanitizer::default(),
            streamed: StreamedText::default(),
            output_calls: HashSet::new(),
            hint: "Enter send · Alt+Enter newline · /help".into(),
            history,
        }
    }
    fn draft(&self) -> String {
        self.editor.lines().join("\n")
    }
    fn set_draft(&mut self, text: &str) {
        self.editor = TextArea::from(text.split('\n'));
        self.editor.set_cursor_line_style(Style::default());
        self.editor
            .set_wrap_mode(ratatui_textarea::WrapMode::WordOrGlyph);
        self.editor
            .move_cursor(ratatui_textarea::CursorMove::Bottom);
        self.editor.move_cursor(ratatui_textarea::CursorMove::End);
    }
    fn render(&self, frame: &mut Frame) {
        let area = frame.area();
        let regions = Layout::vertical([
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);
        let tail_rows = wrap_lines(&self.tail, area.width as usize);
        let tail = tail_rows
            .into_iter()
            .rev()
            .take(2)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        frame.render_widget(Paragraph::new(tail).wrap(Wrap { trim: false }), regions[0]);
        let model = if self.snapshot.profile.model.is_empty() {
            "model not configured"
        } else {
            &self.snapshot.profile.model
        };
        let status = format!(
            "{:?} · {}\n{} / {} · {} tokens",
            self.snapshot.status,
            policy_status(&self.snapshot.policy),
            self.snapshot.profile.provider,
            model,
            self.snapshot.usage.input_tokens + self.snapshot.usage.output_tokens
        );
        frame.render_widget(
            Paragraph::new(clean(&status)).style(Style::default().fg(Color::Cyan)),
            regions[1],
        );
        if let Some(pending) = &self.pending {
            let prompt = format!("Permission: {}\nCtrl+Y: approve once  Ctrl+N / Esc: deny\nYour draft is preserved.", clean(&pending.tool_call.name));
            frame.render_widget(
                Paragraph::new(prompt).wrap(Wrap { trim: false }).block(
                    Block::default()
                        .borders(Borders::TOP)
                        .title("Approval required"),
                ),
                regions[2],
            );
        } else {
            frame.render_widget(&self.editor, regions[2]);
        }
        frame.render_widget(
            Paragraph::new(clean(&self.hint)).style(Style::default().fg(Color::DarkGray)),
            regions[3],
        );
    }
}

fn policy_status(policy: &Policy) -> String {
    if policy.mode == Mode::Plan {
        return "Plan · edits blocked · shell blocked".into();
    }
    let edits = if policy.accept_edits {
        "auto"
    } else {
        "reviewed"
    };
    let grants = if policy.allowed_commands.is_empty() {
        String::new()
    } else {
        format!(" ({} exact grants)", policy.allowed_commands.len())
    };
    format!("Act · edits {edits} · shell reviewed{grants}")
}

struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(std::io::stdout(), DisableBracketedPaste);
        ratatui::restore();
    }
}

pub async fn run(
    handle: SessionHandle,
    history_path: PathBuf,
    first_prompt: Option<String>,
) -> Result<()> {
    let result = run_inner(handle.clone(), history_path, first_prompt).await;
    handle.cancel().await?;
    // Let the executor reap children and durably record cancellation before the
    // Tokio runtime is shut down by main, including after a terminal I/O error.
    tokio::time::timeout(Duration::from_secs(5), async {
        while handle.snapshot().await?.status.is_active() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("timed out waiting for cancellation cleanup")??;
    result
}

async fn run_inner(
    handle: SessionHandle,
    history_path: PathBuf,
    first_prompt: Option<String>,
) -> Result<()> {
    anyhow::ensure!(
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        "interactive mode requires a terminal; use bund run for pipes and scripts"
    );
    let attachment = handle.attach().await?;
    let mut events = attachment.events;
    let mut cursor = attachment.snapshot.cursor;
    let history = History::load(history_path)?;
    let mut screen = Screen::new(attachment.snapshot, history);
    let mut terminal = ratatui::init_with_options(TerminalOptions {
        viewport: Viewport::Inline(9),
    });
    let _guard = TerminalGuard;
    execute!(std::io::stdout(), EnableBracketedPaste)?;
    insert(&mut terminal, &format!("bund · session {}\nWorkspace: {}\n/help for commands. Application permissions do not sandbox shell commands.", screen.snapshot.id, screen.snapshot.workspace.display()))?;
    for message in &screen.snapshot.messages {
        if matches!(message.role, Role::User | Role::Assistant) && !message.text.is_empty() {
            insert(
                &mut terminal,
                &format!(
                    "{} {}",
                    if message.role == Role::User {
                        ">"
                    } else {
                        "bund:"
                    },
                    message.text
                ),
            )?;
        }
    }
    if let Some(pending) = &screen.pending {
        insert(&mut terminal, &pending.preview)?;
    }
    if let Some(prompt) = first_prompt {
        screen.set_draft(&prompt);
        if let Err(error) = submit(&handle, &mut screen).await {
            screen.hint = error.to_string();
        }
    }
    let mut ticker = tokio::time::interval(Duration::from_millis(33));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut pending_events = VecDeque::new();
    let mut dirty = true;
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    'ui: loop {
        if let Some(event) = pending_events.pop_front() {
            process_event(&handle, &mut screen, &mut terminal, event, &mut cursor).await?;
            dirty = true;
            continue;
        }
        tokio::select! {
            _ = &mut shutdown => { handle.cancel().await?; break; },
            _ = ticker.tick() => {
                // Ratatui's inline insertion and resize may synchronously request
                // the cursor position. A background EventStream would compete
                // for Crossterm's input lock and can make those queries time out.
                // This loop owns both input and rendering; zero-time polling
                // never waits for input, and the bounded batch keeps work fair.
                for _ in 0..32 {
                    if !crossterm::event::poll(Duration::ZERO)? { break; }
                    match crossterm::event::read()? {
                        Event::Key(key) if key.kind != crossterm::event::KeyEventKind::Release => {
                            match key_action(&handle, &mut screen, &mut terminal, key).await {
                                Ok(true) => break 'ui,
                                Ok(false) => {},
                                Err(error) => screen.hint = error.to_string(),
                            }
                            dirty = true;
                        },
                        Event::Paste(text) if screen.pending.is_none() => { screen.editor.insert_str(clean(&text)); dirty = true; },
                        Event::Resize(_, _) => dirty = true,
                        _ => {},
                    }
                }
                if dirty { terminal.draw(|frame| screen.render(frame))?; dirty = false; }
            },
            received = events.recv() => match received {
                Ok(event) => { process_event(&handle, &mut screen, &mut terminal, event, &mut cursor).await?; dirty = true; },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    pending_events.extend(handle.events_after(cursor, 512).await?);
                    screen.hint = "Recovered delayed session events".into();
                },
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
        }
    }
    flush_tail(&mut screen, &mut terminal)?;
    terminal.clear()?;
    terminal.show_cursor()?;
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! { _ = terminate.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

async fn process_event(
    handle: &SessionHandle,
    screen: &mut Screen,
    terminal: &mut ratatui::DefaultTerminal,
    event: SessionEvent,
    cursor: &mut u64,
) -> Result<()> {
    if event.sequence <= *cursor {
        return Ok(());
    }
    if event.sequence > *cursor + 1 {
        // A bounded replay page might leave a gap before the next broadcast event.
        let mut next = *cursor;
        while next + 1 < event.sequence {
            let before = next;
            let missing = handle.events_after(next, 512).await?;
            if missing.is_empty() {
                anyhow::bail!("session event history has a gap after {next}");
            }
            for old in missing
                .into_iter()
                .filter(|old| old.sequence < event.sequence)
            {
                next = old.sequence;
                apply_event(handle, screen, terminal, old).await?;
                *cursor = next;
            }
            if next == before {
                anyhow::bail!("session event history has a gap after {next}");
            }
        }
    }
    *cursor = event.sequence;
    apply_event(handle, screen, terminal, event).await
}

async fn apply_event(
    handle: &SessionHandle,
    screen: &mut Screen,
    terminal: &mut ratatui::DefaultTerminal,
    event: SessionEvent,
) -> Result<()> {
    match event.kind {
        EventKind::RunStarted { prompt } => {
            flush_tail(screen, terminal)?;
            screen.streamed = StreamedText::default();
            screen.sanitizer = Sanitizer::default();
            screen.snapshot.status = RunStatus::Running;
            screen.hint = "Working · Esc cancels · draft stays editable".into();
            screen.output_calls.clear();
            insert(terminal, &format!("> {prompt}"))?;
        }
        EventKind::TextDelta { text } => {
            screen.streamed.record(&text);
            screen.tail.push_str(&screen.sanitizer.push(&text));
            if let Some(end) = screen.tail.rfind('\n') {
                let complete = screen.tail[..end].to_owned();
                screen.tail.drain(..=end);
                insert(terminal, &complete)?;
            }
            // A model can stream a very long unbroken paragraph. Commit display rows
            // while keeping a small mutable tail, rather than growing the UI buffer.
            if screen.tail.len() > 4096 {
                let boundary = screen
                    .tail
                    .char_indices()
                    .map(|(i, _)| i)
                    .take_while(|i| *i <= 3072)
                    .last()
                    .unwrap_or(0);
                let complete = screen.tail[..boundary].to_owned();
                screen.tail.drain(..boundary);
                insert(terminal, &complete)?;
            }
        }
        EventKind::AssistantMessage { text } => {
            match screen.streamed.finish(&text) {
                FinalText::Append(suffix) => {
                    screen.tail.push_str(&screen.sanitizer.push(suffix));
                    flush_tail(screen, terminal)?;
                }
                FinalText::Replace(text) => {
                    flush_tail(screen, terminal)?;
                    insert(terminal, &format!("Final response:\n{text}"))?;
                }
            }
            screen.sanitizer = Sanitizer::default();
        }
        EventKind::ApprovalRequired(request) => {
            flush_tail(screen, terminal)?;
            insert(
                terminal,
                &format!(
                    "Approval required: {}\n{}",
                    request.tool_call.name, request.preview
                ),
            )?;
            screen.pending = Some(request);
            screen.snapshot.status = RunStatus::AwaitingApproval;
            screen.hint =
                "Ctrl+Y approve once · Ctrl+N deny · approval never expires automatically".into();
        }
        EventKind::ApprovalResolved { .. } => {
            screen.pending = None;
            screen.snapshot.status = RunStatus::Running;
        }
        EventKind::ToolStarted { call } => {
            screen.hint = format!("Running {} · Esc cancels", call.name);
            insert(terminal, &format!("• {}", call.name))?;
        }
        EventKind::ToolOutput { call_id, text } => {
            screen.output_calls.insert(call_id);
            insert(terminal, &text)?;
        }
        EventKind::ToolFinished(result) => {
            if !screen.output_calls.contains(&result.call_id) && !result.content.is_empty() {
                let content = if result.name == "git_diff" || result.name == "git_status" {
                    result.content
                } else {
                    excerpt(&result.content, 20)
                };
                insert(terminal, &content)?;
            }
            if result.is_error {
                screen.hint = format!("{} failed; see output above", result.name);
            }
        }
        EventKind::ContextCompacted { summary } => {
            insert(terminal, &format!("Context compacted:\n{summary}"))?
        }
        EventKind::SettingsChanged => screen.snapshot = handle.snapshot().await?,
        EventKind::RunFinished {
            status,
            message,
            validation,
        } => {
            flush_tail(screen, terminal)?;
            screen.pending = None;
            screen.streamed = StreamedText::default();
            screen.sanitizer = Sanitizer::default();
            screen.snapshot = handle.snapshot().await?;
            screen.hint = format!("{status:?} · checks {validation:?} · {}", clean(&message));
            if status != RunStatus::Completed {
                insert(terminal, &screen.hint)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn flush_tail(screen: &mut Screen, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
    if !screen.tail.is_empty() {
        insert(terminal, &std::mem::take(&mut screen.tail))?;
    }
    Ok(())
}

async fn key_action(
    handle: &SessionHandle,
    screen: &mut Screen,
    terminal: &mut ratatui::DefaultTerminal,
    key: KeyEvent,
) -> Result<bool> {
    if let Some(request) = &screen.pending {
        if let Some(decision) = approval_key(key) {
            handle.answer_approval(&request.id, decision).await?;
            screen.pending = None;
        } else if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            handle.cancel().await?;
        }
        return Ok(false);
    }
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => {
            if screen.snapshot.status.is_active() {
                handle.cancel().await?;
            } else {
                screen.hint = "Enter sends your draft · /exit quits".into();
            }
        }
        (KeyCode::Char('c'), mods) if mods.contains(KeyModifiers::CONTROL) => {
            if screen.snapshot.status.is_active() {
                handle.cancel().await?;
            } else if screen.draft().is_empty() {
                return Ok(true);
            } else {
                screen.set_draft("");
                screen.hint = "Draft cleared · Ctrl+C again exits".into();
            }
        }
        (KeyCode::Char('d'), mods)
            if mods.contains(KeyModifiers::CONTROL) && screen.draft().is_empty() =>
        {
            handle.cancel().await?;
            return Ok(true);
        }
        (KeyCode::Enter, mods)
            if mods.contains(KeyModifiers::ALT) || mods.contains(KeyModifiers::SHIFT) =>
        {
            screen.editor.insert_newline();
        }
        (KeyCode::Char('j'), mods) if mods.contains(KeyModifiers::CONTROL) => {
            screen.editor.insert_newline();
        }
        (KeyCode::Enter, _) => {
            let draft = screen.draft();
            if draft.trim() == "/exit" {
                handle.cancel().await?;
                return Ok(true);
            }
            if draft.trim() == "/help" {
                insert(terminal, HELP)?;
                screen.set_draft("");
            } else {
                submit(handle, screen).await?;
            }
        }
        (KeyCode::Tab, _) => complete(screen),
        (KeyCode::Up, mods) if mods.is_empty() && screen.editor.lines().len() == 1 => {
            let draft = screen.draft();
            if let Some(text) = screen.history.previous(&draft) {
                screen.set_draft(&text);
            }
        }
        (KeyCode::Down, mods) if mods.is_empty() && screen.editor.lines().len() == 1 => {
            if let Some(text) = screen.history.next() {
                screen.set_draft(&text);
            }
        }
        _ => {
            screen.editor.input(key);
        }
    }
    Ok(false)
}

fn approval_key(key: KeyEvent) -> Option<ApprovalDecision> {
    match key.code {
        KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            Some(ApprovalDecision::ApproveOnce)
        }
        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            Some(ApprovalDecision::Deny)
        }
        KeyCode::Esc => Some(ApprovalDecision::Deny),
        _ => None,
    }
}

async fn submit(handle: &SessionHandle, screen: &mut Screen) -> Result<()> {
    let draft = screen.draft();
    let text = draft.trim();
    if text.is_empty() {
        return Ok(());
    }
    if screen.snapshot.status.is_active() {
        anyhow::bail!("A run is active. Your draft is preserved; Esc cancels the run.");
    }
    let (command, args) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    let args = args.trim();
    match command {
        "/plan" => handle.set_mode(Mode::Plan).await?,
        "/act" => handle.set_mode(Mode::Act).await?,
        "/model" => {
            anyhow::ensure!(
                !args.is_empty(),
                "Current model: {} / {}. Use /model [provider] MODEL",
                screen.snapshot.profile.provider,
                screen.snapshot.profile.model
            );
            let parts: Vec<_> = args.split_whitespace().collect();
            let profile = match parts.as_slice() {
                [model] => update_profile(&screen.snapshot.profile, None, Some(model), None),
                [provider, model] if ["openai", "anthropic"].contains(provider) => {
                    update_profile(&screen.snapshot.profile, Some(provider), Some(model), None)
                }
                _ => anyhow::bail!("Use /model MODEL or /model openai|anthropic MODEL"),
            };
            handle.set_profile(profile).await?;
        }
        "/accept-edits" => {
            let enabled = match args {
                "on" => true,
                "off" => false,
                _ => anyhow::bail!("Use /accept-edits on or /accept-edits off"),
            };
            handle.set_accept_edits(enabled).await?;
        }
        "/diff" => {
            handle.run_tool("git_diff", serde_json::json!({})).await?;
        }
        "/undo" => {
            handle.undo().await?;
        }
        "/compact" => {
            require_model(&screen.snapshot.profile)?;
            handle.compact().await?;
        }
        "/check" => {
            handle.run_checks().await?;
        }
        "/skill" => {
            require_model(&screen.snapshot.profile)?;
            anyhow::ensure!(!args.is_empty(), "Use /skill PATH to a SKILL.md file");
            handle.load_skill(Path::new(args)).await?;
        }
        command if command.starts_with('/') => {
            anyhow::bail!("Unknown command {command}; /help lists commands")
        }
        _ => {
            require_model(&screen.snapshot.profile)?;
            handle.submit(draft.clone()).await?;
        }
    }
    // Only clear and record after the engine accepts the request. Busy/errors leave
    // both the draft and the current cursor/selection intact.
    let history_error = screen.history.record(&draft).err();
    screen.set_draft("");
    screen.snapshot = handle.snapshot().await?;
    screen.hint = history_error
        .map(|error| format!("Accepted; history could not be saved: {error}"))
        .unwrap_or_else(|| "Accepted · Esc cancels active work".into());
    Ok(())
}

fn complete(screen: &mut Screen) {
    let draft = screen.draft();
    if draft.starts_with('/') && !draft.contains(char::is_whitespace) {
        let candidates: Vec<_> = COMMANDS
            .iter()
            .filter(|name| name.starts_with(&draft))
            .copied()
            .collect();
        if candidates.len() == 1 {
            screen.set_draft(&format!("{} ", candidates[0]));
        } else {
            screen.hint = candidates.join("  ");
        }
    } else if let Some(last) = draft
        .split_whitespace()
        .last()
        .filter(|last| last.starts_with('@'))
    {
        let reference = &last[1..];
        let (directory, prefix) = reference.rsplit_once('/').unwrap_or(("", reference));
        let parent = screen.snapshot.workspace.join(directory);
        let Ok(parent) = parent.canonicalize() else {
            return;
        };
        if !parent.starts_with(&screen.snapshot.workspace) {
            return;
        }
        let Ok(entries) = std::fs::read_dir(parent) else {
            return;
        };
        let mut candidates = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_str()?.to_owned();
                if !name.starts_with(prefix)
                    || matches!(name.as_str(), ".git" | "node_modules" | "target")
                    || clean(&name) != name
                {
                    return None;
                }
                Some(format!(
                    "{}{}",
                    name,
                    if entry.file_type().ok()?.is_dir() {
                        "/"
                    } else {
                        ""
                    }
                ))
            })
            .take(100)
            .collect::<Vec<_>>();
        candidates.sort();
        if candidates.len() == 1 {
            screen.set_draft(&format!("{}{}", draft, &candidates[0][prefix.len()..]));
        } else {
            screen.hint = candidates.join("  ");
        }
    }
}

fn insert(terminal: &mut ratatui::DefaultTerminal, text: &str) -> Result<()> {
    let width = terminal.size()?.width.max(1) as usize;
    let lines = wrap_lines(&clean(text), width);
    for chunk in lines.chunks(200) {
        terminal.insert_before(chunk.len() as u16, |buffer| {
            use ratatui::widgets::Widget;
            Paragraph::new(chunk.iter().cloned().map(Line::from).collect::<Vec<_>>())
                .render(buffer.area, buffer);
        })?;
    }
    Ok(())
}

fn wrap_lines(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut result = Vec::new();
    for line in text.split('\n') {
        let mut current = String::new();
        let mut length = 0;
        for grapheme in line.graphemes(true) {
            let size = UnicodeWidthStr::width(grapheme);
            if length + size > width && !current.is_empty() {
                result.push(std::mem::take(&mut current));
                length = 0;
            }
            if size > width {
                current.push('�');
                length += 1;
            } else {
                current.push_str(grapheme);
                length += size;
            }
        }
        result.push(current);
    }
    result
}

fn excerpt(text: &str, count: usize) -> String {
    let mut lines = text.lines();
    let mut output = lines.by_ref().take(count).collect::<Vec<_>>().join("\n");
    if lines.next().is_some() {
        output.push_str("\n… more output is saved in this session");
    }
    output
}

struct History {
    path: PathBuf,
    entries: Vec<String>,
    position: Option<usize>,
    draft: String,
}
impl History {
    fn load(path: PathBuf) -> Result<Self> {
        let mut entries = Vec::new();
        if path.exists() {
            let file = File::open(&path)?;
            anyhow::ensure!(
                file.metadata()?.len() <= 16 * 1024 * 1024,
                "prompt history is too large: {}",
                path.display()
            );
            for line in BufReader::new(file).lines() {
                if let Ok(value) = serde_json::from_str::<String>(&line?) {
                    entries.push(value);
                }
            }
            if entries.len() > 500 {
                entries.drain(..entries.len() - 500);
            }
        }
        Ok(Self {
            path,
            entries,
            position: None,
            draft: String::new(),
        })
    }
    fn record(&mut self, text: &str) -> Result<()> {
        if self.entries.last().map(String::as_str) != Some(text) {
            if let Some(parent) = self.path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut options = OpenOptions::new();
            options.create(true).append(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&self.path).context("save prompt history")?;
            serde_json::to_writer(&mut file, text)?;
            writeln!(file)?;
            self.entries.push(text.into());
            if self.entries.len() > 500 {
                self.entries.remove(0);
            }
        }
        self.position = None;
        Ok(())
    }
    fn previous(&mut self, draft: &str) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let position = match self.position {
            Some(p) => p.saturating_sub(1),
            None => {
                self.draft = draft.into();
                self.entries.len() - 1
            }
        };
        self.position = Some(position);
        Some(self.entries[position].clone())
    }
    fn next(&mut self) -> Option<String> {
        let position = self.position?;
        if position + 1 == self.entries.len() {
            self.position = None;
            Some(self.draft.clone())
        } else {
            self.position = Some(position + 1);
            Some(self.entries[position + 1].clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn policy_status_uses_saved_grants_and_plan_restrictions() {
        let mut policy = Policy {
            mode: Mode::Act,
            accept_edits: true,
            allowed_commands: vec!["cargo test".into()],
        };
        assert_eq!(
            policy_status(&policy),
            "Act · edits auto · shell reviewed (1 exact grants)"
        );
        policy.mode = Mode::Plan;
        assert_eq!(
            policy_status(&policy),
            "Plan · edits blocked · shell blocked"
        );
        policy.mode = Mode::Act;
        policy.accept_edits = false;
        policy.allowed_commands.clear();
        assert_eq!(
            policy_status(&policy),
            "Act · edits reviewed · shell reviewed"
        );
    }
    #[test]
    fn regular_typing_cannot_accidentally_approve() {
        assert_eq!(
            approval_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            approval_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL)),
            Some(ApprovalDecision::ApproveOnce)
        );
        assert_eq!(
            approval_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(ApprovalDecision::Deny)
        );
    }
    #[test]
    fn wide_unicode_and_combining_marks_wrap_without_splitting() {
        assert_eq!(wrap_lines("ab世界e\u{301}", 4), ["ab世", "界e\u{301}"]);
        assert_eq!(wrap_lines("\n", 4), ["", ""]);
        assert!(wrap_lines("世界", 1)
            .iter()
            .all(|line| UnicodeWidthStr::width(line.as_str()) <= 1));
    }
    #[test]
    fn history_restores_draft_and_keeps_multiline_prompts() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = History::load(dir.path().join("history")).unwrap();
        h.record("first\nsecond").unwrap();
        assert_eq!(h.previous("draft"), Some("first\nsecond".into()));
        assert_eq!(h.next(), Some("draft".into()));
        assert_eq!(History::load(h.path).unwrap().entries, ["first\nsecond"]);
    }
}
