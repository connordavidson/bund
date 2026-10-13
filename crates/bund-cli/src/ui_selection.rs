//! Selection of a bounded snapshot of the visible conversation.
use crate::output::clean;
use ratatui::{
    layout::Rect,
    style::Color,
    text::Line,
    widgets::{Clear, Paragraph},
    Frame,
};
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub(super) struct SelectionRow {
    line: Line<'static>,
    copy_columns: Range<usize>,
}

impl SelectionRow {
    pub fn new(line: Line<'static>) -> Self {
        let text = line.to_string();
        let safe = clean(&text).replace(['\n', '\r', '\t'], " ");
        Self {
            line: if safe == text { line } else { Line::raw(safe) },
            copy_columns: 0..usize::MAX,
        }
    }
    pub fn copy_columns(mut self, columns: Range<usize>) -> Self {
        self.copy_columns = columns;
        self
    }
}

pub(super) struct SelectionView {
    area: Rect,
    rows: Vec<SelectionRow>,
    start: Option<(usize, usize)>,
    end: (usize, usize),
    dragging: bool,
    moved: bool,
}

impl SelectionView {
    pub fn new(area: Rect, mut rows: Vec<SelectionRow>) -> Self {
        rows.truncate(area.height as usize);
        Self {
            area,
            rows,
            start: None,
            end: (0, 0),
            dragging: false,
            moved: false,
        }
    }
    pub fn area(&self) -> Rect {
        self.area
    }
    pub fn is_active(&self) -> bool {
        self.start.is_some() && (self.dragging || self.moved)
    }
    pub fn clear(&mut self) {
        self.start = None;
        self.dragging = false;
        self.moved = false;
    }
    pub fn start(&mut self, x: u16, y: u16) -> bool {
        self.clear();
        if !self.area.contains((x, y).into()) || self.rows.is_empty() {
            return false;
        }
        let row = (y - self.area.y) as usize;
        if row >= self.rows.len() {
            return false;
        }
        self.start = Some((row, (x - self.area.x) as usize));
        self.end = self.start.unwrap();
        self.dragging = true;
        true
    }
    pub fn drag(&mut self, x: u16, y: u16) -> bool {
        if !self.dragging || self.area.is_empty() || self.rows.is_empty() {
            return false;
        }
        let row = y
            .saturating_sub(self.area.y)
            .min(self.rows.len() as u16 - 1) as usize;
        let column = x.saturating_sub(self.area.x).min(self.area.width - 1) as usize;
        self.end = (row, column);
        self.moved |= self.start != Some(self.end);
        true
    }
    fn columns(&self, row: usize) -> Option<Range<usize>> {
        if !self.moved {
            return None;
        }
        let start = self.start?;
        let (first, last) = if start <= self.end {
            (start, self.end)
        } else {
            (self.end, start)
        };
        if row < first.0 || row > last.0 {
            return None;
        }
        let low = if row == first.0 { first.1 } else { 0 };
        let high = if row == last.0 {
            last.1 + 1
        } else {
            self.area.width as usize
        };
        let allowed = &self.rows[row].copy_columns;
        Some(low.max(allowed.start)..high.min(allowed.end).min(self.area.width as usize))
    }
    pub fn finish(&mut self) -> Option<String> {
        if !self.dragging {
            return None;
        }
        self.dragging = false;
        if !self.moved {
            self.clear();
            return None;
        }
        let mut selected = Vec::new();
        for (index, row) in self.rows.iter().enumerate() {
            let Some(columns) = self.columns(index) else {
                continue;
            };
            let mut column = 0;
            let mut text = String::new();
            for grapheme in row.line.to_string().graphemes(true) {
                let width = UnicodeWidthStr::width(grapheme);
                if column < columns.end && column + width > columns.start {
                    text.push_str(grapheme);
                }
                column += width;
            }
            selected.push(text.trim_end_matches(' ').to_owned());
        }
        let text = selected.join("\n");
        if text.trim().is_empty() {
            self.clear();
            return None;
        }
        Some(text)
    }
    pub fn paint(&self, frame: &mut Frame) {
        if !self.is_active() || frame.area().intersection(self.area) != self.area {
            return;
        }
        frame.render_widget(Clear, self.area);
        frame.render_widget(
            Paragraph::new(
                self.rows
                    .iter()
                    .map(|row| row.line.clone())
                    .collect::<Vec<_>>(),
            ),
            self.area,
        );
        for (index, row) in self.rows.iter().enumerate() {
            let Some(columns) = self.columns(index) else {
                continue;
            };
            let mut column = 0;
            for grapheme in row.line.to_string().graphemes(true) {
                let width = UnicodeWidthStr::width(grapheme);
                if column < columns.end && column + width > columns.start {
                    for cell in column..(column + width).min(self.area.width as usize) {
                        frame.buffer_mut()[(self.area.x + cell as u16, self.area.y + index as u16)]
                            .set_bg(Color::Rgb(35, 85, 145))
                            .set_fg(Color::White);
                    }
                }
                column += width;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reverse_drag_copies_whole_unicode_graphemes_without_speaker_gutters() {
        let mut view = SelectionView::new(
            Rect::new(0, 4, 30, 2),
            vec![
                SelectionRow::new(Line::raw("● 世界 e\u{301}")).copy_columns(2..30),
                SelectionRow::new(Line::raw("  next 🦀")).copy_columns(2..30),
            ],
        );
        assert!(!view.start(0, 1));
        assert!(view.start(9, 5));
        view.drag(3, 4); // The second cell of 世 still selects the complete glyph.
        assert_eq!(view.finish().as_deref(), Some("世界 e\u{301}\nnext 🦀"));
        assert!(view.is_active());
        assert!(view.start(2, 4));
        assert_eq!(view.finish(), None, "a click must not change the clipboard");
        assert!(!view.is_active());
    }
    #[test]
    fn outside_drags_clamp_to_visible_rows_and_control_sequences_are_inert() {
        let mut view = SelectionView::new(
            Rect::new(0, 4, 30, 2),
            vec![
                SelectionRow::new(Line::raw("safe\x1b]52;c;hidden\x07 text")),
                SelectionRow::new(Line::raw("last")),
            ],
        );
        assert!(view.start(0, 4));
        view.drag(500, 500);
        assert_eq!(view.finish().as_deref(), Some("safe text\nlast"));
        view.clear();
        assert!(!view.is_active());
    }
}
