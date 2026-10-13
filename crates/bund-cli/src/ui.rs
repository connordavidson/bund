use crate::{
    config::{require_model, update_profile},
    output::clean,
};
use anyhow::{Context, Result};
use bund_core::{
    ApprovalDecision, ApprovalRequest, EventKind, Mode, Policy, RunStatus, SessionEvent,
    SessionHandle, SessionSnapshot, Transcript,
};
use crossterm::{
    event::{DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, BorderType, Borders, Paragraph},
    Frame,
};
use ratatui_textarea::TextArea;
use std::{
    collections::VecDeque,
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, IsTerminal, Write},
    path::{Path, PathBuf},
    time::Duration,
};
#[path = "ui_render.rs"]
mod render;
use render::{Anchor, Document};

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
const HELP: &str = "Commands: /plan /act /model [provider] MODEL /diff /undo /compact /check /skill PATH /accept-edits on|off /exit\nEnter submits; Alt+Enter or Ctrl+J inserts a newline; Tab completes commands and @paths.\nEsc or Ctrl+C cancels work. Approval: Ctrl+Y approves once; Ctrl+N or Esc denies.\nCtrl+O expands activity; PgUp/PgDn scroll; Ctrl+End returns to latest output.\nType while work runs; busy submission keeps your draft. Up/Down recall history on a single line.\n@file references tell the agent which workspace files to inspect.\nPermissions are application checks, not an operating-system sandbox. Shell actions cannot be undone.";

/// One state owner keeps drafts independent from streaming and approval controls.
struct Screen {
    editor: TextArea<'static>,
    snapshot: SessionSnapshot,
    pending: Option<ApprovalRequest>,
    transcript: Transcript,
    expanded: bool,
    follow: bool,
    anchor: Option<Anchor>,
    document: Document,
    document_dirty: bool,
    document_width: u16,
    viewport_height: usize,
    top: usize,
    approval_scroll: usize,
    history_ceiling: Option<u64>,
    history_task: Option<tokio::task::JoinHandle<Result<HistoryLoad>>>,
    history_follow_pending: bool,
    hint: String,
    notices: VecDeque<String>,
    history: History,
}

#[derive(Clone, Copy)]
enum HistoryTarget {
    Top,
    Bottom,
    Latest,
}

struct HistoryLoad {
    transcript: Transcript,
    ceiling: u64,
    target: HistoryTarget,
}

impl Drop for Screen {
    fn drop(&mut self) {
        if let Some(task) = self.history_task.take() {
            task.abort();
        }
    }
}

impl Screen {
    fn build_document(&self, width: usize) -> Document {
        use bund_core::{ResponsePurpose, TranscriptToolStatus};
        let mut document = Document::default();
        if width < 4 {
            return document;
        }
        if self.transcript.runs().is_empty() {
            document.section("welcome", "welcome", render::markdown("Ask Bund to inspect or change this workspace.\n/help lists commands. Edits and shell commands require approval.", width));
        }
        for run in self.transcript.runs() {
            let mut prompt = vec![Line::styled(
                "You",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )];
            prompt.extend(render::plain(&run.prompt, width, Style::default()));
            prompt.push(Line::default());
            document.section(&run.id, "prompt", prompt);

            let latest = run
                .responses
                .iter()
                .rev()
                .find(|r| r.purpose == ResponsePurpose::Conversation);
            let text = run
                .output
                .as_deref()
                .or_else(|| {
                    latest
                        .filter(|r| {
                            !r.complete || (run.status.is_active() && r.tool_calls.is_empty())
                        })
                        .map(|r| r.text.as_str())
                })
                .unwrap_or("");
            let mut answer = vec![Line::styled(
                "Bund",
                Style::default().add_modifier(Modifier::BOLD),
            )];
            if !text.is_empty() {
                answer.extend(render::markdown(text, width));
            } else if run.status.is_active() {
                answer.push(Line::styled(
                    "Working…",
                    Style::default().fg(Color::DarkGray),
                ));
            }
            if latest.is_some_and(|r| !r.complete) && !run.status.is_active() {
                answer.push(Line::styled(
                    "Incomplete response",
                    Style::default().fg(Color::Yellow),
                ));
            }
            document.section(&run.id, "answer", answer);

            let activity = format!("Activity · {} tools", run.tools.len());
            if self.expanded {
                let inner = width.saturating_sub(4).max(1);
                let mut border = render::bordered(&activity, Vec::new(), width);
                let footer = border.pop().unwrap();
                document.section(&run.id, "activity", border);
                // Merge response rounds and executions by their journal position.
                let mut items = run
                    .responses
                    .iter()
                    .enumerate()
                    .map(|(i, r)| (r.start_sequence, true, i))
                    .chain(
                        run.tools
                            .iter()
                            .enumerate()
                            .map(|(i, t)| (t.start_sequence, false, i)),
                    )
                    .collect::<Vec<_>>();
                items.sort_by_key(|item| item.0);
                if run.responses.iter().all(|r| r.reasoning.is_empty()) {
                    let mut body = render::plain(
                        "No reasoning summary provided",
                        inner,
                        Style::default().fg(Color::DarkGray),
                    );
                    body.push(Line::default());
                    document.section(
                        &run.id,
                        "reasoning-unavailable",
                        render::bordered_body(body, width),
                    );
                }
                for (_, response, index) in items {
                    let mut body = Vec::new();
                    let section;
                    if response {
                        let r = &run.responses[index];
                        section = format!("response-{}", r.id);
                        if r.purpose == ResponsePurpose::Compaction {
                            body.extend(render::plain(
                                if r.complete {
                                    "Context compacted"
                                } else {
                                    "Compacting context…"
                                },
                                inner,
                                Style::default().fg(Color::DarkGray),
                            ));
                            document.section(&run.id, &section, render::bordered_body(body, width));
                            continue;
                        }
                        if !r.reasoning.is_empty() {
                            body.extend(render::plain(
                                if r.complete {
                                    "Reasoning summary"
                                } else {
                                    "Reasoning summary · partial"
                                },
                                inner,
                                Style::default().add_modifier(Modifier::BOLD),
                            ));
                            for text in &r.reasoning {
                                body.extend(render::markdown(text, inner));
                            }
                        }
                        if !r.is_final && !r.text.is_empty() && r.text != text {
                            body.extend(render::plain(
                                if r.complete {
                                    "Update"
                                } else {
                                    "Update · partial"
                                },
                                inner,
                                Style::default().add_modifier(Modifier::BOLD),
                            ));
                            body.extend(render::markdown(&r.text, inner));
                        }
                        if r.truncated {
                            body.extend(render::plain(
                                "… response details shortened in this history window",
                                inner,
                                Style::default().fg(Color::DarkGray),
                            ));
                        }
                    } else {
                        let tool = &run.tools[index];
                        section = format!("tool-{}", tool.call.id);
                        body.extend(render::plain(
                            &format!("{} · {:?}", tool.call.name, tool.status),
                            inner,
                            Style::default().add_modifier(Modifier::BOLD),
                        ));
                        body.extend(render::plain(
                            &tool.call.arguments.to_string(),
                            inner,
                            Style::default().fg(Color::DarkGray),
                        ));
                        if !tool.preview.is_empty() {
                            body.extend(render::plain(&tool.preview, inner, Style::default()));
                        }
                        if !tool.output.is_empty() && tool.output != tool.preview {
                            body.extend(render::plain(&tool.output, inner, Style::default()));
                        }
                        if tool.truncated {
                            body.extend(render::plain(
                                "… output shortened; full permitted output is saved locally",
                                inner,
                                Style::default().fg(Color::DarkGray),
                            ));
                        }
                    }
                    body.push(Line::default());
                    document.section(&run.id, &section, render::bordered_body(body, width));
                }
                document.section(&run.id, "activity-end", vec![footer]);
            } else {
                document.section(
                    &run.id,
                    "activity",
                    render::plain(
                        &format!("▸ {activity} · Ctrl+O to expand"),
                        width,
                        Style::default().fg(Color::DarkGray),
                    ),
                );
            }
            let mut outcome = Vec::new();
            for tool in &run.tools {
                if matches!(
                    tool.status,
                    TranscriptToolStatus::Failed | TranscriptToolStatus::Denied
                ) {
                    let excerpt = clean(&tool.output)
                        .lines()
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(200)
                        .collect::<String>();
                    outcome.extend(render::plain(
                        &format!("{} · {:?}: {}", tool.call.name, tool.status, excerpt),
                        width,
                        Style::default().fg(Color::Yellow),
                    ));
                }
            }
            if let Some(error) = &run.error {
                outcome.extend(render::plain(
                    error,
                    width,
                    Style::default().fg(Color::Yellow),
                ));
            }
            if !run.status.is_active() {
                let validation = match run.validation {
                    bund_core::ValidationStatus::NotRun => "not run",
                    bund_core::ValidationStatus::Passed => "passed",
                    bund_core::ValidationStatus::Failed => "failed",
                };
                outcome.extend(render::plain(
                    &format!("{:?} · checks {validation}", run.status),
                    width,
                    Style::default().fg(Color::DarkGray),
                ));
            }
            if run.truncated {
                outcome.extend(render::plain(
                    "Some details are outside this bounded history window.",
                    width,
                    Style::default().fg(Color::DarkGray),
                ));
            }
            outcome.push(Line::default());
            document.section(&run.id, "outcome", outcome);
        }
        for (i, notice) in self.notices.iter().enumerate() {
            document.section("notice", &i.to_string(), render::markdown(notice, width));
        }
        document
    }
    fn new(snapshot: SessionSnapshot, history: History) -> Self {
        let mut editor = TextArea::default();
        editor.set_cursor_line_style(Style::default());
        editor.set_wrap_mode(ratatui_textarea::WrapMode::WordOrGlyph);
        editor.set_placeholder_text("Ask bund about this workspace…");
        Self {
            pending: snapshot.pending_approval.clone(),
            snapshot,
            editor,
            transcript: history_window(),
            expanded: false,
            follow: true,
            anchor: None,
            document: Document::default(),
            document_dirty: true,
            document_width: 0,
            viewport_height: 0,
            top: 0,
            approval_scroll: 0,
            history_ceiling: None,
            history_task: None,
            history_follow_pending: false,
            hint: "Enter send · Alt+Enter newline · /help".into(),
            notices: VecDeque::new(),
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
            .set_placeholder_text("Ask bund about this workspace…");
        self.editor
            .move_cursor(ratatui_textarea::CursorMove::Bottom);
        self.editor.move_cursor(ratatui_textarea::CursorMove::End);
    }
    fn notice(&mut self, text: String) {
        if self.notices.len() == 16 {
            self.notices.pop_front();
        }
        self.notices.push_back(text);
        self.document_dirty = true;
    }
    fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let (header, approval, conversation, composer) = regions(area, self.pending.is_some());
        let model = if self.snapshot.profile.model.is_empty() {
            "model not configured"
        } else {
            &self.snapshot.profile.model
        };
        let view = if self.follow {
            ""
        } else {
            " · Browsing history"
        };
        let workspace = self
            .snapshot
            .workspace
            .file_name()
            .unwrap_or(self.snapshot.workspace.as_os_str())
            .to_string_lossy();
        let mode = if header.width >= 120 {
            policy_status(&self.snapshot.policy)
        } else {
            format!("{:?}", self.snapshot.policy.mode)
        };
        let headings = vec![
            Line::styled(
                clean(&format!(
                    "Bund · {workspace} · session {}",
                    self.snapshot.id
                )),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Line::from(clean(&format!(
                "{} / {} · {} · {:?} · {} tokens{}",
                self.snapshot.profile.provider,
                model,
                mode,
                self.snapshot.status,
                self.snapshot.usage.input_tokens + self.snapshot.usage.output_tokens,
                view
            ))),
            Line::styled(
                format!(
                    "Ctrl+O activity {} · PgUp/PgDn scroll · Ctrl+End latest{}",
                    if self.expanded { "open" } else { "hidden" },
                    view
                ),
                Style::default().fg(Color::DarkGray),
            ),
            Line::styled(clean(&self.hint), Style::default().fg(Color::Cyan)),
        ];
        let headings = if header.height == 1 {
            vec![Line::from(clean(&format!(
                "Bund · {:?}{} · {}",
                self.snapshot.status, view, self.hint
            )))]
        } else {
            headings
        };
        frame.render_widget(Paragraph::new(headings), header);
        self.viewport_height = conversation.height as usize;
        if let Some(request) = &self.pending {
            let prompt = if approval.height < 3 {
                format!(
                    "Ctrl+Y approve · Ctrl+N deny\nApproval: {}",
                    clean(&request.tool_call.name)
                )
            } else {
                format!("Approval required: {}\nCtrl+Y: approve once  Ctrl+N / Esc: deny · PgUp/PgDn review\nYour draft is preserved.", clean(&request.tool_call.name))
            };
            frame.render_widget(
                Paragraph::new(prompt).style(Style::default().fg(Color::Yellow)),
                approval,
            );
            let preview = render::plain(
                &format!(
                    "Workspace: {}\n{}",
                    request.workspace.display(),
                    request.preview
                ),
                conversation.width as usize,
                Style::default(),
            );
            let max = preview.len().saturating_sub(conversation.height as usize);
            self.approval_scroll = self.approval_scroll.min(max);
            frame.render_widget(
                Paragraph::new(
                    preview
                        .into_iter()
                        .skip(self.approval_scroll)
                        .take(conversation.height as usize)
                        .collect::<Vec<_>>(),
                ),
                conversation,
            );
        } else {
            if self.document_dirty || self.document_width != conversation.width {
                self.document = self.build_document(conversation.width as usize);
                self.document_width = conversation.width;
                self.document_dirty = false;
            }
            let max = self
                .document
                .rows
                .len()
                .saturating_sub(conversation.height as usize);
            self.top = if self.follow {
                max
            } else {
                self.anchor
                    .as_ref()
                    .map(|a| self.document.position(a))
                    .unwrap_or(self.top)
                    .min(max)
            };
            self.anchor = self
                .document
                .rows
                .get(self.top)
                .map(|row| row.anchor.clone());
            frame.render_widget(
                Paragraph::new(
                    self.document
                        .rows
                        .iter()
                        .skip(self.top)
                        .take(conversation.height as usize)
                        .map(|row| row.line.clone())
                        .collect::<Vec<_>>(),
                ),
                conversation,
            );
        }
        let borders = if composer.width < 3 || composer.height < 2 {
            Borders::NONE
        } else if composer.height < 3 {
            Borders::LEFT | Borders::RIGHT
        } else {
            Borders::ALL
        };
        let mut block = Block::default()
            .borders(borders)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Cyan));
        if composer.height >= 3 && composer.width >= 3 {
            block = block.title(if self.pending.is_some() {
                " Ask bund · draft paused "
            } else {
                " Ask bund "
            });
        }
        let input = block.inner(composer);
        frame.render_widget(block, composer);
        frame.render_widget(&self.editor, input);
        // The textarea retains its old scroll origin after a tiny window grows.
        // Once its new viewport is known, reveal a short draft in full. Scrolling
        // cannot move the cursor here because every line fits the new viewport.
        if self.editor.lines().len() <= input.height as usize
            && self.editor.lines().iter().all(|line| {
                unicode_width::UnicodeWidthStr::width(line.as_str()) <= input.width as usize
            })
        {
            let (row, col) = self.editor.scroll_offset();
            if row != 0 || col != 0 {
                self.editor.scroll((
                    -(row.min(i16::MAX as u16) as i16),
                    -(col.min(i16::MAX as u16) as i16),
                ));
                frame.render_widget(&self.editor, input);
            }
        }
    }
    fn move_page(&mut self, down: bool) {
        let step = self.viewport_height.saturating_sub(1).max(1);
        if self.pending.is_some() {
            self.approval_scroll = if down {
                self.approval_scroll.saturating_add(step)
            } else {
                self.approval_scroll.saturating_sub(step)
            };
            return;
        }
        self.follow = false;
        let max = self
            .document
            .rows
            .len()
            .saturating_sub(self.viewport_height);
        self.top = if down {
            self.top.saturating_add(step).min(max)
        } else {
            self.top.saturating_sub(step)
        };
        self.anchor = self
            .document
            .rows
            .get(self.top)
            .map(|row| row.anchor.clone());
    }
}

fn regions(area: Rect, pending: bool) -> (Rect, Rect, Rect, Rect) {
    let input = if area.height >= 10 {
        5
    } else if area.height >= 4 {
        3
    } else {
        1
    }
    .min(area.height);
    let remaining = area.height.saturating_sub(input);
    let approvals = if pending {
        if remaining >= 6 {
            3
        } else if remaining >= 3 {
            2
        } else {
            1
        }
        .min(remaining)
    } else {
        0
    };
    let preferred = if area.height >= 15 {
        4
    } else if area.height >= 10 {
        3
    } else {
        1
    };
    let header = preferred.min(remaining.saturating_sub(approvals + 1));
    let middle = remaining.saturating_sub(approvals + header);
    (
        Rect::new(area.x, area.y, area.width, header),
        Rect::new(area.x, area.y + header, area.width, approvals),
        Rect::new(area.x, area.y + header + approvals, area.width, middle),
        Rect::new(
            area.x,
            area.bottom().saturating_sub(input),
            area.width,
            input,
        ),
    )
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
    load_latest(&handle, &mut screen);
    let _guard = TerminalGuard;
    let mut terminal = ratatui::try_init()?;
    execute!(std::io::stdout(), EnableBracketedPaste)?;
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
        tokio::select! {
            _ = &mut shutdown => { handle.cancel().await?; break; },
            _ = ticker.tick() => {
                // One owner handles terminal input and drawing. Bound each batch
                // so paste/output bursts cannot starve cancellation or approvals.
                for _ in 0..32 {
                    if !crossterm::event::poll(Duration::ZERO)? { break; }
                    match crossterm::event::read()? {
                        Event::Key(key) if key.kind != crossterm::event::KeyEventKind::Release => {
                            match key_action(&handle, &mut screen, key).await {
                                Ok(true) => break 'ui,
                                Ok(false) => {},
                                Err(error) => { screen.hint = error.to_string(); screen.notice(error.to_string()); },
                            }
                            dirty = true;
                        },
                        Event::Paste(text) if screen.pending.is_none() => { screen.editor.insert_str(clean(&text)); dirty = true; },
                        Event::Resize(_, _) => dirty = true,
                        _ => {},
                    }
                }
                for _ in 0..64 {
                    let Some(event) = pending_events.pop_front() else { break; };
                    process_event(&handle, &mut screen, event, &mut cursor, &mut pending_events).await?;
                    dirty = true;
                }
                match poll_history(&handle, &mut screen, cursor).await {
                    Ok(changed) => dirty |= changed,
                    Err(error) => {
                        screen.history_follow_pending = false;
                        screen.hint = format!("Could not load history: {error}");
                        screen.notice(screen.hint.clone());
                        dirty = true;
                    },
                }
                if dirty { terminal.draw(|frame| screen.render(frame))?; dirty = false; }
            },
            received = events.recv(), if pending_events.is_empty() => match received {
                Ok(event) => { process_event(&handle, &mut screen, event, &mut cursor, &mut pending_events).await?; dirty = true; },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    pending_events.extend(handle.events_after(cursor, 256).await?);
                    screen.hint = "Recovered delayed session events".into();
                },
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
        }
    }
    terminal.show_cursor()?;
    Ok(())
}

async fn replay(handle: &SessionHandle, ceiling: u64) -> Result<Transcript> {
    let mut transcript = history_window();
    let mut cursor = 0;
    while cursor < ceiling {
        let page = handle.events_after(cursor, 256).await?;
        let before = cursor;
        for event in page
            .into_iter()
            .take_while(|event| event.sequence <= ceiling)
        {
            cursor = event.sequence;
            transcript.apply(&event);
        }
        anyhow::ensure!(
            cursor > before,
            "session event history has a gap after {cursor}"
        );
        tokio::task::yield_now().await;
    }
    Ok(transcript)
}

fn history_window() -> Transcript {
    Transcript::with_limits(bund_core::TranscriptLimits {
        max_runs: 32,
        max_total_bytes: 512 * 1024,
        ..Default::default()
    })
}

fn start_history_load(
    screen: &mut Screen,
    work: impl std::future::Future<Output = Result<HistoryLoad>> + Send + 'static,
) {
    if let Some(task) = screen.history_task.take() {
        task.abort();
    }
    screen
        .history_ceiling
        .get_or_insert(screen.transcript.cursor());
    screen.history_follow_pending = false;
    screen.history_task = Some(tokio::spawn(work));
    screen.hint = "Loading history · input and cancellation remain available".into();
}

fn load_latest(handle: &SessionHandle, screen: &mut Screen) {
    let handle = handle.clone();
    start_history_load(screen, async move {
        let ceiling = handle.snapshot().await?.cursor;
        Ok(HistoryLoad {
            transcript: replay(&handle, ceiling).await?,
            ceiling,
            target: HistoryTarget::Latest,
        })
    });
}

/// Apply a completed background window, then catch up a latest-view load in
/// bounded batches. Live events still update approvals/status during the load.
async fn poll_history(
    handle: &SessionHandle,
    screen: &mut Screen,
    live_cursor: u64,
) -> Result<bool> {
    let mut changed = false;
    if screen
        .history_task
        .as_ref()
        .is_some_and(|task| task.is_finished())
    {
        let loaded = screen
            .history_task
            .take()
            .unwrap()
            .await
            .context("history loader stopped")??;
        if loaded.transcript.runs().is_empty() && !matches!(loaded.target, HistoryTarget::Latest) {
            if matches!(loaded.target, HistoryTarget::Top) {
                screen.history_ceiling = Some(loaded.ceiling);
            }
            screen.hint = "No more conversation history in that direction".into();
            return Ok(true);
        }
        screen.transcript = loaded.transcript;
        screen.history_ceiling = Some(loaded.ceiling);
        screen.history_follow_pending = matches!(loaded.target, HistoryTarget::Latest);
        if !screen.history_follow_pending {
            screen.hint = "Browsing history · PgUp/PgDn scroll · Ctrl+End latest".into();
        }
        screen.follow = screen.history_follow_pending;
        screen.anchor = None;
        screen.top = if matches!(loaded.target, HistoryTarget::Bottom) {
            usize::MAX
        } else {
            0
        };
        screen.document_dirty = true;
        changed = true;
    }
    if screen.history_follow_pending {
        let cursor = screen.transcript.cursor();
        if cursor < live_cursor {
            let page = handle.events_after(cursor, 64).await?;
            for event in page
                .into_iter()
                .take_while(|event| event.sequence <= live_cursor)
            {
                screen.transcript.apply(&event);
            }
            anyhow::ensure!(
                screen.transcript.cursor() > cursor,
                "session event history has a gap after {cursor}"
            );
            screen.history_ceiling = Some(screen.transcript.cursor());
            screen.document_dirty = true;
            changed = true;
        }
        if screen.transcript.cursor() >= live_cursor {
            screen.history_follow_pending = false;
            screen.history_ceiling = None;
            screen.follow = true;
            screen.hint = "Latest conversation · Enter send · /help".into();
            changed = true;
        }
    }
    Ok(changed)
}

/// Load complete adjacent runs. If adding a run would evict the beginning of
/// this page, return the checkpoint before that run so PgDn cannot skip it.
async fn forward_history(handle: &SessionHandle, after: u64, latest: u64) -> Result<HistoryLoad> {
    let mut next = history_window();
    let mut cursor = after;
    let mut first_id = None;
    let mut checkpoint = None;
    while cursor < latest {
        let page = handle.events_after(cursor, 256).await?;
        let before = cursor;
        for event in page
            .into_iter()
            .take_while(|event| event.sequence <= latest)
        {
            if matches!(event.kind, EventKind::RunStarted { .. }) && !next.runs().is_empty() {
                checkpoint = Some((next.clone(), cursor));
            }
            next.apply(&event);
            let retained = next.runs().front().map(|run| run.id.clone());
            if first_id.is_some() && retained != first_id {
                if let Some((transcript, ceiling)) = checkpoint {
                    return Ok(HistoryLoad {
                        transcript,
                        ceiling,
                        target: HistoryTarget::Top,
                    });
                }
            }
            if first_id.is_none() {
                first_id = retained;
            }
            cursor = event.sequence;
        }
        anyhow::ensure!(
            cursor > before,
            "session event history has a gap after {cursor}"
        );
        tokio::task::yield_now().await;
    }
    Ok(HistoryLoad {
        transcript: next,
        ceiling: cursor,
        target: HistoryTarget::Top,
    })
}

async fn page_history(handle: &SessionHandle, screen: &mut Screen, down: bool) -> Result<()> {
    if screen.pending.is_some() {
        screen.move_page(down);
        return Ok(());
    }
    if screen.history_task.is_some() {
        screen.hint = "History is loading · Ctrl+End returns to latest".into();
        return Ok(());
    }
    // Freeze on the first scroll, before new events can evict the viewed run.
    screen
        .history_ceiling
        .get_or_insert(screen.transcript.cursor());
    screen.history_follow_pending = false;
    screen.follow = false;
    if !down && screen.top == 0 {
        if let Some(first) = screen
            .transcript
            .runs()
            .front()
            .filter(|run| run.start_sequence > 1)
        {
            let ceiling = first.start_sequence - 1;
            let handle = handle.clone();
            start_history_load(screen, async move {
                Ok(HistoryLoad {
                    transcript: replay(&handle, ceiling).await?,
                    ceiling,
                    target: HistoryTarget::Bottom,
                })
            });
            return Ok(());
        }
    }
    if down
        && screen.top
            >= screen
                .document
                .rows
                .len()
                .saturating_sub(screen.viewport_height)
    {
        if let Some(ceiling) = screen.history_ceiling {
            // A frozen active run may have acquired more responses; overlap its
            // start instead of rebuilding its tail without the prompt/status.
            let after = screen
                .transcript
                .runs()
                .back()
                .filter(|run| run.status.is_active())
                .map(|run| run.start_sequence.saturating_sub(1))
                .unwrap_or(ceiling);
            let handle = handle.clone();
            start_history_load(screen, async move {
                let latest = handle.snapshot().await?.cursor;
                if ceiling >= latest {
                    return Ok(HistoryLoad {
                        transcript: replay(&handle, latest).await?,
                        ceiling: latest,
                        target: HistoryTarget::Latest,
                    });
                }
                forward_history(&handle, after, latest).await
            });
            return Ok(());
        }
    }
    screen.move_page(down);
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
    event: SessionEvent,
    cursor: &mut u64,
    pending: &mut VecDeque<SessionEvent>,
) -> Result<()> {
    if event.sequence <= *cursor {
        return Ok(());
    }
    if event.sequence > *cursor + 1 {
        // Recover only one bounded page. The tick loop processes a small batch
        // before returning to input, even when a slow display lagged far behind.
        let missing = handle
            .events_after(*cursor, 256)
            .await?
            .into_iter()
            .take_while(|old| old.sequence < event.sequence)
            .collect::<Vec<_>>();
        anyhow::ensure!(
            missing
                .first()
                .is_some_and(|old| old.sequence == *cursor + 1),
            "session event history has a gap after {cursor}"
        );
        pending.push_front(event);
        for old in missing.into_iter().rev() {
            pending.push_front(old);
        }
        return Ok(());
    }
    *cursor = event.sequence;
    apply_event(handle, screen, event).await
}

async fn apply_event(
    handle: &SessionHandle,
    screen: &mut Screen,
    event: SessionEvent,
) -> Result<()> {
    if screen.history_ceiling.is_none() {
        screen.transcript.apply(&event);
        screen.document_dirty = true;
    }
    match event.kind {
        EventKind::RunStarted { .. } => {
            screen.snapshot.status = RunStatus::Running;
            screen.hint = "Working · Esc cancels · draft stays editable".into();
        }
        EventKind::ApprovalRequired(request) => {
            screen.pending = Some(request);
            screen.approval_scroll = 0;
            screen.snapshot.status = RunStatus::AwaitingApproval;
            screen.hint = "Approval required · Ctrl+Y approve once · Ctrl+N deny".into();
        }
        EventKind::ApprovalResolved { .. } => {
            screen.pending = None;
            screen.snapshot.status = RunStatus::Running;
        }
        EventKind::ToolStarted { call } => {
            screen.hint = format!("Running {} · Esc cancels", clean(&call.name))
        }
        EventKind::ToolFinished(result) if result.is_error => {
            screen.hint = format!("{} failed · see conversation", clean(&result.name))
        }
        EventKind::ContextCompacted { .. } => screen.hint = "Context compacted".into(),
        EventKind::SettingsChanged => screen.snapshot = handle.snapshot().await?,
        EventKind::RunFinished {
            status, validation, ..
        } => {
            screen.pending = None;
            screen.snapshot = handle.snapshot().await?;
            screen.hint = format!("{status:?} · checks {validation:?} · Enter send · /help");
        }
        _ => {}
    }
    Ok(())
}

async fn key_action(handle: &SessionHandle, screen: &mut Screen, key: KeyEvent) -> Result<bool> {
    match (key.code, key.modifiers) {
        (KeyCode::Char('o'), mods) if mods.contains(KeyModifiers::CONTROL) => {
            screen.expanded = !screen.expanded;
            screen.document_dirty = true;
            return Ok(false);
        }
        (KeyCode::End, mods) if mods.contains(KeyModifiers::CONTROL) => {
            if screen.pending.is_some() {
                screen.approval_scroll = usize::MAX;
            } else if screen.history_ceiling.is_some() || screen.history_task.is_some() {
                load_latest(handle, screen);
            } else {
                screen.follow = true;
            }
            return Ok(false);
        }
        (KeyCode::PageUp, _) | (KeyCode::PageDown, _) => {
            page_history(handle, screen, key.code == KeyCode::PageDown).await?;
            return Ok(false);
        }
        _ => {}
    }
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
                screen.notice(HELP.into());
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
    if screen.history_ceiling.is_some() || screen.history_task.is_some() {
        load_latest(handle, screen);
    }
    screen.follow = true;
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
#[path = "ui_history_tests.rs"]
mod history_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn test_screen() -> Screen {
        let snapshot = SessionSnapshot {
            policy: Policy::default(),
            id: "fixture".into(),
            workspace: PathBuf::from("."),
            profile: bund_core::ModelProfile::new("openai", "fixture-model"),
            status: RunStatus::Idle,
            mode: Mode::Act,
            cursor: 0,
            run_id: None,
            pending_approval: None,
            messages: vec![],
            usage: bund_core::Usage::default(),
            validation: bund_core::ValidationStatus::NotRun,
        };
        Screen::new(
            snapshot,
            History {
                path: PathBuf::new(),
                entries: vec![],
                position: None,
                draft: String::new(),
            },
        )
    }

    fn buffer_row(backend: &TestBackend, row: u16) -> String {
        (0..backend.buffer().area.width)
            .map(|col| backend.buffer()[(col, row)].symbol())
            .collect()
    }

    #[test]
    fn fullscreen_geometry_preserves_top_information_bottom_draft_and_approval() {
        let mut screen = test_screen();
        screen.set_draft("Keep this draft\nsecond line");
        for (width, height) in [(80, 24), (50, 16), (100, 30), (24, 8), (8, 2), (1, 1)] {
            let mut terminal = ratatui::Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| screen.render(frame)).unwrap();
            let (_, _, _, composer) = regions(Rect::new(0, 0, width, height), false);
            assert_eq!(composer.bottom(), height);
            assert_eq!(screen.draft(), "Keep this draft\nsecond line");
            if height >= 16 {
                assert!(buffer_row(terminal.backend(), 0).contains("Bund"));
                assert!(buffer_row(terminal.backend(), 1).contains("fixture-model"));
                assert_eq!(terminal.backend().buffer()[(0, height - 5)].symbol(), "╭");
                assert_eq!(
                    terminal.backend().buffer()[(width - 1, height - 1)].symbol(),
                    "╯"
                );
                assert!(buffer_row(terminal.backend(), height - 4).contains("Keep this draft"));
            }
        }
        screen.pending = Some(ApprovalRequest {
            id: "approval".into(),
            run_id: "run".into(),
            tool_call: bund_core::ToolCall {
                id: "call".into(),
                name: "apply_patch".into(),
                arguments: serde_json::json!({}),
            },
            preview: "exact proposed patch".into(),
            fingerprint: "fixture".into(),
            workspace: PathBuf::from("."),
        });
        let mut terminal = ratatui::Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| screen.render(frame)).unwrap();
        assert!(buffer_row(terminal.backend(), 4).contains("Approval required"));
        assert!(buffer_row(terminal.backend(), 5).contains("Ctrl+Y"));
        assert!(buffer_row(terminal.backend(), 8).contains("exact proposed patch"));
        assert!(buffer_row(terminal.backend(), 19).contains("draft paused"));
        assert!(buffer_row(terminal.backend(), 20).contains("Keep this draft"));
        assert_eq!(terminal.backend().buffer()[(0, 19)].fg, Color::Cyan);
    }

    #[test]
    fn resizing_short_draft_preserves_cursor_and_selection() {
        use ratatui_textarea::CursorMove;
        let mut screen = test_screen();
        screen.set_draft("first\nsecond");
        screen.editor.move_cursor(CursorMove::Top);
        screen.editor.move_cursor(CursorMove::Head);
        screen.editor.start_selection();
        screen.editor.move_cursor(CursorMove::Bottom);
        screen.editor.move_cursor(CursorMove::End);
        let cursor = screen.editor.cursor();
        let selection = screen.editor.selection_range();
        let mut terminal = ratatui::Terminal::new(TestBackend::new(50, 8)).unwrap();
        terminal.draw(|frame| screen.render(frame)).unwrap();
        terminal.backend_mut().resize(50, 24);
        terminal.draw(|frame| screen.render(frame)).unwrap();
        assert!(buffer_row(terminal.backend(), 20).contains("first"));
        assert!(buffer_row(terminal.backend(), 21).contains("second"));
        assert_eq!(screen.editor.cursor(), cursor);
        assert_eq!(screen.editor.selection_range(), selection);
    }

    fn event(screen: &mut Screen, kind: EventKind) {
        screen.transcript.apply(&SessionEvent {
            version: 1,
            session_id: "fixture".into(),
            sequence: screen.transcript.cursor() + 1,
            run_id: Some("run".into()),
            kind,
        });
        screen.document_dirty = true;
    }

    #[test]
    fn collapsed_activity_hides_details_and_final_replaces_provisional_text() {
        use bund_core::ResponsePurpose;
        let mut screen = test_screen();
        event(
            &mut screen,
            EventKind::RunStarted {
                prompt: "inspect".into(),
            },
        );
        event(
            &mut screen,
            EventKind::ResponseStarted {
                id: "response".into(),
                purpose: ResponsePurpose::Conversation,
            },
        );
        event(
            &mut screen,
            EventKind::ResponseTextDelta {
                id: "response".into(),
                text: "partial response".into(),
            },
        );
        event(
            &mut screen,
            EventKind::ResponseCompleted {
                id: "response".into(),
                purpose: ResponsePurpose::Conversation,
                text: "Authoritative answer".into(),
                reasoning: vec!["private provider summary".into()],
                tool_calls: vec![],
            },
        );
        event(
            &mut screen,
            EventKind::RunOutput {
                response_id: Some("response".into()),
                text: "Authoritative answer".into(),
            },
        );
        let text = |screen: &Screen| {
            screen
                .build_document(80)
                .rows
                .iter()
                .map(|row| row.line.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert!(!text(&screen).contains("private provider summary"));
        assert!(!text(&screen).contains("partial response"));
        assert_eq!(text(&screen).matches("Authoritative answer").count(), 1);
        screen.expanded = true;
        assert!(text(&screen).contains("Reasoning summary"));
        assert!(text(&screen).contains("private provider summary"));
        assert_eq!(text(&screen).matches("Authoritative answer").count(), 1);
    }

    #[test]
    fn scroll_anchor_survives_streaming_resize_and_expansion() {
        let mut screen = test_screen();
        event(
            &mut screen,
            EventKind::RunStarted {
                prompt: "inspect".into(),
            },
        );
        event(
            &mut screen,
            EventKind::TextDelta {
                text: (0..80).map(|n| format!("line {n}\n")).collect(),
            },
        );
        screen.set_draft("persistent draft");
        let mut terminal = ratatui::Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| screen.render(frame)).unwrap();
        screen.move_page(false);
        terminal.draw(|frame| screen.render(frame)).unwrap();
        let anchor = screen.anchor.clone().unwrap();
        event(
            &mut screen,
            EventKind::TextDelta {
                text: "latest line\n".into(),
            },
        );
        screen.expanded = true;
        terminal.draw(|frame| screen.render(frame)).unwrap();
        assert_eq!(screen.anchor, Some(anchor.clone()));
        terminal.backend_mut().resize(50, 18);
        terminal.draw(|frame| screen.render(frame)).unwrap();
        assert_eq!(screen.anchor.as_ref().unwrap().section, anchor.section);
        assert_eq!(screen.draft(), "persistent draft");
        screen.follow = true;
        terminal.draw(|frame| screen.render(frame)).unwrap();
        assert_eq!(
            screen.top,
            screen
                .document
                .rows
                .len()
                .saturating_sub(screen.viewport_height)
        );
    }

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
    fn history_restores_draft_and_keeps_multiline_prompts() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = History::load(dir.path().join("history")).unwrap();
        h.record("first\nsecond").unwrap();
        assert_eq!(h.previous("draft"), Some("first\nsecond".into()));
        assert_eq!(h.next(), Some("draft".into()));
        assert_eq!(History::load(h.path).unwrap().entries, ["first\nsecond"]);
    }
}
