//! The engine owns the live monitoring state and is the only writer of `config.toml`.
//!
//! Front ends (the TUI now, a Unix-socket client in stage 5) talk to it through three
//! things only:
//!
//! 1. [`EngineCommand`]s sent over a channel, each answered with a [`CommandReply`];
//! 2. an initial [`Snapshot`] (the reply to [`EngineCommand::GetSnapshot`]);
//! 3. a stream of [`Update`]s (broadcast).
//!
//! Every type that crosses this boundary is `Serialize + Deserialize`, so the same
//! interface can later be carried over a socket as JSON lines unchanged. To get a
//! gap-free view: subscribe to the updates first, then request the snapshot, and
//! ignore updates whose `seq` is not greater than `Snapshot::seq`.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::config::normalize_code;
use crate::config_edit;
use crate::model::{CheckResult, EventRecord, VanityState, VanityTarget};
use crate::notify::{Message, Notifiers};
use crate::pipeline;
use crate::scheduler::{self, CheckEvent, Scheduler};
use crate::state::StateEngine;
use crate::store::Store;

const COMMAND_CHANNEL_SIZE: usize = 64;
pub const UPDATE_BUS_SIZE: usize = 256;
/// Events included in a [`Snapshot`].
const SNAPSHOT_EVENTS: u32 = 100;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("the engine is not running")]
    Closed,
    #[error("the scheduler stopped unexpectedly")]
    SchedulerStopped,
}

/// A request from a front end.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum EngineCommand {
    /// Start watching `code` and add it to the config file.
    AddTarget { code: String },
    /// Stop watching `code` and remove it from the config file.
    RemoveTarget { code: String },
    /// Run the next check of `code` right away.
    CheckNow { code: String },
    /// Send a test message through every active notifier; results arrive as notices.
    TestNotify,
    /// Current state of everything; see the module docs for the subscribe order.
    GetSnapshot,
}

/// The engine's answer to one [`EngineCommand`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum CommandReply {
    Done { message: String },
    Rejected { reason: String },
    Snapshot(Snapshot),
}

/// Kind of the last finished check of a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultKind {
    Taken,
    Free,
    RateLimited,
    Error,
    Unknown,
}

impl From<&CheckResult> for ResultKind {
    fn from(result: &CheckResult) -> Self {
        match result {
            CheckResult::Taken { .. } => Self::Taken,
            CheckResult::Free => Self::Free,
            CheckResult::RateLimited { .. } => Self::RateLimited,
            CheckResult::Error(_) => Self::Error,
            CheckResult::Unknown { .. } => Self::Unknown,
        }
    }
}

/// Everything a front end shows about one target. Always absolute: applying the same
/// view twice is harmless.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetView {
    pub code: String,
    pub state: VanityState,
    pub last_result: Option<ResultKind>,
    /// Display text of the last result (e.g. the error message).
    pub last_detail: Option<String>,
    pub last_checked: Option<SystemTime>,
    pub next_check: Option<SystemTime>,
    /// Per-target `auto_claim` from the config (`None` = not set).
    pub auto_claim: Option<bool>,
}

impl TargetView {
    fn new(target: &VanityTarget, state: VanityState) -> Self {
        Self {
            code: target.code.clone(),
            state,
            last_result: None,
            last_detail: None,
            last_checked: None,
            next_check: None,
            auto_claim: target.auto_claim,
        }
    }

    /// Name of the guild that owns (or last owned) the code, if known.
    pub fn owner(&self) -> Option<&str> {
        match &self.state {
            VanityState::Taken { guild_name, .. } => guild_name.as_deref(),
            VanityState::FreeSuspected { previous_owner, .. } => previous_owner.as_deref(),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    Info,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "update", rename_all = "snake_case")]
pub enum UpdateKind {
    /// A target was added or one of its fields changed (insert or replace by code).
    Target(TargetView),
    TargetRemoved {
        code: String,
    },
    Event(EventRecord),
    /// Free-form engine message (test-notify results, ...).
    Notice {
        level: NoticeLevel,
        text: String,
    },
}

/// One item of the update stream. `seq` increases by one per update.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Update {
    pub seq: u64,
    #[serde(flatten)]
    pub kind: UpdateKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// `seq` of the last update already reflected in this snapshot.
    pub seq: u64,
    pub targets: Vec<TargetView>,
    /// Recent events, oldest first.
    pub events: Vec<EventRecord>,
    /// Startup warnings (config, notifiers).
    pub warnings: Vec<String>,
}

/// A command plus the channel its reply goes to (in-process only).
#[derive(Debug)]
pub struct CommandEnvelope {
    pub command: EngineCommand,
    pub reply: oneshot::Sender<CommandReply>,
}

/// Cheap, cloneable front-end side of the engine.
#[derive(Debug, Clone)]
pub struct EngineHandle {
    commands: mpsc::Sender<CommandEnvelope>,
    updates: broadcast::Sender<Update>,
}

impl EngineHandle {
    pub fn subscribe(&self) -> broadcast::Receiver<Update> {
        self.updates.subscribe()
    }

    pub async fn request(&self, command: EngineCommand) -> Result<CommandReply, EngineError> {
        let (reply, rx) = oneshot::channel();
        self.commands
            .send(CommandEnvelope { command, reply })
            .await
            .map_err(|_| EngineError::Closed)?;
        rx.await.map_err(|_| EngineError::Closed)
    }

    pub async fn snapshot(&self) -> Result<Snapshot, EngineError> {
        match self.request(EngineCommand::GetSnapshot).await? {
            CommandReply::Snapshot(s) => Ok(s),
            _ => Err(EngineError::Closed),
        }
    }
}

/// Everything the engine needs from its surroundings.
pub struct EngineInit {
    pub state: StateEngine,
    pub store: Store,
    /// Event bus the notifier task listens on.
    pub bus: broadcast::Sender<EventRecord>,
    pub scheduler: Scheduler,
    pub notifiers: Arc<Notifiers>,
    pub targets: Vec<VanityTarget>,
    /// File that target changes are written back to.
    pub config_path: PathBuf,
    pub warnings: Vec<String>,
}

pub struct Engine {
    state: StateEngine,
    store: Store,
    bus: broadcast::Sender<EventRecord>,
    bus_rx: broadcast::Receiver<EventRecord>,
    scheduler: Scheduler,
    notifiers: Arc<Notifiers>,
    config_path: PathBuf,
    warnings: Vec<String>,
    rows: Vec<TargetView>,
    seq: u64,
    updates: broadcast::Sender<Update>,
    commands_tx: mpsc::Sender<CommandEnvelope>,
    commands_rx: mpsc::Receiver<CommandEnvelope>,
    notice_tx: mpsc::UnboundedSender<(NoticeLevel, String)>,
    notice_rx: mpsc::UnboundedReceiver<(NoticeLevel, String)>,
}

impl Engine {
    pub fn new(init: EngineInit) -> Self {
        let (commands_tx, commands_rx) = mpsc::channel(COMMAND_CHANNEL_SIZE);
        let (notice_tx, notice_rx) = mpsc::unbounded_channel();
        let rows = init
            .targets
            .iter()
            .map(|t| TargetView::new(t, init.state.state(&t.code)))
            .collect();
        Self {
            bus_rx: init.bus.subscribe(),
            state: init.state,
            store: init.store,
            bus: init.bus,
            scheduler: init.scheduler,
            notifiers: init.notifiers,
            config_path: init.config_path,
            warnings: init.warnings,
            rows,
            seq: 0,
            updates: broadcast::channel(UPDATE_BUS_SIZE).0,
            commands_tx,
            commands_rx,
            notice_tx,
            notice_rx,
        }
    }

    pub fn handle(&self) -> EngineHandle {
        EngineHandle {
            commands: self.commands_tx.clone(),
            updates: self.updates.clone(),
        }
    }

    /// Drives the engine until `shutdown` resolves (returning its output) or the
    /// scheduler dies.
    pub async fn run<F: Future>(&mut self, shutdown: F) -> Result<F::Output, EngineError> {
        tokio::pin!(shutdown);
        let outcome = loop {
            tokio::select! {
                check = self.scheduler.events.recv() => match check {
                    Some(check) => self.handle_check(check).await,
                    None => break Err(EngineError::SchedulerStopped),
                },
                // `self` keeps a sender, so the channel never closes while running.
                Some(envelope) = self.commands_rx.recv() => {
                    let reply = self.handle_command(envelope.command).await;
                    // The requester may have gone away; nothing to do then.
                    let _ = envelope.reply.send(reply);
                }
                Some((level, text)) = self.notice_rx.recv() => {
                    self.emit(UpdateKind::Notice { level, text });
                }
                out = &mut shutdown => break Ok(out),
            }
        };
        self.scheduler.task.abort();
        outcome
    }

    fn emit(&mut self, kind: UpdateKind) {
        self.seq += 1;
        // Err only means nobody is subscribed.
        let _ = self.updates.send(Update {
            seq: self.seq,
            kind,
        });
    }

    fn row_mut(&mut self, code: &str) -> Option<&mut TargetView> {
        self.rows.iter_mut().find(|r| r.code == code)
    }

    async fn handle_check(&mut self, check: CheckEvent) {
        if self.row_mut(&check.code).is_none() {
            debug!(code = %check.code, "dropping result of a removed target");
            return;
        }
        pipeline::process(&mut self.state, &self.store, &self.bus, &check).await;
        let state = self.state.state(&check.code);
        let view = self.row_mut(&check.code).map(|row| {
            row.state = state;
            row.last_result = Some(ResultKind::from(&check.result));
            row.last_detail = Some(check.result.to_string());
            row.last_checked = Some(check.at);
            row.next_check = Some(check.next_at);
            row.clone()
        });
        if let Some(view) = view {
            self.emit(UpdateKind::Target(view));
        }
        loop {
            match self.bus_rx.try_recv() {
                Ok(record) => self.emit(UpdateKind::Event(record)),
                Err(broadcast::error::TryRecvError::Lagged(n)) => {
                    warn!("engine lagged on the event bus, {n} events skipped");
                }
                Err(_) => break,
            }
        }
    }

    async fn handle_command(&mut self, command: EngineCommand) -> CommandReply {
        let result = match command {
            EngineCommand::AddTarget { code } => self.add_target(&code).await,
            EngineCommand::RemoveTarget { code } => self.remove_target(&code).await,
            EngineCommand::CheckNow { code } => self.check_now(&code).await,
            EngineCommand::TestNotify => self.test_notify(),
            EngineCommand::GetSnapshot => return CommandReply::Snapshot(self.snapshot().await),
        };
        match result {
            Ok(message) => CommandReply::Done { message },
            Err(reason) => CommandReply::Rejected { reason },
        }
    }

    async fn snapshot(&self) -> Snapshot {
        let mut events: Vec<EventRecord> = match self.store.recent_events(SNAPSHOT_EVENTS).await {
            Ok(stored) => stored
                .into_iter()
                .map(|s| EventRecord {
                    event: s.event,
                    at: s.at,
                })
                .collect(),
            Err(e) => {
                warn!("cannot load recent events for snapshot: {e}");
                Vec::new()
            }
        };
        events.reverse();
        Snapshot {
            seq: self.seq,
            targets: self.rows.clone(),
            events,
            warnings: self.warnings.clone(),
        }
    }

    async fn add_target(&mut self, raw: &str) -> Result<String, String> {
        let code = normalize_code(raw).map_err(|e| e.to_string())?;
        if self.rows.iter().any(|r| r.code == code) {
            return Err(format!("{code} is already watched"));
        }
        // Persist first so memory never gets ahead of the file.
        config_edit::add_target(&self.config_path, &code)
            .map_err(|e| format!("cannot save {code} to the config: {e}"))?;
        let target = VanityTarget::new(code.clone());
        self.scheduler
            .commands
            .send(scheduler::Command::AddTarget(target.clone()))
            .await
            .map_err(|_| "the scheduler is not running".to_owned())?;
        let view = TargetView::new(&target, self.state.state(&code));
        self.rows.push(view.clone());
        self.emit(UpdateKind::Target(view));
        info!(%code, "target added");
        Ok(format!("added {code}"))
    }

    async fn remove_target(&mut self, raw: &str) -> Result<String, String> {
        let code = raw.to_lowercase();
        if !self.rows.iter().any(|r| r.code == code) {
            return Err(format!("{code} is not watched"));
        }
        match config_edit::remove_target(&self.config_path, &code) {
            // Not in the file (e.g. it was edited by hand meanwhile): still stop watching.
            Ok(()) | Err(config_edit::ConfigEditError::NotFound(_)) => {}
            Err(e) => return Err(format!("cannot remove {code} from the config: {e}")),
        }
        self.scheduler
            .commands
            .send(scheduler::Command::RemoveTarget(code.clone()))
            .await
            .map_err(|_| "the scheduler is not running".to_owned())?;
        self.rows.retain(|r| r.code != code);
        self.emit(UpdateKind::TargetRemoved { code: code.clone() });
        info!(%code, "target removed");
        Ok(format!("removed {code}"))
    }

    async fn check_now(&mut self, raw: &str) -> Result<String, String> {
        let code = raw.to_lowercase();
        if !self.rows.iter().any(|r| r.code == code) {
            return Err(format!("{code} is not watched"));
        }
        self.scheduler
            .commands
            .send(scheduler::Command::CheckNow(code.clone()))
            .await
            .map_err(|_| "the scheduler is not running".to_owned())?;
        Ok(format!("checking {code} now"))
    }

    fn test_notify(&self) -> Result<String, String> {
        if self.notifiers.is_empty() {
            return Err(
                "no notifier is active (enable [notify.discord]/[notify.telegram] and set the \
                 environment variables)"
                    .to_owned(),
            );
        }
        let notifiers = self.notifiers.clone();
        let notices = self.notice_tx.clone();
        tokio::spawn(async move {
            let message = Message::test(SystemTime::now());
            for (name, result) in notifiers.send_all(&message).await {
                let notice = match result {
                    Ok(()) => (NoticeLevel::Info, format!("test notification sent: {name}")),
                    Err(e) => (NoticeLevel::Error, format!("test notification failed: {e}")),
                };
                // The engine may already be gone on shutdown.
                let _ = notices.send(notice);
            }
        });
        Ok("sending test notification".to_owned())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::checker::Checker;
    use crate::config::{Config, Secrets};
    use crate::model::Event;
    use crate::notify::Flags;
    use crate::ratelimit::RateLimiter;
    use crate::scheduler::SchedulerSettings;
    use serde_json::json;
    use std::time::Duration;
    use tokio::time::timeout;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const WAIT: Duration = Duration::from_secs(5);
    const SAMPLE: &str =
        "# keep me\n[general]\nlog_level = \"info\"\n\n[[targets]]\ncode = \"abc\"  # inline\n";

    struct Fixture {
        handle: EngineHandle,
        config: PathBuf,
        _dir: tempfile::TempDir,
        _server: MockServer,
    }

    fn codes(path: &std::path::Path) -> Vec<String> {
        let raw = std::fs::read_to_string(path).unwrap();
        let loaded = Config::parse(&raw, Secrets::default()).unwrap();
        loaded.config.targets.into_iter().map(|t| t.code).collect()
    }

    /// Starts an engine against a mock server where every code is taken.
    async fn fixture(initial: &[&str]) -> Fixture {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex_invites())
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"guild": {"id": "1", "name": "Guild"}})),
            )
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, SAMPLE).unwrap();

        let checker = Arc::new(Checker::new(&server.uri(), Duration::from_secs(2)).unwrap());
        let limiter = Arc::new(RateLimiter::with_burst(60_000, 100));
        let targets: Vec<VanityTarget> = initial.iter().map(|c| VanityTarget::new(*c)).collect();
        let scheduler = scheduler::spawn(
            SchedulerSettings {
                default_interval: Duration::from_secs(3600),
                jitter_percent: 0,
                backoff_max: Duration::from_secs(60),
            },
            checker,
            limiter,
            targets.clone(),
        );
        let flags = Flags {
            on_free: true,
            on_taken: false,
            on_claim: true,
            on_error: true,
        };
        let mut engine = Engine::new(EngineInit {
            state: StateEngine::new(1),
            store: Store::open_in_memory().unwrap(),
            bus: pipeline::event_bus(),
            scheduler,
            notifiers: Arc::new(Notifiers::new(flags, vec![])),
            targets,
            config_path: config.clone(),
            warnings: vec!["a warning".into()],
        });
        let handle = engine.handle();
        tokio::spawn(async move { engine.run(std::future::pending::<()>()).await });
        Fixture {
            handle,
            config,
            _dir: dir,
            _server: server,
        }
    }

    fn path_regex_invites() -> wiremock::matchers::PathRegexMatcher {
        wiremock::matchers::path_regex(r"^/api/v10/invites/.+$")
    }

    async fn request(f: &Fixture, command: EngineCommand) -> CommandReply {
        timeout(WAIT, f.handle.request(command))
            .await
            .unwrap()
            .unwrap()
    }

    fn add(code: &str) -> EngineCommand {
        EngineCommand::AddTarget { code: code.into() }
    }

    async fn next_target_update(rx: &mut broadcast::Receiver<Update>, code: &str) -> TargetView {
        loop {
            let u = timeout(WAIT, rx.recv()).await.unwrap().unwrap();
            if let UpdateKind::Target(v) = u.kind
                && v.code == code
                && v.last_result.is_some()
            {
                return v;
            }
        }
    }

    #[tokio::test]
    async fn add_target_persists_schedules_and_reports() {
        let f = fixture(&["abc"]).await;
        let mut rx = f.handle.subscribe();

        let reply = request(&f, add("New-One")).await;
        assert!(matches!(reply, CommandReply::Done { .. }), "{reply:?}");

        // Persisted with comments intact.
        let raw = std::fs::read_to_string(&f.config).unwrap();
        assert!(
            raw.contains("# keep me") && raw.contains("# inline"),
            "{raw}"
        );
        assert_eq!(codes(&f.config), ["abc", "new-one"]);

        // The scheduler picked it up: a real check result arrives for it.
        let view = next_target_update(&mut rx, "new-one").await;
        assert_eq!(view.last_result, Some(ResultKind::Taken));
        assert!(view.last_checked.is_some() && view.next_check.is_some());
        assert!(matches!(view.state, VanityState::Taken { .. }));

        let snap = f.handle.snapshot().await.unwrap();
        let snap_codes: Vec<_> = snap.targets.iter().map(|t| t.code.as_str()).collect();
        assert_eq!(snap_codes, ["abc", "new-one"]);
        assert_eq!(snap.warnings, ["a warning"]);
    }

    #[tokio::test]
    async fn invalid_and_duplicate_codes_are_rejected_without_touching_the_file() {
        let f = fixture(&["abc"]).await;
        let before = std::fs::read_to_string(&f.config).unwrap();
        for bad in ["a", "has space", "UPPER_case", "abc", "ABC"] {
            let reply = request(&f, add(bad)).await;
            assert!(
                matches!(reply, CommandReply::Rejected { .. }),
                "{bad}: {reply:?}"
            );
        }
        assert_eq!(std::fs::read_to_string(&f.config).unwrap(), before);
        assert_eq!(f.handle.snapshot().await.unwrap().targets.len(), 1);
    }

    #[tokio::test]
    async fn remove_target_updates_file_and_stream() {
        let f = fixture(&["abc"]).await;
        let mut rx = f.handle.subscribe();
        let reply = request(&f, EngineCommand::RemoveTarget { code: "abc".into() }).await;
        assert!(matches!(reply, CommandReply::Done { .. }), "{reply:?}");
        assert!(codes(&f.config).is_empty());
        assert!(
            std::fs::read_to_string(&f.config)
                .unwrap()
                .contains("# keep me")
        );
        loop {
            let u = timeout(WAIT, rx.recv()).await.unwrap().unwrap();
            if u.kind == (UpdateKind::TargetRemoved { code: "abc".into() }) {
                break;
            }
        }
        assert!(f.handle.snapshot().await.unwrap().targets.is_empty());

        let again = request(&f, EngineCommand::RemoveTarget { code: "abc".into() }).await;
        assert!(matches!(again, CommandReply::Rejected { .. }));
    }

    #[tokio::test]
    async fn check_now_and_test_notify_replies() {
        let f = fixture(&["abc"]).await;
        let mut rx = f.handle.subscribe();
        next_target_update(&mut rx, "abc").await;
        let reply = request(&f, EngineCommand::CheckNow { code: "abc".into() }).await;
        assert!(matches!(reply, CommandReply::Done { .. }));
        // The woken target reports again although its interval is an hour.
        next_target_update(&mut rx, "abc").await;

        let unknown = request(&f, EngineCommand::CheckNow { code: "zz".into() }).await;
        assert!(matches!(unknown, CommandReply::Rejected { .. }));
        let notify = request(&f, EngineCommand::TestNotify).await;
        assert!(matches!(notify, CommandReply::Rejected { .. }));
    }

    #[tokio::test]
    async fn updates_have_increasing_seq_and_snapshot_seq_dedupes() {
        let f = fixture(&[]).await;
        let mut rx = f.handle.subscribe();
        request(&f, add("one")).await;
        let snap = f.handle.snapshot().await.unwrap();
        request(&f, add("two")).await;
        let mut seqs = Vec::new();
        while seqs.len() < 2 {
            let u = timeout(WAIT, rx.recv()).await.unwrap().unwrap();
            seqs.push(u.seq);
        }
        assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
        assert!(snap.seq >= 1);
        assert!(
            seqs.iter().any(|s| *s <= snap.seq),
            "stream replays updates older than the snapshot, which clients must skip"
        );
    }

    #[tokio::test]
    async fn events_are_forwarded_to_the_stream() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v10/invites/abc"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(json!({"code": 10006, "message": "x"})),
            )
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("c.toml");
        let checker = Arc::new(Checker::new(&server.uri(), Duration::from_secs(2)).unwrap());
        let scheduler = scheduler::spawn(
            SchedulerSettings {
                default_interval: Duration::from_secs(3600),
                jitter_percent: 0,
                backoff_max: Duration::from_secs(60),
            },
            checker,
            Arc::new(RateLimiter::with_burst(60_000, 100)),
            vec![VanityTarget::new("abc")],
        );
        let flags = Flags {
            on_free: true,
            on_taken: false,
            on_claim: true,
            on_error: true,
        };
        let mut engine = Engine::new(EngineInit {
            state: StateEngine::new(1),
            store: Store::open_in_memory().unwrap(),
            bus: pipeline::event_bus(),
            scheduler,
            notifiers: Arc::new(Notifiers::new(flags, vec![])),
            targets: vec![VanityTarget::new("abc")],
            config_path: config,
            warnings: vec![],
        });
        let handle = engine.handle();
        let mut rx = handle.subscribe();
        tokio::spawn(async move { engine.run(std::future::pending::<()>()).await });
        loop {
            let u = timeout(WAIT, rx.recv()).await.unwrap().unwrap();
            if let UpdateKind::Event(r) = u.kind {
                assert!(matches!(r.event, Event::BecameFree { .. }));
                break;
            }
        }
        let snap = handle.snapshot().await.unwrap();
        assert_eq!(snap.targets[0].state, VanityState::Free);
        assert_eq!(snap.events.len(), 1);
    }

    #[test]
    fn protocol_types_round_trip_as_json_lines() {
        let commands = [
            add("abc"),
            EngineCommand::RemoveTarget { code: "abc".into() },
            EngineCommand::CheckNow { code: "abc".into() },
            EngineCommand::TestNotify,
            EngineCommand::GetSnapshot,
        ];
        for c in commands {
            let line = serde_json::to_string(&c).unwrap();
            assert!(!line.contains('\n'));
            assert_eq!(serde_json::from_str::<EngineCommand>(&line).unwrap(), c);
        }

        let view = TargetView {
            code: "abc".into(),
            state: VanityState::Taken {
                guild_id: Some("1".into()),
                guild_name: Some("G".into()),
            },
            last_result: Some(ResultKind::Taken),
            last_detail: Some("TAKEN".into()),
            last_checked: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(5)),
            next_check: None,
            auto_claim: Some(true),
        };
        let updates = [
            Update {
                seq: 1,
                kind: UpdateKind::Target(view.clone()),
            },
            Update {
                seq: 2,
                kind: UpdateKind::TargetRemoved { code: "abc".into() },
            },
            Update {
                seq: 3,
                kind: UpdateKind::Event(EventRecord {
                    event: Event::BecameFree {
                        code: "abc".into(),
                        previous_owner: None,
                    },
                    at: SystemTime::UNIX_EPOCH + Duration::from_secs(9),
                }),
            },
            Update {
                seq: 4,
                kind: UpdateKind::Notice {
                    level: NoticeLevel::Error,
                    text: "x".into(),
                },
            },
        ];
        for u in updates {
            let line = serde_json::to_string(&u).unwrap();
            assert_eq!(serde_json::from_str::<Update>(&line).unwrap(), u);
        }
        let replies = [
            CommandReply::Done {
                message: "ok".into(),
            },
            CommandReply::Rejected {
                reason: "no".into(),
            },
            CommandReply::Snapshot(Snapshot {
                seq: 7,
                targets: vec![view],
                events: vec![],
                warnings: vec!["w".into()],
            }),
        ];
        for r in replies {
            let line = serde_json::to_string(&r).unwrap();
            assert_eq!(serde_json::from_str::<CommandReply>(&line).unwrap(), r);
        }
    }
}
