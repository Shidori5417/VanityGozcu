//! Main target table.

use std::time::{Duration, SystemTime};

use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};

use crate::engine::{ResultKind, TargetView};
use crate::model::VanityState;

fn compact(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// "5s ago" for a past instant, "-" when never.
pub fn fmt_ago(now: SystemTime, at: Option<SystemTime>) -> String {
    match at {
        None => "-".to_owned(),
        Some(at) => format!(
            "{} ago",
            compact(now.duration_since(at).unwrap_or_default())
        ),
    }
}

/// "in 12s" for a future instant, "due" once passed, "pending" when unscheduled.
pub fn fmt_until(now: SystemTime, at: Option<SystemTime>) -> String {
    match at {
        None => "pending".to_owned(),
        Some(at) => match at.duration_since(now) {
            Ok(d) if !d.is_zero() => format!("in {}", compact(d)),
            _ => "due".to_owned(),
        },
    }
}

pub fn status_text_style(state: &VanityState) -> (String, Style) {
    match state {
        VanityState::Unknown => ("unknown".to_owned(), Style::new().fg(Color::DarkGray)),
        VanityState::Taken { .. } => ("taken".to_owned(), Style::new().fg(Color::Yellow)),
        VanityState::FreeSuspected { count, .. } => (
            format!("free? ({count})"),
            Style::new().fg(Color::LightGreen),
        ),
        VanityState::Free => (
            "FREE".to_owned(),
            Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
        ),
        VanityState::Claimed => ("claimed".to_owned(), Style::new().fg(Color::Cyan)),
        VanityState::Lost => ("lost".to_owned(), Style::new().fg(Color::Red)),
    }
}

fn last_result_text_style(kind: Option<ResultKind>) -> (&'static str, Style) {
    match kind {
        None => ("-", Style::new().fg(Color::DarkGray)),
        Some(ResultKind::Taken) => ("taken", Style::new().fg(Color::Gray)),
        Some(ResultKind::Free) => ("free", Style::new().fg(Color::Green)),
        Some(ResultKind::RateLimited) => ("rate limited", Style::new().fg(Color::Yellow)),
        Some(ResultKind::Error) => ("error", Style::new().fg(Color::Red)),
        Some(ResultKind::Unknown) => ("unexpected", Style::new().fg(Color::Red)),
    }
}

fn auto_claim_text(flag: Option<bool>) -> &'static str {
    match flag {
        Some(true) | None => "yes",
        Some(false) => "no",
    }
}

pub fn render(
    frame: &mut Frame,
    area: Rect,
    targets: &[TargetView],
    selected: usize,
    now: SystemTime,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" VanityGözcü - {} targets ", targets.len()));
    if targets.is_empty() {
        let hint = Paragraph::new("No targets yet. Press 'a' to add one.")
            .style(Style::new().fg(Color::DarkGray))
            .block(block);
        frame.render_widget(hint, area);
        return;
    }
    let header = Row::new(
        [
            "Code",
            "Status",
            "Last result",
            "Last check",
            "Next check",
            "Owner",
            "Auto-claim",
        ]
        .map(|h| Cell::from(h).style(Style::new().add_modifier(Modifier::BOLD))),
    );
    let rows = targets.iter().map(|t| {
        let (status, status_style) = status_text_style(&t.state);
        let (result, result_style) = last_result_text_style(t.last_result);
        Row::new([
            Cell::from(t.code.as_str()),
            Cell::from(Span::styled(status, status_style)),
            Cell::from(Span::styled(result, result_style)),
            Cell::from(fmt_ago(now, t.last_checked)),
            Cell::from(fmt_until(now, t.next_check)),
            Cell::from(t.owner().unwrap_or("-")),
            Cell::from(auto_claim_text(t.auto_claim)),
        ])
    });
    let widths = [
        Constraint::Length(20),
        Constraint::Length(12),
        Constraint::Length(13),
        Constraint::Length(12),
        Constraint::Length(12),
        Constraint::Min(10),
        Constraint::Length(10),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    let mut state = TableState::default().with_selected(Some(selected));
    frame.render_stateful_widget(table, area, &mut state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn relative_times() {
        let now = at(10_000);
        assert_eq!(fmt_ago(now, None), "-");
        assert_eq!(fmt_ago(now, Some(at(9995))), "5s ago");
        assert_eq!(fmt_ago(now, Some(at(10_000 - 125))), "2m05s ago");
        assert_eq!(fmt_ago(now, Some(at(10_000 - 7500))), "2h05m ago");
        assert_eq!(fmt_until(now, None), "pending");
        assert_eq!(fmt_until(now, Some(at(10_012))), "in 12s");
        assert_eq!(fmt_until(now, Some(at(10_000))), "due");
        assert_eq!(fmt_until(now, Some(at(9900))), "due");
    }
}
