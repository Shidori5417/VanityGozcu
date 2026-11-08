//! Modal dialogs: add target, confirm delete.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::config::{ConfigError, normalize_code};
use crate::engine::{EngineCommand, TargetView};

/// Longest input accepted; codes are at most 32 characters.
const MAX_INPUT: usize = 40;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dialog {
    Add {
        input: String,
        error: Option<String>,
    },
    ConfirmDelete {
        code: String,
    },
}

/// What a key press did to the open dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Dialog stays open.
    Keep,
    /// Dialog closed without an action.
    Close,
    /// Dialog closed and this command should be sent.
    Submit(EngineCommand),
}

impl Dialog {
    pub fn add() -> Self {
        Dialog::Add {
            input: String::new(),
            error: None,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent, targets: &[TargetView]) -> Outcome {
        match self {
            Dialog::Add { input, error } => match key.code {
                KeyCode::Esc => Outcome::Close,
                KeyCode::Enter => match validate(input, targets) {
                    Ok(code) => Outcome::Submit(EngineCommand::AddTarget { code }),
                    Err(msg) => {
                        *error = Some(msg);
                        Outcome::Keep
                    }
                },
                KeyCode::Backspace => {
                    input.pop();
                    *error = None;
                    Outcome::Keep
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    if input.chars().count() < MAX_INPUT {
                        input.push(c);
                    }
                    *error = None;
                    Outcome::Keep
                }
                _ => Outcome::Keep,
            },
            Dialog::ConfirmDelete { code } => match key.code {
                KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
                    Outcome::Submit(EngineCommand::RemoveTarget { code: code.clone() })
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => Outcome::Close,
                _ => Outcome::Keep,
            },
        }
    }
}

/// Normalizes `input` and checks it is not already watched; the error text is shown
/// inline in the dialog.
fn validate(input: &str, targets: &[TargetView]) -> Result<String, String> {
    let code = normalize_code(input.trim()).map_err(|e| match e {
        ConfigError::InvalidCode { reason, .. } => reason.to_owned(),
        other => other.to_string(),
    })?;
    if targets.iter().any(|t| t.code == code) {
        return Err(format!("{code} is already watched"));
    }
    Ok(code)
}

fn popup(area: Rect, width: u16, height: u16) -> Rect {
    let [v] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(area);
    let [h] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(v);
    h
}

pub fn render(frame: &mut Frame, area: Rect, dialog: &Dialog) {
    match dialog {
        Dialog::Add { input, error } => {
            let rect = popup(area, 48, 6);
            frame.render_widget(Clear, rect);
            let mut lines = vec![
                Line::from("Vanity code (a-z, 0-9, '-', 2-32 chars):"),
                Line::from(vec![
                    Span::raw("> "),
                    Span::raw(input.clone()),
                    Span::styled("_", Style::new().fg(Color::DarkGray)),
                ]),
            ];
            lines.push(match error {
                Some(e) => Line::styled(e.clone(), Style::new().fg(Color::Red)),
                None => Line::styled("Enter: add   Esc: cancel", Style::new().fg(Color::DarkGray)),
            });
            let block = Block::default().borders(Borders::ALL).title(" Add target ");
            frame.render_widget(Paragraph::new(lines).block(block), rect);
        }
        Dialog::ConfirmDelete { code } => {
            let rect = popup(area, 48, 5);
            frame.render_widget(Clear, rect);
            let lines = vec![
                Line::from(format!(
                    "Stop watching '{code}' and remove it from the config?"
                )),
                Line::styled(
                    "y: delete   n/Esc: cancel",
                    Style::new().fg(Color::DarkGray),
                ),
            ];
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::new().fg(Color::Red))
                .title(" Delete target ");
            frame.render_widget(
                Paragraph::new(lines)
                    .wrap(ratatui::widgets::Wrap { trim: true })
                    .block(block),
                rect,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::VanityState;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn view(code: &str) -> TargetView {
        TargetView {
            code: code.into(),
            state: VanityState::Unknown,
            last_result: None,
            last_detail: None,
            last_checked: None,
            next_check: None,
            auto_claim: None,
        }
    }

    fn type_text(d: &mut Dialog, text: &str, targets: &[TargetView]) {
        for c in text.chars() {
            assert_eq!(d.handle_key(key(KeyCode::Char(c)), targets), Outcome::Keep);
        }
    }

    #[test]
    fn add_submits_normalized_code() {
        let mut d = Dialog::add();
        type_text(&mut d, "Hello-1", &[]);
        assert_eq!(
            d.handle_key(key(KeyCode::Enter), &[]),
            Outcome::Submit(EngineCommand::AddTarget {
                code: "hello-1".into()
            })
        );
    }

    #[test]
    fn invalid_and_duplicate_show_inline_error_and_stay_open() {
        let targets = [view("taken")];
        let mut d = Dialog::add();
        type_text(&mut d, "a", &targets);
        assert_eq!(d.handle_key(key(KeyCode::Enter), &targets), Outcome::Keep);
        assert!(matches!(&d, Dialog::Add { error: Some(e), .. } if e.contains("length")));

        let mut d = Dialog::add();
        type_text(&mut d, "bad code", &targets);
        d.handle_key(key(KeyCode::Enter), &targets);
        assert!(matches!(&d, Dialog::Add { error: Some(e), .. } if e.contains("only")));

        let mut d = Dialog::add();
        type_text(&mut d, "TAKEN", &targets);
        d.handle_key(key(KeyCode::Enter), &targets);
        assert!(matches!(&d, Dialog::Add { error: Some(e), .. } if e.contains("already")));

        // Typing again clears the error; Esc cancels.
        d.handle_key(key(KeyCode::Backspace), &targets);
        assert!(matches!(&d, Dialog::Add { error: None, .. }));
        assert_eq!(d.handle_key(key(KeyCode::Esc), &targets), Outcome::Close);
    }

    #[test]
    fn confirm_delete_needs_explicit_yes() {
        let mut d = Dialog::ConfirmDelete { code: "abc".into() };
        assert_eq!(d.handle_key(key(KeyCode::Char('x')), &[]), Outcome::Keep);
        assert_eq!(d.handle_key(key(KeyCode::Char('n')), &[]), Outcome::Close);
        assert_eq!(
            d.handle_key(key(KeyCode::Char('y')), &[]),
            Outcome::Submit(EngineCommand::RemoveTarget { code: "abc".into() })
        );
    }
}
