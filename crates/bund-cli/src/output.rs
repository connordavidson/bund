use crate::args::OutputFormat;
use anyhow::{Context, Result};
use bund_core::{EventKind, RunStatus, SessionEvent, SessionHandle};
use std::{
    collections::VecDeque,
    io::{self, Write},
};

#[derive(Default)]
pub struct Sanitizer {
    state: u8,
}
impl Sanitizer {
    /// Stateful across chunks so split OSC/CSI sequences cannot reach the terminal.
    pub fn push(&mut self, text: &str) -> String {
        let mut output = String::new();
        for c in text.chars() {
            match self.state {
                1 => {
                    self.state = match c {
                        '[' => 2,
                        ']' | 'P' | '^' | '_' => 3,
                        _ => 0,
                    }
                }
                2 => {
                    if ('@'..='~').contains(&c) {
                        self.state = 0;
                    }
                }
                3 => {
                    if c == '\u{7}' {
                        self.state = 0;
                    } else if c == '\u{1b}' {
                        self.state = 4;
                    }
                }
                4 => self.state = if c == '\\' { 0 } else { 3 },
                _ => match c {
                    '\u{1b}' => self.state = 1,
                    '\n' => output.push('\n'),
                    '\t' => output.push_str("    "),
                    '\r' => {}
                    c if c.is_control()
                        || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') => {}
                    c => output.push(c),
                },
            }
        }
        output
    }
}
pub fn clean(text: &str) -> String {
    Sanitizer::default().push(text)
}

pub fn exit_code(status: RunStatus) -> i32 {
    match status {
        RunStatus::Completed => 0,
        RunStatus::Cancelled => 130,
        _ => 1,
    }
}

pub async fn headless(handle: SessionHandle, prompt: String, format: OutputFormat) -> Result<i32> {
    let attachment = handle.attach().await?;
    let mut events = attachment.events;
    let mut cursor = attachment.snapshot.cursor;
    let run_id = handle.submit(prompt).await?;
    let mut pending: VecDeque<SessionEvent> = VecDeque::new();
    let mut forced_code = None;
    let mut sanitizer = Sanitizer::default();
    let mut streamed = false;
    loop {
        let event = if let Some(event) = pending.pop_front() {
            event
        } else {
            tokio::select! {
                signal = tokio::signal::ctrl_c() => {
                    signal?;
                    handle.cancel().await?;
                    forced_code = Some(130);
                    continue;
                },
                received = events.recv() => match received {
                    Ok(event) => event,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        pending.extend(handle.events_after(cursor, 512).await?);
                        continue;
                    },
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => anyhow::bail!("session event stream closed before completion"),
                }
            }
        };
        if event.sequence <= cursor {
            continue;
        }
        if event.sequence > cursor + 1 {
            let before = handle
                .events_after(cursor, 512)
                .await?
                .into_iter()
                .filter(|old| old.sequence < event.sequence)
                .collect::<Vec<_>>();
            anyhow::ensure!(
                !before.is_empty(),
                "session event history has a gap after {cursor}"
            );
            pending.push_front(event);
            for old in before.into_iter().rev() {
                pending.push_front(old);
            }
            continue;
        }
        cursor = event.sequence;
        if event.run_id.as_deref() != Some(&run_id) {
            continue;
        }
        if format == OutputFormat::Jsonl {
            serde_json::to_writer(io::stdout().lock(), &event)?;
            println!();
            io::stdout().flush()?;
        }
        match event.kind {
            EventKind::TextDelta { text } if format == OutputFormat::Text => {
                print!("{}", sanitizer.push(&text));
                io::stdout().flush()?;
                streamed = true;
            }
            EventKind::AssistantMessage { text } if format == OutputFormat::Text => {
                if !streamed {
                    print!("{}", clean(&text));
                }
                println!();
                io::stdout().flush()?;
                streamed = false;
                sanitizer = Sanitizer::default();
            }
            EventKind::ApprovalRequired(request) => {
                if format == OutputFormat::Text {
                    eprintln!(
                        "approval_required: {}\n{}",
                        clean(&request.tool_call.name),
                        clean(&request.preview)
                    );
                }
                forced_code = Some(3);
                handle
                    .cancel()
                    .await
                    .context("cancel pending headless approval")?;
            }
            EventKind::RunFinished {
                status, message, ..
            } => {
                if format == OutputFormat::Text
                    && (status != RunStatus::Completed || !message.is_empty())
                {
                    if streamed {
                        println!();
                    }
                    if status != RunStatus::Completed {
                        eprintln!("{}", clean(&message));
                    }
                }
                return Ok(forced_code.unwrap_or_else(|| exit_code(status)));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strips_split_osc_and_csi_without_corrupting_unicode() {
        let mut s = Sanitizer::default();
        assert_eq!(s.push("hello\u{1b}]52;c;secret"), "hello");
        assert_eq!(s.push("more\u{1b}"), "");
        assert_eq!(s.push("\\世界\u{1b}[3"), "世界");
        assert_eq!(s.push("1m!\u{1b}[0m"), "!");
    }
    #[test]
    fn strips_controls_and_bidi_but_keeps_lines() {
        assert_eq!(clean("a\r\u{8}\u{7}\u{202e}b\n\tc"), "ab\n    c");
    }
    #[test]
    fn terminal_status_exit_contract() {
        assert_eq!(exit_code(RunStatus::Completed), 0);
        assert_eq!(exit_code(RunStatus::Failed), 1);
        assert_eq!(exit_code(RunStatus::Cancelled), 130);
    }
}
