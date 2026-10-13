//! Terminal-only styling. Conversation data and event reduction live in bund-core.
use crate::output::clean;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub const ORANGE: Color = Color::Rgb(255, 184, 108);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Anchor {
    pub run: String,
    pub section: String,
    /// Offset in visible source text, independent of soft wrapping and borders.
    pub offset: usize,
}

pub struct Row {
    pub anchor: Anchor,
    pub line: Line<'static>,
}

#[derive(Default)]
pub struct Document {
    pub rows: Vec<Row>,
}

impl Document {
    /// Add a two-column speaker gutter after computing anchors from the body.
    /// Callers wrap the body at the available width minus these two columns.
    pub fn message_section(
        &mut self,
        run: &str,
        section: &str,
        lines: Vec<Line<'static>>,
        color: Color,
    ) {
        let start = self.rows.len();
        self.section(run, section, lines);
        let mut marked = false;
        for row in &mut self.rows[start..] {
            let has_text = row
                .line
                .spans
                .iter()
                .any(|span| !span.content.trim().is_empty());
            let prefix = if !marked && has_text {
                marked = true;
                Span::styled("● ", Style::default().fg(color))
            } else {
                Span::raw("  ")
            };
            row.line.spans.insert(0, prefix);
        }
    }

    pub fn section(&mut self, run: &str, section: &str, lines: Vec<Line<'static>>) {
        let mut offset = 0;
        self.rows.extend(lines.into_iter().map(|line| {
            let length = line
                .spans
                .iter()
                .filter(|span| span.style.fg != Some(ORANGE))
                .map(|span| span.content.as_ref())
                .collect::<String>()
                .trim_end()
                .len()
                .max(1);
            let row = Row {
                anchor: Anchor {
                    run: run.into(),
                    section: section.into(),
                    offset,
                },
                line,
            };
            offset += length;
            row
        }));
    }

    pub fn position(&self, anchor: &Anchor) -> usize {
        // Map source offsets back to visual rows after soft wrapping changes.
        // Collapsed detail sections fall back to their parent answer.
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.anchor.run == anchor.run && row.anchor.section == anchor.section)
            .take_while(|(_, row)| row.anchor.offset <= anchor.offset)
            .last()
            .map(|(i, _)| i)
            .or_else(|| {
                self.rows
                    .iter()
                    .position(|row| row.anchor.run == anchor.run && row.anchor.section == "answer")
            })
            .or_else(|| {
                self.rows
                    .iter()
                    .position(|row| row.anchor.run == anchor.run)
            })
            .unwrap_or(0)
    }
}

pub fn bordered_body(body: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    if width < 4 {
        return body;
    }
    let mut lines = bordered("", body, width);
    lines.pop();
    lines.remove(0);
    lines
}

pub fn plain(text: &str, width: usize, style: Style) -> Vec<Line<'static>> {
    clean(text)
        .split('\n')
        .flat_map(|line| wrap(vec![Span::styled(line.to_owned(), style)], width))
        .collect()
}

pub fn markdown(text: &str, width: usize) -> Vec<Line<'static>> {
    let mut code = false;
    let mut lines = Vec::new();
    for line in clean(text).split('\n') {
        if let Some(language) = line.trim_start().strip_prefix("```") {
            code = !code;
            lines.extend(plain(
                if code { language } else { "" },
                width,
                Style::default().fg(Color::DarkGray),
            ));
        } else if code {
            lines.extend(plain(
                &format!("  {line}"),
                width,
                Style::default().fg(Color::LightCyan),
            ));
        } else if let Some(heading) = heading(line) {
            lines.extend(plain(
                heading,
                width,
                Style::default().add_modifier(Modifier::BOLD),
            ));
        } else {
            let line =
                if let Some(item) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
                    format!("• {item}")
                } else {
                    line.to_owned()
                };
            lines.extend(wrap(inline(&line), width));
        }
    }
    lines
}

fn heading(line: &str) -> Option<&str> {
    let line = line.trim_start();
    let hashes = line.bytes().take_while(|byte| *byte == b'#').count();
    if (1..=6).contains(&hashes) {
        line[hashes..].strip_prefix(' ').map(str::trim_start)
    } else {
        None
    }
}

fn inline(text: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let next = [
            rest.find('`').map(|i| (i, "`")),
            rest.find("**").map(|i| (i, "**")),
        ]
        .into_iter()
        .flatten()
        .min_by_key(|(i, _)| *i);
        let Some((start, delimiter)) = next else {
            spans.push(Span::raw(rest.to_owned()));
            break;
        };
        let after = &rest[start + delimiter.len()..];
        let Some(end) = after.find(delimiter) else {
            spans.push(Span::raw(rest.to_owned()));
            break;
        };
        spans.push(Span::raw(rest[..start].to_owned()));
        spans.push(Span::styled(
            after[..end].to_owned(),
            if delimiter == "`" {
                Style::default().fg(Color::LightCyan)
            } else {
                Style::default().add_modifier(Modifier::BOLD)
            },
        ));
        rest = &after[end + delimiter.len()..];
    }
    spans
}

pub fn wrap(spans: Vec<Span<'static>>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = Vec::new();
    let mut used = 0;
    for span in spans {
        let mut part = String::new();
        for grapheme in span.content.graphemes(true) {
            let size = UnicodeWidthStr::width(grapheme);
            if used + size > width && used > 0 {
                current.push(Span::styled(std::mem::take(&mut part), span.style));
                lines.push(Line::from(std::mem::take(&mut current)));
                used = 0;
            }
            if size > width {
                part.push('�');
                used += 1;
            } else {
                part.push_str(grapheme);
                used += size;
            }
        }
        current.push(Span::styled(part, span.style));
    }
    lines.push(Line::from(current));
    lines
}

pub fn bordered(title: &str, body: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    if width < 4 {
        return body;
    }
    let border = Style::default().fg(ORANGE);
    let title = plain(&format!("─ {title} "), width - 2, border).remove(0);
    let title_width = title.width();
    let mut head = vec![Span::styled("╭", border)];
    head.extend(title.spans);
    head.push(Span::styled(
        format!("{}╮", "─".repeat((width - 2).saturating_sub(title_width))),
        border,
    ));
    let mut lines = vec![Line::from(head)];
    for line in body {
        let padding = (width - 4).saturating_sub(line.width());
        let mut spans = vec![Span::styled("│ ", border)];
        spans.extend(line.spans);
        spans.push(Span::raw(" ".repeat(padding)));
        spans.push(Span::styled(" │", border));
        lines.push(Line::from(spans));
    }
    lines.push(Line::styled(format!("╰{}╯", "─".repeat(width - 2)), border));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_gutters_preserve_markdown_and_unicode_at_narrow_widths() {
        for width in [4, 8, 20] {
            let mut document = Document::default();
            document.message_section(
                "run",
                "answer",
                markdown("\n**bold** `世界e\u{301}`\nnext line", width - 2),
                Color::Gray,
            );
            let rows = &document.rows;
            assert_eq!(rows[0].line.to_string().trim(), "");
            assert_eq!(rows[1].line.spans[0].content, "● ");
            assert_eq!(rows[1].line.spans[0].style.fg, Some(Color::Gray));
            for (i, row) in rows.iter().enumerate() {
                assert!(row.line.width() <= width);
                if i != 1 {
                    assert_eq!(row.line.spans[0].content, "  ");
                }
            }
            let spans = rows
                .iter()
                .flat_map(|row| row.line.spans.iter().skip(1))
                .collect::<Vec<_>>();
            let text = spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            assert!(text.contains("世界e\u{301}"));
            assert!(spans
                .iter()
                .any(|span| span.style.add_modifier.contains(Modifier::BOLD)));
            assert!(spans
                .iter()
                .any(|span| span.style.fg == Some(Color::LightCyan)));
        }
    }

    #[test]
    fn message_gutters_do_not_change_source_anchors_on_resize() {
        let text = "abcdefghij".repeat(200);
        let mut wide = Document::default();
        wide.message_section(
            "run",
            "prompt",
            plain(&text, 80, Style::default()),
            Color::Blue,
        );
        assert_eq!(wide.rows[0].line.spans[0].style.fg, Some(Color::Blue));
        let anchor = wide.rows[10].anchor.clone();
        assert_eq!(anchor.offset, 800);
        let mut narrow = Document::default();
        narrow.message_section(
            "run",
            "prompt",
            plain(&text, 40, Style::default()),
            Color::Blue,
        );
        assert_eq!(narrow.rows[narrow.position(&anchor)].anchor, anchor);
        assert_eq!(narrow.rows[0].line.width(), 42);
        assert_eq!(wide.rows[0].line.width(), 82);
    }

    #[test]
    fn unicode_and_styles_fit_narrow_rows() {
        for width in [1, 4, 20] {
            for line in markdown(
                "# Result\n- **bold** and `世界e\u{301}`\n```rust\nlet x = 1;\n```",
                width,
            ) {
                assert!(line.width() <= width);
            }
        }
        assert!(markdown("**bold**", 20)[0]
            .spans
            .iter()
            .any(|span| span.style.add_modifier.contains(Modifier::BOLD)));
        assert!(!markdown("\x1b]52;c;SECRET\x07safe", 30)[0]
            .to_string()
            .contains("SECRET"));
    }
    #[test]
    fn activity_border_does_not_color_body_and_fits() {
        let rows = bordered("Activity", markdown("tool output", 16), 20);
        assert!(rows.iter().all(|line| line.width() == 20));
        assert_eq!(rows[1].spans[0].style.fg, Some(ORANGE));
        assert_eq!(rows[1].spans[1].style.fg, None);
    }

    #[test]
    fn anchors_track_source_offsets_across_wrap_and_previous_detail_growth() {
        let text = "abcdefghij".repeat(200);
        let mut wide = Document::default();
        wide.section("run", "tool-2", plain(&text, 80, Style::default()));
        let anchor = wide.rows[10].anchor.clone();
        assert_eq!(anchor.offset, 800);
        let mut narrow = Document::default();
        narrow.section(
            "run",
            "response-1",
            plain(&"Earlier reasoning ".repeat(200), 40, Style::default()),
        );
        narrow.section("run", "tool-2", plain(&text, 40, Style::default()));
        assert_eq!(narrow.rows[narrow.position(&anchor)].anchor.offset, 800);
        let mut collapsed = Document::default();
        collapsed.section("run", "prompt", plain("Question", 40, Style::default()));
        collapsed.section("run", "answer", plain("Answer", 40, Style::default()));
        assert_eq!(
            collapsed.rows[collapsed.position(&anchor)].anchor.section,
            "answer"
        );
    }
}
