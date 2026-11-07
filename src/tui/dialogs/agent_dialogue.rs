//! Read-only viewer and controls for a bounded two-session dialogue.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;
use unicode_width::UnicodeWidthChar;

use crate::tui::agent_dialogue::{AgentDialogue, DialoguePhase};
use crate::tui::styles::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentDialogueAction {
    Continue,
    Close,
    TogglePause,
    Stop,
    Extend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineKind {
    Heading,
    Dimmed,
    FirstAgent,
    SecondAgent,
    Body,
}

#[derive(Debug, Clone)]
struct CachedLine {
    kind: LineKind,
    text: String,
}

pub struct AgentDialogueDialog {
    scroll: usize,
    visible_rows: usize,
    wrap_width: usize,
    cached_revision: u64,
    wrapped: Vec<CachedLine>,
    follow_tail: bool,
}

impl Default for AgentDialogueDialog {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentDialogueDialog {
    pub fn new() -> Self {
        Self {
            scroll: 0,
            visible_rows: 1,
            wrap_width: 0,
            cached_revision: 0,
            wrapped: Vec::new(),
            follow_tail: true,
        }
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
        self.follow_tail = self.scroll == self.max_scroll();
    }

    pub fn scroll_wheel(&mut self, down: bool) {
        self.scroll_by(if down { 3 } else { -3 });
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> AgentDialogueAction {
        match key.code {
            KeyCode::Esc => AgentDialogueAction::Close,
            KeyCode::Char('p' | 'P') => AgentDialogueAction::TogglePause,
            KeyCode::Char('x' | 'X') => AgentDialogueAction::Stop,
            KeyCode::Char('+') => AgentDialogueAction::Extend,
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll_by(-1);
                AgentDialogueAction::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll_by(1);
                AgentDialogueAction::Continue
            }
            KeyCode::PageUp | KeyCode::Char('u') => {
                self.scroll_by(-(self.visible_rows.max(2) as isize / 2));
                AgentDialogueAction::Continue
            }
            KeyCode::PageDown | KeyCode::Char('d') => {
                self.scroll_by(self.visible_rows.max(2) as isize / 2);
                AgentDialogueAction::Continue
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.scroll = 0;
                self.follow_tail = false;
                AgentDialogueAction::Continue
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.scroll = self.max_scroll();
                self.follow_tail = true;
                AgentDialogueAction::Continue
            }
            _ => AgentDialogueAction::Continue,
        }
    }

    pub fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
        dialogue: &AgentDialogue,
    ) {
        let width = area.width.saturating_sub(4).clamp(30, 108);
        let height = area.height.saturating_sub(2).clamp(11, 40);
        let dialog_area = super::centered_rect(area, width, height);
        frame.render_widget(Clear, dialog_area);

        let first = &dialogue.participants()[0];
        let second = &dialogue.participants()[1];
        let title = format!(
            " Dialogue · {} ↔ {} · {}/{} turns ",
            first.title,
            second.title,
            dialogue.turns().len(),
            dialogue.max_turns()
        );
        let pause_label = if dialogue.paused() { "resume" } else { "pause" };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .title(title)
            .title_style(Style::default().fg(theme.title).bold())
            .title_bottom(
                Line::from(vec![
                    Span::styled(" j/k", Style::default().fg(theme.accent)),
                    Span::styled(" scroll  ", Style::default().fg(theme.dimmed)),
                    Span::styled("p", Style::default().fg(theme.accent)),
                    Span::styled(
                        format!(" {pause_label}  "),
                        Style::default().fg(theme.dimmed),
                    ),
                    Span::styled("+", Style::default().fg(theme.accent)),
                    Span::styled(" extend  ", Style::default().fg(theme.dimmed)),
                    Span::styled("x", Style::default().fg(theme.accent)),
                    Span::styled(" stop  ", Style::default().fg(theme.dimmed)),
                    Span::styled("Esc", Style::default().fg(theme.accent)),
                    Span::styled(" close ", Style::default().fg(theme.dimmed)),
                ])
                .right_aligned(),
            );
        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let chunks = Layout::vertical([Constraint::Length(1), Constraint::Min(1)])
            .margin(1)
            .split(inner);
        let status_style = match dialogue.phase() {
            DialoguePhase::Finished(_) => Style::default().fg(theme.dimmed),
            _ if dialogue.paused() => Style::default().fg(theme.waiting),
            _ => Style::default().fg(theme.accent),
        };
        frame.render_widget(
            Paragraph::new(format!("Status: {}", dialogue.status_label())).style(status_style),
            chunks[0],
        );

        self.visible_rows = chunks[1].height.max(1) as usize;
        let wrap_width = chunks[1].width.max(1) as usize;
        if self.cached_revision != dialogue.revision() || self.wrap_width != wrap_width {
            let followed = self.follow_tail;
            self.rebuild(dialogue, wrap_width);
            if followed {
                self.scroll = self.max_scroll();
            } else {
                self.scroll = self.scroll.min(self.max_scroll());
            }
        }

        let visible = self
            .wrapped
            .iter()
            .skip(self.scroll)
            .take(self.visible_rows)
            .map(|line| {
                let style = match line.kind {
                    LineKind::Heading => Style::default().fg(theme.title).bold(),
                    LineKind::Dimmed => Style::default().fg(theme.dimmed),
                    LineKind::FirstAgent => Style::default().fg(theme.accent).bold(),
                    LineKind::SecondAgent => Style::default().fg(theme.running).bold(),
                    LineKind::Body => Style::default().fg(theme.text),
                };
                Line::from(line.text.clone()).style(style)
            })
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(visible), chunks[1]);
    }

    fn rebuild(&mut self, dialogue: &AgentDialogue, width: usize) {
        self.wrapped.clear();
        self.push_wrapped(LineKind::Heading, "Objective", width);
        self.push_wrapped(LineKind::Body, dialogue.goal(), width);
        self.wrapped.push(CachedLine {
            kind: LineKind::Body,
            text: String::new(),
        });

        if dialogue.turns().is_empty() {
            self.push_wrapped(
                LineKind::Dimmed,
                "The first prompt is being prepared. Replies appear here after native transcript capture.",
                width,
            );
        } else {
            for (index, turn) in dialogue.turns().iter().enumerate() {
                let participant = dialogue.participant(&turn.speaker_id);
                let label = participant
                    .map(|agent| format!("{}. {} · {}", index + 1, agent.title, agent.tool))
                    .unwrap_or_else(|| format!("{}. unknown session", index + 1));
                let kind = if turn.speaker_id == dialogue.participants()[0].id {
                    LineKind::FirstAgent
                } else {
                    LineKind::SecondAgent
                };
                self.push_wrapped(kind, &label, width);
                self.push_wrapped(LineKind::Body, &turn.content, width);
                self.wrapped.push(CachedLine {
                    kind: LineKind::Body,
                    text: String::new(),
                });
            }
        }
        self.wrap_width = width;
        self.cached_revision = dialogue.revision();
    }

    fn push_wrapped(&mut self, kind: LineKind, text: &str, width: usize) {
        for row in wrap_text(text, width) {
            self.wrapped.push(CachedLine { kind, text: row });
        }
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
    use crate::tui::agent_dialogue::DialogueParticipant;
    use crossterm::event::KeyModifiers;

    #[test]
    fn vim_controls_map_to_supervisor_actions() {
        let mut dialog = AgentDialogueDialog::new();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        assert_eq!(
            dialog.handle_key(key(KeyCode::Char('p'))),
            AgentDialogueAction::TogglePause
        );
        assert_eq!(
            dialog.handle_key(key(KeyCode::Char('x'))),
            AgentDialogueAction::Stop
        );
        assert_eq!(
            dialog.handle_key(key(KeyCode::Char('+'))),
            AgentDialogueAction::Extend
        );
        assert_eq!(
            dialog.handle_key(key(KeyCode::Esc)),
            AgentDialogueAction::Close
        );
    }

    #[test]
    fn cache_rebuilds_only_for_width_or_state_revision() {
        let participant = |id: &str| DialogueParticipant {
            id: id.to_string(),
            title: id.to_string(),
            tool: "codex".to_string(),
        };
        let dialogue = AgentDialogue::new(
            1,
            participant("one"),
            participant("two"),
            "goal".to_string(),
        );
        let mut dialog = AgentDialogueDialog::new();
        dialog.rebuild(&dialogue, 40);
        let cached = dialog.wrapped.len();
        dialog.rebuild(&dialogue, 40);
        assert_eq!(dialog.wrapped.len(), cached);
    }
}
