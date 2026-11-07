//! Read-only session-memory viewer and Vim-first share target picker.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;
use unicode_width::UnicodeWidthChar;

use crate::session::memory::SessionMemory;
use crate::tui::styles::Theme;

pub enum MemoryDialogResult {
    Continue,
    Cancel,
    Share,
    Converse,
}

pub struct SessionMemoryDialog {
    request_id: u64,
    session_id: String,
    title: String,
    memory: Option<SessionMemory>,
    error: Option<String>,
    scroll: usize,
    visible_rows: usize,
    wrap_width: usize,
    wrapped: Vec<String>,
}

impl SessionMemoryDialog {
    pub fn loading(request_id: u64, session_id: String, title: String) -> Self {
        Self {
            request_id,
            session_id,
            title,
            memory: None,
            error: None,
            scroll: 0,
            visible_rows: 1,
            wrap_width: 0,
            wrapped: Vec::new(),
        }
    }

    pub fn request_id(&self) -> u64 {
        self.request_id
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn memory(&self) -> Option<&SessionMemory> {
        self.memory.as_ref()
    }

    pub fn finish(&mut self, result: Result<SessionMemory, String>) {
        match result {
            Ok(memory) => {
                self.memory = Some(memory);
                self.error = None;
            }
            Err(error) => {
                self.memory = None;
                self.error = Some(error);
            }
        }
        self.scroll = 0;
        self.wrap_width = 0;
        self.wrapped.clear();
    }

    fn max_scroll(&self) -> usize {
        self.wrapped.len().saturating_sub(self.visible_rows)
    }

    fn scroll_by(&mut self, delta: isize) {
        self.scroll = if delta < 0 {
            self.scroll.saturating_sub(delta.unsigned_abs())
        } else {
            self.scroll.saturating_add(delta as usize)
        }
        .min(self.max_scroll());
    }

    pub fn scroll_wheel(&mut self, down: bool) {
        self.scroll_by(if down { 3 } else { -3 });
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> MemoryDialogResult {
        match key.code {
            KeyCode::Esc => MemoryDialogResult::Cancel,
            KeyCode::Char('s' | 'S') if self.memory.is_some() => MemoryDialogResult::Share,
            KeyCode::Char('c' | 'C')
                if self
                    .memory
                    .as_ref()
                    .is_some_and(|memory| memory.source.supports_dialogue()) =>
            {
                MemoryDialogResult::Converse
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll_by(-1);
                MemoryDialogResult::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll_by(1);
                MemoryDialogResult::Continue
            }
            KeyCode::PageUp | KeyCode::Char('u') => {
                self.scroll_by(-(self.visible_rows.max(2) as isize / 2));
                MemoryDialogResult::Continue
            }
            KeyCode::PageDown | KeyCode::Char('d') => {
                self.scroll_by(self.visible_rows.max(2) as isize / 2);
                MemoryDialogResult::Continue
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.scroll = 0;
                MemoryDialogResult::Continue
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.scroll = self.max_scroll();
                MemoryDialogResult::Continue
            }
            KeyCode::Enter if self.error.is_some() => MemoryDialogResult::Cancel,
            _ => MemoryDialogResult::Continue,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let width = area.width.saturating_sub(4).clamp(24, 104);
        let height = area.height.saturating_sub(2).clamp(9, 38);
        let dialog_area = super::centered_rect(area, width, height);
        frame.render_widget(Clear, dialog_area);

        let footer = if let Some(memory) = &self.memory {
            let mut spans = vec![
                Span::styled(" j/k", Style::default().fg(theme.accent)),
                Span::styled(" scroll  ", Style::default().fg(theme.dimmed)),
                Span::styled("s", Style::default().fg(theme.accent)),
                Span::styled(" share  ", Style::default().fg(theme.dimmed)),
            ];
            if memory.source.supports_dialogue() {
                spans.extend([
                    Span::styled("c", Style::default().fg(theme.accent)),
                    Span::styled(" converse  ", Style::default().fg(theme.dimmed)),
                ]);
            }
            spans.extend([
                Span::styled("Esc", Style::default().fg(theme.accent)),
                Span::styled(" close ", Style::default().fg(theme.dimmed)),
            ]);
            Line::from(spans)
        } else {
            Line::from(vec![
                Span::styled(" Esc", Style::default().fg(theme.accent)),
                Span::styled(" close ", Style::default().fg(theme.dimmed)),
            ])
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .title(format!(" Context · {} ", self.title))
            .title_style(Style::default().fg(theme.title).bold())
            .title_bottom(footer.right_aligned());
        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let chunks = Layout::vertical([Constraint::Length(1), Constraint::Min(1)])
            .margin(1)
            .split(inner);
        let source = match (&self.memory, &self.error) {
            (Some(memory), _) => format!("{} · bounded read-only snapshot", memory.source.label()),
            (None, Some(_)) => "memory unavailable".to_string(),
            _ => "loading on demand…".to_string(),
        };
        frame.render_widget(
            Paragraph::new(source).style(Style::default().fg(theme.dimmed)),
            chunks[0],
        );

        let body = if let Some(memory) = &self.memory {
            memory.content.as_str()
        } else if let Some(error) = &self.error {
            error.as_str()
        } else {
            "Reading the selected session without blocking the TUI…"
        };
        let wrap_width = chunks[1].width.max(1) as usize;
        if self.wrap_width != wrap_width {
            self.wrap_width = wrap_width;
            self.wrapped = wrap_text(body, wrap_width);
        }
        self.visible_rows = chunks[1].height.max(1) as usize;
        self.scroll = self.scroll.min(self.max_scroll());
        let visible: Vec<Line> = self
            .wrapped
            .iter()
            .skip(self.scroll)
            .take(self.visible_rows)
            .map(|line| Line::from(line.clone()))
            .collect();
        frame.render_widget(
            Paragraph::new(visible).style(Style::default().fg(theme.text)),
            chunks[1],
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryTarget {
    pub id: String,
    pub title: String,
    pub tool: String,
}

pub enum MemoryTargetResult {
    Continue,
    Cancel,
    Selected(String),
}

pub struct MemoryTargetDialog {
    items: Vec<MemoryTarget>,
    selected: usize,
    visible_rows: usize,
    title: &'static str,
    submit_label: &'static str,
}

impl MemoryTargetDialog {
    pub fn new(items: Vec<MemoryTarget>) -> Self {
        Self::with_labels(items, " Share context with… ", " send  ")
    }

    pub fn dialogue(items: Vec<MemoryTarget>) -> Self {
        Self::with_labels(items, " Converse with… ", " choose  ")
    }

    fn with_labels(
        items: Vec<MemoryTarget>,
        title: &'static str,
        submit_label: &'static str,
    ) -> Self {
        Self {
            items,
            selected: 0,
            visible_rows: 1,
            title,
            submit_label,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> MemoryTargetResult {
        match key.code {
            KeyCode::Esc => MemoryTargetResult::Cancel,
            KeyCode::Enter => self
                .items
                .get(self.selected)
                .map(|item| MemoryTargetResult::Selected(item.id.clone()))
                .unwrap_or(MemoryTargetResult::Cancel),
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                MemoryTargetResult::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.items.is_empty() {
                    self.selected = (self.selected + 1).min(self.items.len() - 1);
                }
                MemoryTargetResult::Continue
            }
            KeyCode::PageUp | KeyCode::Char('u') => {
                self.selected = self.selected.saturating_sub(self.visible_rows.max(1));
                MemoryTargetResult::Continue
            }
            KeyCode::PageDown | KeyCode::Char('d') => {
                if !self.items.is_empty() {
                    self.selected = (self.selected + self.visible_rows).min(self.items.len() - 1);
                }
                MemoryTargetResult::Continue
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.selected = 0;
                MemoryTargetResult::Continue
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.selected = self.items.len().saturating_sub(1);
                MemoryTargetResult::Continue
            }
            _ => MemoryTargetResult::Continue,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let visible = self.items.len().clamp(1, 10);
        self.visible_rows = visible;
        let width = area.width.saturating_sub(4).clamp(28, 68);
        let height = (visible as u16 + 4).min(area.height.max(1));
        let dialog_area = super::centered_rect(area, width, height);
        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .title(self.title)
            .title_style(Style::default().fg(theme.title).bold())
            .title_bottom(
                Line::from(vec![
                    Span::styled(" j/k", Style::default().fg(theme.accent)),
                    Span::styled(" move  ", Style::default().fg(theme.dimmed)),
                    Span::styled("Enter", Style::default().fg(theme.accent)),
                    Span::styled(self.submit_label, Style::default().fg(theme.dimmed)),
                    Span::styled("Esc", Style::default().fg(theme.accent)),
                    Span::styled(" back ", Style::default().fg(theme.dimmed)),
                ])
                .right_aligned(),
            );
        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        if self.items.is_empty() {
            frame.render_widget(
                Paragraph::new("No other session is available.")
                    .style(Style::default().fg(theme.dimmed)),
                inner,
            );
            return;
        }

        let offset = if self.selected >= visible {
            self.selected - visible + 1
        } else {
            0
        };
        let lines: Vec<Line> = self
            .items
            .iter()
            .enumerate()
            .skip(offset)
            .take(visible)
            .map(|(index, item)| {
                let marker = if index == self.selected { "› " } else { "  " };
                let style = if index == self.selected {
                    Style::default()
                        .fg(theme.background)
                        .bg(theme.accent)
                        .bold()
                } else {
                    Style::default().fg(theme.text)
                };
                Line::from(format!("{marker}{} · {}", item.title, item.tool)).style(style)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            rows.push(String::new());
            continue;
        }
        let mut row = String::new();
        let mut row_width = 0usize;
        for ch in line.chars() {
            let char_width = ch.width().unwrap_or(0);
            if row_width + char_width > width && !row.is_empty() {
                rows.push(std::mem::take(&mut row));
                row_width = 0;
            }
            row.push(ch);
            row_width += char_width;
        }
        rows.push(row);
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::memory::MemorySource;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn target_picker_supports_complete_vim_navigation() {
        let mut dialog = MemoryTargetDialog::new(vec![
            MemoryTarget {
                id: "a".into(),
                title: "A".into(),
                tool: "claude".into(),
            },
            MemoryTarget {
                id: "b".into(),
                title: "B".into(),
                tool: "codex".into(),
            },
        ]);
        assert!(matches!(
            dialog.handle_key(key(KeyCode::Char('j'))),
            MemoryTargetResult::Continue
        ));
        assert!(matches!(
            dialog.handle_key(key(KeyCode::Enter)),
            MemoryTargetResult::Selected(id) if id == "b"
        ));
        assert!(matches!(
            dialog.handle_key(key(KeyCode::Esc)),
            MemoryTargetResult::Cancel
        ));
    }

    #[test]
    fn converse_requires_a_role_bounded_transcript() {
        let memory = |source| SessionMemory {
            session_id: "a".into(),
            title: "A".into(),
            tool: "claude".into(),
            source,
            content: "reply".into(),
            partial: true,
        };
        let mut native = SessionMemoryDialog::loading(1, "a".into(), "A".into());
        native.finish(Ok(memory(MemorySource::ClaudeTranscript)));
        assert!(matches!(
            native.handle_key(key(KeyCode::Char('c'))),
            MemoryDialogResult::Converse
        ));

        let mut pane = SessionMemoryDialog::loading(2, "a".into(), "A".into());
        pane.finish(Ok(memory(MemorySource::PaneHistory)));
        assert!(matches!(
            pane.handle_key(key(KeyCode::Char('c'))),
            MemoryDialogResult::Continue
        ));
    }
}
