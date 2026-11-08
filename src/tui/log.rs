//! Event log panel.

use std::collections::VecDeque;
use std::time::SystemTime;

use chrono::{DateTime, Local};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::engine::NoticeLevel;
use crate::model::Event;

/// Lines kept in memory.
const MAX_LINES: usize = 500;

/// Colour class of a log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogStyle {
    Free,
    Taken,
    Error,
    Info,
}

impl LogStyle {
    fn style(self) -> Style {
        match self {
            LogStyle::Free => Style::new().fg(Color::Green),
            LogStyle::Taken => Style::new().fg(Color::Yellow),
            LogStyle::Error => Style::new().fg(Color::Red),
            LogStyle::Info => Style::new().fg(Color::Gray),
        }
    }
}

impl From<NoticeLevel> for LogStyle {
    fn from(level: NoticeLevel) -> Self {
        match level {
            NoticeLevel::Info => LogStyle::Info,
            NoticeLevel::Error => LogStyle::Error,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub at: SystemTime,
    pub style: LogStyle,
    pub text: String,
}

/// Human-readable line and colour for an event.
pub fn describe(event: &Event) -> (LogStyle, String) {
    match event {
        Event::BecameFree {
            code,
            previous_owner,
        } => (
            LogStyle::Free,
            format!(
                "{code} is FREE (was: {})",
                previous_owner.as_deref().unwrap_or("unknown")
            ),
        ),
        Event::BecameTaken { code, guild_name } => (
            LogStyle::Taken,
            format!(
                "{code} was taken again by {}",
                guild_name.as_deref().unwrap_or("unknown")
            ),
        ),
        Event::CheckerDegraded { code, reason } => (
            LogStyle::Error,
            format!("{code}: checker degraded ({reason})"),
        ),
        Event::ClaimSucceeded { code } => (LogStyle::Free, format!("{code}: claimed")),
        Event::ClaimFailed { code, reason } => {
            (LogStyle::Error, format!("{code}: claim failed ({reason})"))
        }
        Event::ClaimNeedsVerification { code } => (
            LogStyle::Error,
            format!("{code}: claim needs manual verification"),
        ),
    }
}

/// `HH:MM:SS` in the local time zone.
pub fn clock(at: SystemTime) -> String {
    DateTime::<Local>::from(at).format("%H:%M:%S").to_string()
}

/// Bounded log with a scroll position counted in lines up from the newest.
#[derive(Debug, Default)]
pub struct Log {
    lines: VecDeque<LogLine>,
    scroll: usize,
}

impl Log {
    pub fn push(&mut self, at: SystemTime, style: LogStyle, text: impl Into<String>) {
        if self.lines.len() == MAX_LINES {
            self.lines.pop_front();
        }
        self.lines.push_back(LogLine {
            at,
            style,
            text: text.into(),
        });
        // Keep the view anchored while the user reads older lines.
        if self.scroll > 0 {
            self.scroll = (self.scroll + 1).min(self.lines.len() - 1);
        }
    }

    pub fn lines(&self) -> impl Iterator<Item = &LogLine> {
        self.lines.iter()
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub fn scroll_up(&mut self, n: usize) {
        self.scroll = (self.scroll + n).min(self.lines.len().saturating_sub(1));
    }

    pub fn scroll_down(&mut self, n: usize) {
        self.scroll = self.scroll.saturating_sub(n);
    }

    /// The lines that fit in `height` rows, oldest first.
    fn window(&self, height: usize) -> Vec<&LogLine> {
        let end = self.lines.len().saturating_sub(self.scroll);
        let start = end.saturating_sub(height);
        self.lines.range(start..end).collect()
    }
}

pub fn render(frame: &mut Frame, area: Rect, log: &Log) {
    let title = if log.scroll > 0 {
        format!(" Events (local time) - scrolled up {} ", log.scroll)
    } else {
        " Events (local time) ".to_owned()
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let height = usize::from(block.inner(area).height);
    let lines: Vec<Line> = log
        .window(height)
        .into_iter()
        .map(|l| {
            let mut style = l.style.style();
            if l.style == LogStyle::Free {
                style = style.add_modifier(Modifier::BOLD);
            }
            Line::from(vec![
                Span::styled(clock(l.at), Style::new().fg(Color::DarkGray)),
                Span::raw(" "),
                Span::styled(l.text.clone(), style),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn t(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn event_colours() {
        let free = Event::BecameFree {
            code: "a".into(),
            previous_owner: None,
        };
        let taken = Event::BecameTaken {
            code: "a".into(),
            guild_name: None,
        };
        let degraded = Event::CheckerDegraded {
            code: "a".into(),
            reason: "x".into(),
        };
        assert_eq!(describe(&free).0, LogStyle::Free);
        assert_eq!(describe(&taken).0, LogStyle::Taken);
        assert_eq!(describe(&degraded).0, LogStyle::Error);
    }

    #[test]
    fn clock_uses_the_local_time_zone() {
        let at = t(1_700_000_000);
        let text = clock(at);
        assert_eq!(text.len(), 8);
        assert_eq!(&text[2..3], ":");
        // Same instant rendered through chrono's local zone, whatever the machine's zone is.
        let local = DateTime::<Local>::from(at);
        assert_eq!(text, local.format("%H:%M:%S").to_string());
        // The offset is applied: UTC time of day differs from local by the zone offset.
        let utc = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let offset = i64::from(local.offset().local_minus_utc());
        let shifted = utc + chrono::Duration::seconds(offset);
        assert_eq!(text, shifted.format("%H:%M:%S").to_string());
    }

    #[test]
    fn window_and_scroll() {
        let mut log = Log::default();
        for i in 0..10 {
            log.push(t(i), LogStyle::Info, format!("l{i}"));
        }
        let texts = |log: &Log, h| {
            log.window(h)
                .into_iter()
                .map(|l| l.text.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(texts(&log, 3), ["l7", "l8", "l9"]);
        log.scroll_up(4);
        assert_eq!(texts(&log, 3), ["l3", "l4", "l5"]);
        // New lines do not move the view while scrolled.
        log.push(t(10), LogStyle::Info, "l10");
        assert_eq!(texts(&log, 3), ["l3", "l4", "l5"]);
        log.scroll_down(100);
        assert_eq!(texts(&log, 2), ["l9", "l10"]);
        log.scroll_up(1000);
        assert_eq!(log.scroll, log.len() - 1);
    }

    #[test]
    fn log_is_bounded() {
        let mut log = Log::default();
        for i in 0..(MAX_LINES + 20) {
            log.push(t(0), LogStyle::Info, format!("{i}"));
        }
        assert_eq!(log.len(), MAX_LINES);
        assert_eq!(log.lines().next().map(|l| l.text.as_str()), Some("20"));
    }
}
