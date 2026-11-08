//! Terminal UI.
//!
//! The TUI never touches the config or the scheduler: it renders an [`App`] that is fed
//! by the engine's snapshot and update stream, and turns key presses into
//! [`EngineCommand`]s (see `engine.rs`). [`App`] is pure state with no I/O so it can be
//! unit-tested without a terminal.

pub mod dialogs;
pub mod log;
pub mod table;

use std::io;
use std::time::{Duration, SystemTime};

use crossterm::event::{
    Event as TermEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures_util::StreamExt;
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style};
use ratatui::widgets::Paragraph;
use thiserror::Error;
use tokio::sync::{broadcast, mpsc};
use tokio::time::{MissedTickBehavior, interval};

use crate::engine::{
    CommandReply, EngineCommand, EngineError, EngineHandle, NoticeLevel, Snapshot, TargetView,
    Update, UpdateKind,
};
use dialogs::{Dialog, Outcome};
use log::{Log, LogStyle};

/// Refresh period for relative times ("5s ago").
const TICK: Duration = Duration::from_secs(1);
/// Lines moved by PageUp/PageDown in the log.
const LOG_PAGE: usize = 5;

#[derive(Debug, Error)]
pub enum TuiError {
    #[error("terminal I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Engine(#[from] EngineError),
}

/// What the event loop must do after a key press.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Quit,
    Send(EngineCommand),
}

/// Pure UI state.
#[derive(Debug)]
pub struct App {
    pub targets: Vec<TargetView>,
    pub selected: usize,
    pub log: Log,
    pub dialog: Option<Dialog>,
    pub now: SystemTime,
    last_seq: u64,
    loaded: bool,
}

impl App {
    pub fn new(now: SystemTime) -> Self {
        Self {
            targets: Vec::new(),
            selected: 0,
            log: Log::default(),
            dialog: None,
            now,
            last_seq: 0,
            loaded: false,
        }
    }

    pub fn selected_code(&self) -> Option<&str> {
        self.targets.get(self.selected).map(|t| t.code.as_str())
    }

    fn clamp_selection(&mut self) {
        self.selected = self.selected.min(self.targets.len().saturating_sub(1));
    }

    /// Replaces the target list. The first snapshot also fills the log with the startup
    /// warnings and recent events.
    pub fn apply_snapshot(&mut self, snapshot: Snapshot) {
        self.targets = snapshot.targets;
        self.last_seq = snapshot.seq;
        self.clamp_selection();
        if !self.loaded {
            self.loaded = true;
            for w in snapshot.warnings {
                self.log
                    .push(self.now, LogStyle::Error, format!("warning: {w}"));
            }
            for record in &snapshot.events {
                let (style, text) = log::describe(&record.event);
                self.log.push(record.at, style, text);
            }
        }
    }

    /// Applies one stream update; updates the snapshot already covered are skipped.
    pub fn apply_update(&mut self, update: Update) {
        if update.seq <= self.last_seq {
            return;
        }
        self.last_seq = update.seq;
        match update.kind {
            UpdateKind::Target(view) => {
                match self.targets.iter_mut().find(|t| t.code == view.code) {
                    Some(slot) => *slot = view,
                    None => self.targets.push(view),
                }
            }
            UpdateKind::TargetRemoved { code } => {
                self.targets.retain(|t| t.code != code);
                self.clamp_selection();
            }
            UpdateKind::Event(record) => {
                let (style, text) = log::describe(&record.event);
                self.log.push(record.at, style, text);
            }
            UpdateKind::Notice { level, text } => {
                self.log.push(self.now, LogStyle::from(level), text);
            }
        }
    }

    /// Shows the engine's answer to a command in the log.
    pub fn apply_reply(&mut self, reply: CommandReply) {
        match reply {
            CommandReply::Done { message } => self.log.push(self.now, LogStyle::Info, message),
            CommandReply::Rejected { reason } => self.log.push(
                self.now,
                LogStyle::from(NoticeLevel::Error),
                format!("rejected: {reason}"),
            ),
            CommandReply::Snapshot(_) => {}
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Some(Action::Quit);
        }
        if let Some(dialog) = self.dialog.as_mut() {
            return match dialog.handle_key(key, &self.targets) {
                Outcome::Keep => None,
                Outcome::Close => {
                    self.dialog = None;
                    None
                }
                Outcome::Submit(command) => {
                    self.dialog = None;
                    Some(Action::Send(command))
                }
            };
        }
        match key.code {
            KeyCode::Char('q') => return Some(Action::Quit),
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(self.targets.len().saturating_sub(1));
            }
            KeyCode::Home | KeyCode::Char('g') => self.selected = 0,
            KeyCode::End | KeyCode::Char('G') => {
                self.selected = self.targets.len().saturating_sub(1);
            }
            KeyCode::PageUp => self.log.scroll_up(LOG_PAGE),
            KeyCode::PageDown => self.log.scroll_down(LOG_PAGE),
            KeyCode::Char('a') => self.dialog = Some(Dialog::add()),
            KeyCode::Char('d') => {
                if let Some(code) = self.selected_code() {
                    self.dialog = Some(Dialog::ConfirmDelete {
                        code: code.to_owned(),
                    });
                }
            }
            KeyCode::Char('c') => {
                if let Some(code) = self.selected_code() {
                    return Some(Action::Send(EngineCommand::CheckNow {
                        code: code.to_owned(),
                    }));
                }
            }
            KeyCode::Char('t') => return Some(Action::Send(EngineCommand::TestNotify)),
            KeyCode::Char('m') => self.log.push(
                self.now,
                LogStyle::Info,
                "mode switching (notify / auto-claim) arrives in stage 4",
            ),
            _ => {}
        }
        None
    }
}

const HELP: &str =
    " a add  d delete  c check now  t test notify  m mode  j/k move  PgUp/PgDn log  q quit";

pub fn draw(frame: &mut Frame, app: &App) {
    let [main, events, footer] = Layout::vertical([
        Constraint::Percentage(55),
        Constraint::Percentage(45),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    table::render(frame, main, &app.targets, app.selected, app.now);
    log::render(frame, events, &app.log);
    frame.render_widget(
        Paragraph::new(HELP).style(Style::new().fg(Color::DarkGray)),
        footer,
    );
    if let Some(dialog) = &app.dialog {
        dialogs::render(frame, frame.area(), dialog);
    }
}

/// Puts the terminal back into its normal state. Safe to call more than once.
fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = crossterm::execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
}

/// Raw mode + alternate screen for as long as it lives. The release profile aborts on
/// panic (no unwinding, so `Drop` would not run), hence the panic hook restores the
/// terminal first so the panic message stays readable.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            previous(info);
        }));
        enable_raw_mode()?;
        let guard = Self;
        crossterm::execute!(io::stdout(), EnterAlternateScreen)?;
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Runs the UI until the user quits. The terminal is restored on every exit path.
pub async fn run(handle: EngineHandle) -> Result<(), TuiError> {
    // Subscribe before asking for the snapshot so no update falls in between.
    let mut updates = handle.subscribe();
    let mut app = App::new(SystemTime::now());
    app.apply_snapshot(handle.snapshot().await?);

    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut terminal_events = EventStream::new();
    let mut tick = interval(TICK);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let (reply_tx, mut replies) = mpsc::unbounded_channel::<Result<CommandReply, EngineError>>();

    loop {
        terminal.draw(|frame| draw(frame, &app))?;
        tokio::select! {
            event = terminal_events.next() => match event {
                Some(Ok(TermEvent::Key(key))) if key.kind != KeyEventKind::Release => {
                    match app.handle_key(key) {
                        Some(Action::Quit) => return Ok(()),
                        Some(Action::Send(command)) => {
                            let handle = handle.clone();
                            let reply_tx = reply_tx.clone();
                            tokio::spawn(async move {
                                let _ = reply_tx.send(handle.request(command).await);
                            });
                        }
                        None => {}
                    }
                }
                // Resize and other events just fall through to the redraw.
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.into()),
                None => return Ok(()),
            },
            update = updates.recv() => match update {
                Ok(update) => app.apply_update(update),
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    app.apply_snapshot(handle.snapshot().await?);
                    app.log.push(
                        app.now,
                        LogStyle::Error,
                        format!("UI fell behind, {n} updates skipped; state was reloaded"),
                    );
                }
                Err(broadcast::error::RecvError::Closed) => return Err(EngineError::Closed.into()),
            },
            Some(reply) = replies.recv() => app.apply_reply(reply?),
            _ = tick.tick() => app.now = SystemTime::now(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::engine::ResultKind;
    use crate::model::{Event, EventRecord, VanityState};
    use ratatui::backend::TestBackend;
    use std::time::UNIX_EPOCH;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn view(code: &str) -> TargetView {
        TargetView {
            code: code.into(),
            state: VanityState::Taken {
                guild_id: None,
                guild_name: Some("Guild X".into()),
            },
            last_result: Some(ResultKind::Taken),
            last_detail: None,
            last_checked: Some(at(995)),
            next_check: Some(at(1012)),
            auto_claim: None,
        }
    }

    fn snapshot(codes: &[&str], seq: u64) -> Snapshot {
        Snapshot {
            seq,
            targets: codes.iter().map(|c| view(c)).collect(),
            events: vec![],
            warnings: vec![],
        }
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn app_with(codes: &[&str]) -> App {
        let mut app = App::new(at(1000));
        app.apply_snapshot(snapshot(codes, 5));
        app
    }

    fn update(seq: u64, kind: UpdateKind) -> Update {
        Update { seq, kind }
    }

    fn log_texts(app: &App) -> Vec<String> {
        app.log.lines().map(|l| l.text.clone()).collect()
    }

    #[test]
    fn stale_updates_are_skipped_and_new_ones_applied() {
        let mut app = app_with(&["abc"]);
        let mut changed = view("abc");
        changed.state = VanityState::Free;
        app.apply_update(update(5, UpdateKind::Target(changed.clone())));
        assert!(matches!(app.targets[0].state, VanityState::Taken { .. }));
        app.apply_update(update(6, UpdateKind::Target(changed)));
        assert_eq!(app.targets[0].state, VanityState::Free);
        app.apply_update(update(7, UpdateKind::Target(view("new"))));
        assert_eq!(app.targets.len(), 2);
        app.apply_update(update(8, UpdateKind::TargetRemoved { code: "abc".into() }));
        assert_eq!(app.targets.len(), 1);
    }

    #[test]
    fn events_and_notices_reach_the_log() {
        let mut app = app_with(&[]);
        app.apply_update(update(
            6,
            UpdateKind::Event(EventRecord {
                event: Event::BecameFree {
                    code: "abc".into(),
                    previous_owner: Some("G".into()),
                },
                at: at(900),
            }),
        ));
        app.apply_update(update(
            7,
            UpdateKind::Notice {
                level: NoticeLevel::Error,
                text: "boom".into(),
            },
        ));
        let lines: Vec<_> = app.log.lines().collect();
        assert_eq!(lines[0].style, LogStyle::Free);
        assert!(lines[0].text.contains("abc is FREE"));
        assert_eq!(lines[1].style, LogStyle::Error);
    }

    #[test]
    fn first_snapshot_loads_history_later_ones_do_not() {
        let mut app = App::new(at(1000));
        let mut snap = snapshot(&["abc"], 3);
        snap.warnings = vec!["careful".into()];
        snap.events = vec![EventRecord {
            event: Event::BecameTaken {
                code: "abc".into(),
                guild_name: None,
            },
            at: at(10),
        }];
        app.apply_snapshot(snap.clone());
        assert_eq!(app.log.len(), 2);
        app.apply_snapshot(snap);
        assert_eq!(app.log.len(), 2);
    }

    #[test]
    fn navigation_clamps_and_follows_removal() {
        let mut app = app_with(&["a1", "b2", "c3"]);
        app.handle_key(key('j'));
        app.handle_key(key('j'));
        app.handle_key(key('j'));
        assert_eq!(app.selected_code(), Some("c3"));
        app.handle_key(key('k'));
        assert_eq!(app.selected_code(), Some("b2"));
        app.handle_key(key('G'));
        app.apply_update(update(6, UpdateKind::TargetRemoved { code: "c3".into() }));
        assert_eq!(app.selected_code(), Some("b2"));
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.selected_code(), Some("a1"));
    }

    #[test]
    fn keys_produce_actions() {
        let mut app = app_with(&["abc"]);
        assert_eq!(
            app.handle_key(key('c')),
            Some(Action::Send(EngineCommand::CheckNow { code: "abc".into() }))
        );
        assert_eq!(
            app.handle_key(key('t')),
            Some(Action::Send(EngineCommand::TestNotify))
        );
        assert_eq!(app.handle_key(key('q')), Some(Action::Quit));
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Action::Quit)
        );
        app.handle_key(key('m'));
        assert!(log_texts(&app)[0].contains("stage 4"));
    }

    #[test]
    fn add_flow_validates_then_sends() {
        let mut app = app_with(&["abc"]);
        assert_eq!(app.handle_key(key('a')), None);
        assert!(app.dialog.is_some());
        // 'q' is text inside the dialog, not quit.
        assert_eq!(app.handle_key(key('q')), None);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            None
        );
        assert!(matches!(
            &app.dialog,
            Some(Dialog::Add { error: Some(_), .. })
        ));
        app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        for c in "Neu".chars() {
            app.handle_key(key(c));
        }
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(Action::Send(EngineCommand::AddTarget {
                code: "neu".into()
            }))
        );
        assert!(app.dialog.is_none());
        // Ctrl-C quits even with a dialog open.
        app.handle_key(key('a'));
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Action::Quit)
        );
    }

    #[test]
    fn delete_requires_confirmation() {
        let mut app = app_with(&["abc"]);
        assert_eq!(app.handle_key(key('d')), None);
        assert_eq!(
            app.dialog,
            Some(Dialog::ConfirmDelete { code: "abc".into() })
        );
        assert_eq!(app.handle_key(key('n')), None);
        assert!(app.dialog.is_none());
        app.handle_key(key('d'));
        assert_eq!(
            app.handle_key(key('y')),
            Some(Action::Send(EngineCommand::RemoveTarget {
                code: "abc".into()
            }))
        );
        // Nothing selected: no dialog, no action.
        let mut empty = app_with(&[]);
        assert_eq!(empty.handle_key(key('d')), None);
        assert_eq!(empty.handle_key(key('c')), None);
        assert!(empty.dialog.is_none());
    }

    #[test]
    fn replies_are_logged() {
        let mut app = app_with(&[]);
        app.apply_reply(CommandReply::Done {
            message: "added abc".into(),
        });
        app.apply_reply(CommandReply::Rejected {
            reason: "nope".into(),
        });
        let lines: Vec<_> = app.log.lines().collect();
        assert_eq!(lines[0].style, LogStyle::Info);
        assert_eq!(lines[1].style, LogStyle::Error);
    }

    fn render_to_string(app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn render_smoke_shows_table_log_and_dialog() {
        let mut app = app_with(&["ornekkod"]);
        app.log
            .push(at(0), LogStyle::Free, "ornekkod is FREE (was: Old)");
        let text = render_to_string(&app, 110, 30);
        for needle in [
            "ornekkod", "taken", "Guild X", "5s ago", "in 12s", "is FREE",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
        app.handle_key(key('a'));
        assert!(render_to_string(&app, 110, 30).contains("Add target"));
        // Tiny terminals must not panic.
        let _ = render_to_string(&app, 20, 5);
        let _ = render_to_string(&App::new(at(0)), 1, 1);
    }
}
