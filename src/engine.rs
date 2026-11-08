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

use std::collections::HashSet;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::claimer::{ClaimOutcome, Claimer, PrecheckOutcome};

use crate::config::normalize_code;
use crate::config_edit;
use crate::model::{CheckResult, Event, EventRecord, VanityState, VanityTarget};
use crate::notify::{Kind, Message, Notifiers};
use crate::pipeline;
use crate::scheduler::{self, CheckEvent, Scheduler};
use crate::state::StateEngine;
use crate::store::Store;

const COMMAND_CHANNEL_SIZE: usize = 64;
pub const UPDATE_BUS_SIZE: usize = 256;
/// Events included in a [`Snapshot`].
const SNAPSHOT_EVENTS: u32 = 100;
/// Delay before a code that just looked free is re-checked (instead of waiting a full
/// interval); the re-check still goes through the rate limiter.
pub const DEFAULT_RECHECK_DELAY: Duration = Duration::from_millis(1500);
/// Prefix of the entry the engine keeps in [`Snapshot::warnings`] for auto-claim problems.
const CLAIM_WARNING_PREFIX: &str = "auto-claim: ";

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
    /// Switch between notify-only (`false`) and auto-claim (`true`) mode and write
    /// `[auto_claim] enabled` to the config file.
    SetAutoClaim { enabled: bool },
    /// Enable or disable auto-claim for one target and write its `auto_claim` flag.
    SetTargetAutoClaim { code: String, enabled: bool },
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

/// Where the auto-claim preconditions stand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PrecheckState {
    /// Cannot work in this run (no token, no guild id, unusable token).
    Unavailable {
        reason: String,
    },
    /// Not run yet (auto-claim has not been switched on).
    Idle,
    Running,
    Ok {
        note: Option<String>,
    },
    Failed {
        reason: String,
    },
    /// Discord rejected the token or asked for verification: no further attempts
    /// until the program is restarted.
    Halted {
        reason: String,
    },
}

/// Auto-claim mode as shown in the UI header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimStatus {
    /// The user's switch (`[auto_claim] enabled`).
    pub mode: bool,
    pub precheck: PrecheckState,
}

impl ClaimStatus {
    /// Claims are actually fired: mode on and the precheck passed.
    pub fn active(&self) -> bool {
        self.mode && matches!(self.precheck, PrecheckState::Ok { .. })
    }
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
    /// The auto-claim mode or its precheck changed.
    AutoClaim(ClaimStatus),
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
    /// Startup warnings (config, notifiers, auto-claim precheck).
    pub warnings: Vec<String>,
    pub claim: ClaimStatus,
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

/// Auto-claim wiring handed to the engine.
pub struct ClaimInit {
    /// `None` when claiming cannot work (no token or guild id); see `unavailable`.
    pub claimer: Option<Arc<Claimer>>,
    /// Why there is no claimer; shown in the UI.
    pub unavailable: Option<String>,
    /// `[auto_claim] enabled` at startup.
    pub enabled: bool,
    /// `general.confirm_checks`.
    pub confirm_checks: u32,
    /// `auto_claim.confirm_checks_override`, used instead while auto-claim is active.
    pub confirm_override: u32,
}

impl ClaimInit {
    /// No claimer; the state engine confirms after `confirm_checks` free results.
    pub fn disabled(confirm_checks: u32) -> Self {
        Self {
            claimer: None,
            unavailable: Some("not configured".to_owned()),
            enabled: false,
            confirm_checks,
            confirm_override: confirm_checks,
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
    pub claim: ClaimInit,
    /// See [`DEFAULT_RECHECK_DELAY`].
    pub recheck_delay: Duration,
}

/// Results of work the engine spawned, fed back into its loop.
#[derive(Debug)]
enum Internal {
    Precheck(PrecheckOutcome),
    Claim { code: String, outcome: ClaimOutcome },
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
    claimer: Option<Arc<Claimer>>,
    claim: ClaimStatus,
    confirm_base: u32,
    confirm_override: u32,
    /// Codes with a claim request in flight.
    claiming: HashSet<String>,
    /// Codes with a fast re-check scheduled.
    recheck_pending: HashSet<String>,
    recheck_delay: Duration,
    internal_tx: mpsc::UnboundedSender<Internal>,
    internal_rx: mpsc::UnboundedReceiver<Internal>,
}

impl Engine {
    pub fn new(init: EngineInit) -> Self {
        let (commands_tx, commands_rx) = mpsc::channel(COMMAND_CHANNEL_SIZE);
        let (notice_tx, notice_rx) = mpsc::unbounded_channel();
        let (internal_tx, internal_rx) = mpsc::unbounded_channel();
        let rows = init
            .targets
            .iter()
            .map(|t| TargetView::new(t, init.state.state(&t.code)))
            .collect();
        let precheck = match (&init.claim.claimer, init.claim.unavailable) {
            (Some(_), _) => PrecheckState::Idle,
            (None, reason) => PrecheckState::Unavailable {
                reason: reason.unwrap_or_else(|| "not configured".to_owned()),
            },
        };
        let claim = ClaimStatus {
            mode: init.claim.enabled && init.claim.claimer.is_some(),
            precheck,
        };
        let mut state = init.state;
        state.set_confirm_checks(init.claim.confirm_checks);
        Self {
            claimer: init.claim.claimer,
            claim,
            confirm_base: init.claim.confirm_checks,
            confirm_override: init.claim.confirm_override,
            claiming: HashSet::new(),
            recheck_pending: HashSet::new(),
            recheck_delay: init.recheck_delay,
            internal_tx,
            internal_rx,
            bus_rx: init.bus.subscribe(),
            state,
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
        if self.claim.mode {
            self.start_precheck();
        }
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
                Some(internal) = self.internal_rx.recv() => self.handle_internal(internal).await,
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
        self.recheck_pending.remove(&check.code);
        if self.claiming.contains(&check.code) {
            // The claim decides the outcome; this result would only race it.
            self.apply_check_to_row(&check, None);
            return;
        }
        let want_claim = self.claim_wanted(&check.code);
        let mut spawned = false;
        pipeline::process_with(&mut self.state, &self.store, &self.bus, &check, |event| {
            // Claim path: spawn the PATCH the instant the state machine confirms the
            // code is free, before the database write.
            if want_claim
                && let (Event::BecameFree { code, .. }, Some(claimer)) = (event, &self.claimer)
            {
                let claimer = claimer.clone();
                let tx = self.internal_tx.clone();
                let code = code.clone();
                spawned = true;
                tokio::spawn(async move {
                    let outcome = claimer.claim(&code).await;
                    // The engine may be gone on shutdown.
                    let _ = tx.send(Internal::Claim { code, outcome });
                });
            }
        })
        .await;
        let state = self.state.state(&check.code);
        if spawned {
            self.claiming.insert(check.code.clone());
            self.emit(UpdateKind::Notice {
                level: NoticeLevel::Info,
                text: format!("{}: free, claim request sent", check.code),
            });
        }
        let suspected = matches!(state, VanityState::FreeSuspected { .. });
        self.apply_check_to_row(&check, Some(state));
        if suspected && self.recheck_pending.insert(check.code.clone()) {
            self.schedule_recheck(check.code);
        }
    }

    /// Updates the row of `check.code` (including its state when given) and publishes
    /// the row and any pending events.
    fn apply_check_to_row(&mut self, check: &CheckEvent, state: Option<VanityState>) {
        let view = self.row_mut(&check.code).map(|row| {
            if let Some(state) = state {
                row.state = state;
            }
            row.last_result = Some(ResultKind::from(&check.result));
            row.last_detail = Some(check.result.to_string());
            row.last_checked = Some(check.at);
            row.next_check = Some(check.next_at);
            row.clone()
        });
        if let Some(view) = view {
            self.emit(UpdateKind::Target(view));
        }
        self.drain_bus();
    }

    fn drain_bus(&mut self) {
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

    /// Wakes the target again after the re-check delay (the scheduler then goes through
    /// the rate limiter as for any check).
    fn schedule_recheck(&self, code: String) {
        let commands = self.scheduler.commands.clone();
        let delay = self.recheck_delay;
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            // The scheduler may be gone on shutdown.
            let _ = commands.send(scheduler::Command::CheckNow(code)).await;
        });
    }

    /// Effective auto-claim for a target: mode on, precheck passed, target not opted out.
    fn claim_wanted(&self, code: &str) -> bool {
        self.claim.active()
            && self.claimer.is_some()
            && self
                .rows
                .iter()
                .find(|r| r.code == code)
                .is_some_and(|r| r.auto_claim != Some(false))
    }

    async fn handle_internal(&mut self, internal: Internal) {
        match internal {
            Internal::Precheck(outcome) => self.finish_precheck(outcome),
            Internal::Claim { code, outcome } => {
                self.claiming.remove(&code);
                pipeline::process_claim(
                    &mut self.state,
                    &self.store,
                    &self.bus,
                    &code,
                    &outcome,
                    SystemTime::now(),
                )
                .await;
                match outcome {
                    ClaimOutcome::InvalidToken => self.halt(
                        "Discord rejected the token (HTTP 401); auto-claim is off until restart"
                            .to_owned(),
                    ),
                    ClaimOutcome::NeedsVerification => self.halt(
                        "Discord asked for verification (captcha / 2FA); auto-claim stopped, \
                         manual action required"
                            .to_owned(),
                    ),
                    ClaimOutcome::Succeeded | ClaimOutcome::Failed { .. } => {}
                }
                let state = self.state.state(&code);
                let view = self.row_mut(&code).map(|row| {
                    row.state = state;
                    row.clone()
                });
                if let Some(view) = view {
                    self.emit(UpdateKind::Target(view));
                }
                self.drain_bus();
            }
        }
    }

    fn start_precheck(&mut self) {
        let Some(claimer) = self.claimer.clone() else {
            return;
        };
        self.set_precheck(PrecheckState::Running);
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(Internal::Precheck(claimer.precheck().await));
        });
    }

    fn finish_precheck(&mut self, outcome: PrecheckOutcome) {
        match outcome {
            PrecheckOutcome::Ok { note } => {
                self.set_claim_warning(None);
                let text = match &note {
                    Some(note) => format!("auto-claim precheck ok ({note})"),
                    None => "auto-claim precheck ok".to_owned(),
                };
                self.set_precheck(PrecheckState::Ok { note });
                self.emit(UpdateKind::Notice {
                    level: NoticeLevel::Info,
                    text,
                });
            }
            PrecheckOutcome::Failed { reason } => {
                let text = format!("precheck failed, auto-claim stays off: {reason}");
                self.set_claim_warning(Some(text.clone()));
                self.set_precheck(PrecheckState::Failed { reason });
                self.emit(UpdateKind::Notice {
                    level: NoticeLevel::Error,
                    text: format!("{CLAIM_WARNING_PREFIX}{text}"),
                });
            }
            PrecheckOutcome::InvalidToken => self.halt(
                "Discord rejected the token (HTTP 401); auto-claim is off until restart".to_owned(),
            ),
            PrecheckOutcome::NeedsVerification => {
                self.halt(
                    "Discord asked for verification (captcha / 2FA); auto-claim stopped, manual \
                     action required"
                        .to_owned(),
                );
                self.send_urgent("Discord asked for verification during the auto-claim precheck");
            }
        }
    }

    /// Stops auto-claim for the rest of the session.
    fn halt(&mut self, reason: String) {
        warn!("{reason}");
        self.set_claim_warning(Some(reason.clone()));
        self.emit(UpdateKind::Notice {
            level: NoticeLevel::Error,
            text: format!("{CLAIM_WARNING_PREFIX}{reason}"),
        });
        self.set_precheck(PrecheckState::Halted { reason });
    }

    /// Replaces the auto-claim entry in the snapshot warnings.
    fn set_claim_warning(&mut self, text: Option<String>) {
        self.warnings
            .retain(|w| !w.starts_with(CLAIM_WARNING_PREFIX));
        if let Some(text) = text {
            self.warnings.push(format!("{CLAIM_WARNING_PREFIX}{text}"));
        }
    }

    fn set_precheck(&mut self, precheck: PrecheckState) {
        self.claim.precheck = precheck;
        self.publish_claim_status();
    }

    /// Applies the confirmation count that fits the mode and tells the front ends.
    fn publish_claim_status(&mut self) {
        let confirm = if self.claim.active() {
            self.confirm_override
        } else {
            self.confirm_base
        };
        self.state.set_confirm_checks(confirm);
        self.emit(UpdateKind::AutoClaim(self.claim.clone()));
    }

    /// Tells the user through every notifier that a human has to step in.
    fn send_urgent(&self, detail: &str) {
        let notifiers = self.notifiers.clone();
        let message = Message {
            title: "URGENT: manual action required - auto-claim stopped".to_owned(),
            code: "auto-claim".to_owned(),
            fields: vec![("Detail".to_owned(), detail.to_owned())],
            at: SystemTime::now(),
            kind: Kind::Urgent,
        };
        tokio::spawn(async move {
            for (name, result) in notifiers.send_all(&message).await {
                if let Err(e) = result {
                    warn!(notifier = name, "urgent notification failed: {e}");
                }
            }
        });
    }

    async fn handle_command(&mut self, command: EngineCommand) -> CommandReply {
        let result = match command {
            EngineCommand::AddTarget { code } => self.add_target(&code).await,
            EngineCommand::RemoveTarget { code } => self.remove_target(&code).await,
            EngineCommand::CheckNow { code } => self.check_now(&code).await,
            EngineCommand::TestNotify => self.test_notify(),
            EngineCommand::SetAutoClaim { enabled } => self.set_auto_claim(enabled),
            EngineCommand::SetTargetAutoClaim { code, enabled } => {
                self.set_target_auto_claim(&code, enabled)
            }
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
            claim: self.claim.clone(),
        }
    }

    fn set_auto_claim(&mut self, enabled: bool) -> Result<String, String> {
        if enabled {
            match &self.claim.precheck {
                PrecheckState::Unavailable { reason } => {
                    return Err(format!("auto-claim is unavailable: {reason}"));
                }
                PrecheckState::Halted { reason } => {
                    return Err(format!("auto-claim is halted: {reason}"));
                }
                _ => {}
            }
        }
        // Persist first so memory never gets ahead of the file.
        config_edit::set_auto_claim_enabled(&self.config_path, enabled)
            .map_err(|e| format!("cannot save auto-claim mode to the config: {e}"))?;
        self.claim.mode = enabled;
        info!(enabled, "auto-claim mode changed");
        if enabled
            && matches!(
                self.claim.precheck,
                PrecheckState::Idle | PrecheckState::Failed { .. }
            )
        {
            self.start_precheck();
            return Ok("auto-claim mode ON; running the precheck first".to_owned());
        }
        self.publish_claim_status();
        Ok(if enabled {
            "auto-claim mode ON".to_owned()
        } else {
            "auto-claim mode OFF (notify only)".to_owned()
        })
    }

    fn set_target_auto_claim(&mut self, raw: &str, enabled: bool) -> Result<String, String> {
        let code = raw.to_lowercase();
        if !self.rows.iter().any(|r| r.code == code) {
            return Err(format!("{code} is not watched"));
        }
        config_edit::set_target_auto_claim(&self.config_path, &code, enabled)
            .map_err(|e| format!("cannot save auto_claim for {code} to the config: {e}"))?;
        let view = self.row_mut(&code).map(|row| {
            row.auto_claim = Some(enabled);
            row.clone()
        });
        if let Some(view) = view {
            self.emit(UpdateKind::Target(view));
        }
        Ok(format!(
            "auto-claim for {code} {}",
            if enabled { "ON" } else { "OFF" }
        ))
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
            claim: ClaimInit::disabled(1),
            recheck_delay: Duration::from_secs(3600),
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
            claim: ClaimInit::disabled(1),
            recheck_delay: Duration::from_secs(3600),
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
            EngineCommand::SetAutoClaim { enabled: true },
            EngineCommand::SetTargetAutoClaim {
                code: "abc".into(),
                enabled: false,
            },
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
            Update {
                seq: 5,
                kind: UpdateKind::AutoClaim(ClaimStatus {
                    mode: true,
                    precheck: PrecheckState::Ok {
                        note: Some("n".into()),
                    },
                }),
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
                claim: ClaimStatus {
                    mode: false,
                    precheck: PrecheckState::Failed { reason: "r".into() },
                },
            }),
        ];
        for r in replies {
            let line = serde_json::to_string(&r).unwrap();
            assert_eq!(serde_json::from_str::<CommandReply>(&line).unwrap(), r);
        }
    }

    // ---- auto-claim ----------------------------------------------------------------

    use crate::claimer::Claimer;
    use crate::notify::DiscordNotifier;
    use crate::store::Store as TestStore;
    use secrecy::SecretString;

    const GUILD: &str = "123456789012345678";
    const TOKEN: &str = "mfa.ENGINE-TEST-SECRET-token";

    struct Rig {
        handle: EngineHandle,
        rx: broadcast::Receiver<Update>,
        server: MockServer,
        store: TestStore,
        config: PathBuf,
        _dir: tempfile::TempDir,
    }

    struct RigOpts {
        mode: bool,
        has_claimer: bool,
        target_flag: Option<bool>,
        confirm: u32,
        confirm_override: u32,
        /// After the first free answer the code is taken again by someone else.
        taken_again: bool,
        guild: ResponseTemplate,
        patch: ResponseTemplate,
        recheck_delay: Duration,
        webhook: bool,
    }

    fn guild_ok() -> ResponseTemplate {
        ResponseTemplate::new(200)
            .set_body_json(json!({"features": ["VANITY_URL"], "permissions": "32"}))
    }

    fn opts() -> RigOpts {
        RigOpts {
            mode: true,
            has_claimer: true,
            target_flag: None,
            confirm: 1,
            confirm_override: 1,
            taken_again: false,
            guild: guild_ok(),
            patch: ResponseTemplate::new(200).set_body_json(json!({"code": "abc"})),
            recheck_delay: Duration::from_secs(3600),
            webhook: false,
        }
    }

    async fn rig(o: RigOpts) -> Rig {
        let server = MockServer::start().await;
        let taken = || {
            ResponseTemplate::new(200).set_body_json(json!({"guild": {"id": "1", "name": "Old"}}))
        };
        let free =
            || ResponseTemplate::new(404).set_body_json(json!({"code": 10006, "message": "x"}));
        let invites = || Mock::given(method("GET")).and(path("/api/v10/invites/abc"));
        invites()
            .respond_with(taken())
            .up_to_n_times(1)
            .mount(&server)
            .await;
        if o.taken_again {
            invites()
                .respond_with(free())
                .up_to_n_times(1)
                .mount(&server)
                .await;
            invites().respond_with(taken()).mount(&server).await;
        } else {
            invites().respond_with(free()).mount(&server).await;
        }
        Mock::given(method("GET"))
            .and(path(format!("/api/v10/guilds/{GUILD}")))
            .respond_with(o.guild.clone())
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path(format!("/api/v10/guilds/{GUILD}/vanity-url")))
            .respond_with(o.patch.clone())
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/webhook"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let flag = o
            .target_flag
            .map_or(String::new(), |f| format!("auto_claim = {f}\n"));
        std::fs::write(
            &config,
            format!(
                "[auto_claim]\nenabled = {}  # keep\n\n[[targets]]\ncode = \"abc\"\n{flag}",
                o.mode
            ),
        )
        .unwrap();
        let mut target = VanityTarget::new("abc");
        target.auto_claim = o.target_flag;

        let limiter = Arc::new(RateLimiter::with_burst(60_000, 100));
        let checker = Arc::new(Checker::new(&server.uri(), Duration::from_secs(2)).unwrap());
        let scheduler = scheduler::spawn(
            SchedulerSettings {
                default_interval: Duration::from_secs(3600),
                jitter_percent: 0,
                backoff_max: Duration::from_secs(60),
            },
            checker,
            limiter.clone(),
            vec![target.clone()],
        );
        let claim = if o.has_claimer {
            ClaimInit {
                claimer: Some(Arc::new(
                    Claimer::new(
                        &server.uri(),
                        &SecretString::from(TOKEN.to_owned()),
                        GUILD,
                        Duration::from_secs(2),
                        3,
                        limiter,
                    )
                    .unwrap(),
                )),
                unavailable: None,
                enabled: o.mode,
                confirm_checks: o.confirm,
                confirm_override: o.confirm_override,
            }
        } else {
            ClaimInit::disabled(o.confirm)
        };
        let flags = Flags {
            on_free: false,
            on_taken: false,
            on_claim: false,
            on_error: false,
        };
        let notifiers = if o.webhook {
            let hook = DiscordNotifier::new(
                SecretString::from(format!("{}/webhook", server.uri())),
                None,
                Duration::from_secs(2),
            )
            .unwrap();
            Notifiers::new(flags, vec![crate::notify::Notifier::Discord(hook)])
        } else {
            Notifiers::new(flags, vec![])
        };
        let store = TestStore::open_in_memory().unwrap();
        let bus = pipeline::event_bus();
        let notifiers = Arc::new(notifiers);
        drop(pipeline::spawn_notifier(notifiers.clone(), bus.subscribe()));
        let mut engine = Engine::new(EngineInit {
            state: StateEngine::new(o.confirm),
            store: store.clone(),
            bus,
            scheduler,
            notifiers,
            targets: vec![target],
            config_path: config.clone(),
            warnings: vec![],
            claim,
            recheck_delay: o.recheck_delay,
        });
        let handle = engine.handle();
        let rx = handle.subscribe();
        tokio::spawn(async move { engine.run(std::future::pending::<()>()).await });
        Rig {
            handle,
            rx,
            server,
            store,
            config,
            _dir: dir,
        }
    }

    async fn requests_to(rig: &Rig, http_method: &str, path_suffix: &str) -> usize {
        rig.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method.as_str() == http_method && r.url.path().ends_with(path_suffix))
            .count()
    }

    async fn wait_snapshot(rig: &Rig, ok: impl Fn(&Snapshot) -> bool) -> Snapshot {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let snap = rig.handle.snapshot().await.unwrap();
            if ok(&snap) {
                return snap;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out: {snap:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn wait_event(rig: &mut Rig, ok: impl Fn(&Event) -> bool) -> Event {
        loop {
            let u = timeout(WAIT, rig.rx.recv()).await.unwrap().unwrap();
            if let UpdateKind::Event(r) = u.kind
                && ok(&r.event)
            {
                return r.event;
            }
        }
    }

    /// Waits for the initial "taken" check and the precheck (when armed), then makes the
    /// scheduler look at the code again.
    async fn arm_and_recheck(rig: &Rig, armed: bool) {
        wait_snapshot(rig, |s| {
            s.targets[0].last_result == Some(ResultKind::Taken) && (!armed || s.claim.active())
        })
        .await;
        let reply = rig
            .handle
            .request(EngineCommand::CheckNow { code: "abc".into() })
            .await
            .unwrap();
        assert!(matches!(reply, CommandReply::Done { .. }));
    }

    const QUIET: Duration = Duration::from_millis(400);

    #[tokio::test]
    async fn became_free_with_auto_claim_on_sends_the_patch_and_marks_claimed() {
        // confirm_checks 3 would need three free results; the override makes one enough.
        let mut r = rig(RigOpts {
            confirm: 3,
            confirm_override: 1,
            ..opts()
        })
        .await;
        arm_and_recheck(&r, true).await;
        wait_event(&mut r, |e| matches!(e, Event::BecameFree { .. })).await;
        let done = wait_event(&mut r, |e| matches!(e, Event::ClaimSucceeded { .. })).await;
        assert_eq!(done, Event::ClaimSucceeded { code: "abc".into() });
        assert_eq!(requests_to(&r, "PATCH", "/vanity-url").await, 1);

        let snap = wait_snapshot(&r, |s| s.targets[0].state == VanityState::Claimed).await;
        assert!(
            snap.events
                .iter()
                .any(|e| matches!(e.event, Event::ClaimSucceeded { .. }))
        );
        // Persisted as "owner is us".
        let stored = r.store.load_states().await.unwrap();
        assert_eq!(stored[0].state, VanityState::Claimed);

        // Later checks (free or taken) change nothing, publish nothing, claim nothing.
        let reply = r
            .handle
            .request(EngineCommand::CheckNow { code: "abc".into() })
            .await
            .unwrap();
        assert!(matches!(reply, CommandReply::Done { .. }));
        tokio::time::sleep(QUIET).await;
        while let Ok(u) = r.rx.try_recv() {
            assert!(!matches!(u.kind, UpdateKind::Event(_)), "{u:?}");
        }
        assert_eq!(requests_to(&r, "PATCH", "/vanity-url").await, 1);
        assert_eq!(
            r.handle.snapshot().await.unwrap().targets[0].state,
            VanityState::Claimed
        );
        assert_eq!(r.store.recent_events(10).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn claim_request_never_contains_the_token_in_events_or_updates() {
        let mut r = rig(opts()).await;
        arm_and_recheck(&r, true).await;
        wait_event(&mut r, |e| matches!(e, Event::ClaimSucceeded { .. })).await;
        let snap = r.handle.snapshot().await.unwrap();
        assert!(!format!("{snap:?}").contains("ENGINE-TEST-SECRET"));
        let header = r.server.received_requests().await.unwrap();
        let patch = header
            .iter()
            .find(|q| q.method.as_str() == "PATCH")
            .unwrap();
        assert_eq!(
            patch
                .headers
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            TOKEN
        );
        assert_eq!(String::from_utf8_lossy(&patch.body), r#"{"code":"abc"}"#);
    }

    #[tokio::test]
    async fn notify_only_mode_does_not_claim() {
        let mut r = rig(RigOpts {
            mode: false,
            ..opts()
        })
        .await;
        arm_and_recheck(&r, false).await;
        wait_event(&mut r, |e| matches!(e, Event::BecameFree { .. })).await;
        tokio::time::sleep(QUIET).await;
        assert_eq!(requests_to(&r, "PATCH", "/vanity-url").await, 0);
        assert_eq!(
            requests_to(&r, "GET", &format!("/guilds/{GUILD}")).await,
            0,
            "no precheck while off"
        );
        assert_eq!(
            r.handle.snapshot().await.unwrap().targets[0].state,
            VanityState::Free
        );
    }

    #[tokio::test]
    async fn target_with_auto_claim_off_does_not_claim() {
        let mut r = rig(RigOpts {
            target_flag: Some(false),
            ..opts()
        })
        .await;
        arm_and_recheck(&r, true).await;
        wait_event(&mut r, |e| matches!(e, Event::BecameFree { .. })).await;
        tokio::time::sleep(QUIET).await;
        assert_eq!(requests_to(&r, "PATCH", "/vanity-url").await, 0);
        assert_eq!(
            r.handle.snapshot().await.unwrap().targets[0].state,
            VanityState::Free
        );
    }

    #[tokio::test]
    async fn failed_precheck_keeps_auto_claim_off_and_warns() {
        let mut r = rig(RigOpts {
            guild: ResponseTemplate::new(200).set_body_json(json!({"features": ["COMMUNITY"]})),
            ..opts()
        })
        .await;
        let snap = wait_snapshot(&r, |s| {
            matches!(s.claim.precheck, PrecheckState::Failed { .. })
        })
        .await;
        assert!(snap.claim.mode && !snap.claim.active());
        assert!(
            snap.warnings
                .iter()
                .any(|w| w.contains("VANITY_URL") && w.contains("precheck failed")),
            "{:?}",
            snap.warnings
        );
        // The failure also reaches the log as a red notice.
        loop {
            let u = timeout(WAIT, r.rx.recv()).await.unwrap().unwrap();
            if let UpdateKind::Notice {
                level: NoticeLevel::Error,
                text,
            } = u.kind
                && text.contains("precheck failed")
            {
                break;
            }
        }
        arm_and_recheck(&r, false).await;
        wait_event(&mut r, |e| matches!(e, Event::BecameFree { .. })).await;
        tokio::time::sleep(QUIET).await;
        assert_eq!(requests_to(&r, "PATCH", "/vanity-url").await, 0);
    }

    #[tokio::test]
    async fn invalid_token_halts_auto_claim_for_the_session() {
        let mut r = rig(RigOpts {
            patch: ResponseTemplate::new(401)
                .set_body_json(json!({"code": 0, "message": "401: Unauthorized"})),
            ..opts()
        })
        .await;
        arm_and_recheck(&r, true).await;
        let failed = wait_event(&mut r, |e| matches!(e, Event::ClaimFailed { .. })).await;
        assert!(
            matches!(&failed, Event::ClaimFailed { reason, .. } if reason.contains("invalid token"))
        );
        let snap = wait_snapshot(&r, |s| {
            matches!(s.claim.precheck, PrecheckState::Halted { .. })
        })
        .await;
        assert!(!snap.claim.active());
        // Cannot be switched back on without a restart.
        let reply = r
            .handle
            .request(EngineCommand::SetAutoClaim { enabled: true })
            .await
            .unwrap();
        assert!(matches!(reply, CommandReply::Rejected { .. }), "{reply:?}");
        assert_eq!(requests_to(&r, "PATCH", "/vanity-url").await, 1);
    }

    #[tokio::test]
    async fn verification_request_stops_and_sends_an_urgent_notification_despite_flags() {
        let mut r = rig(RigOpts {
            patch: ResponseTemplate::new(400).set_body_json(json!({
                "captcha_key": ["captcha-required"], "captcha_sitekey": "k", "captcha_service": "hcaptcha"
            })),
            webhook: true,
            ..opts()
        })
        .await;
        arm_and_recheck(&r, true).await;
        let ev = wait_event(&mut r, |e| {
            matches!(e, Event::ClaimNeedsVerification { .. })
        })
        .await;
        assert_eq!(ev, Event::ClaimNeedsVerification { code: "abc".into() });
        let snap = wait_snapshot(&r, |s| {
            matches!(s.claim.precheck, PrecheckState::Halted { .. })
        })
        .await;
        assert!(
            snap.warnings
                .iter()
                .any(|w| w.contains("manual action required"))
        );
        assert_eq!(
            requests_to(&r, "PATCH", "/vanity-url").await,
            1,
            "no retry after captcha"
        );

        // Every notify flag is off, yet the webhook got the urgent message.
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let hooks: Vec<_> = r
                .server
                .received_requests()
                .await
                .unwrap()
                .into_iter()
                .filter(|q| q.url.path() == "/webhook")
                .collect();
            if let Some(hook) = hooks.first() {
                let body = String::from_utf8_lossy(&hook.body).to_lowercase();
                assert!(body.contains("manual action required"), "{body}");
                assert_eq!(hooks.len(), 1, "only the verification event is delivered");
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no urgent notification"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn failed_claim_keeps_watching_and_taken_afterwards_is_lost() {
        let mut r = rig(RigOpts {
            taken_again: true,
            patch: ResponseTemplate::new(400)
                .set_body_json(json!({"code": 50035, "message": "Invalid Form Body"})),
            ..opts()
        })
        .await;
        arm_and_recheck(&r, true).await;
        let failed = wait_event(&mut r, |e| matches!(e, Event::ClaimFailed { .. })).await;
        assert!(matches!(&failed, Event::ClaimFailed { reason, .. } if reason.contains("50035")));
        // Still Free, auto-claim still armed (a failed claim is not fatal).
        let snap = wait_snapshot(&r, |s| s.targets[0].state == VanityState::Free).await;
        assert!(snap.claim.active());
        // Someone else grabbed it.
        let reply = r
            .handle
            .request(EngineCommand::CheckNow { code: "abc".into() })
            .await
            .unwrap();
        assert!(matches!(reply, CommandReply::Done { .. }));
        wait_snapshot(&r, |s| s.targets[0].state == VanityState::Lost).await;
        assert_eq!(
            r.store.load_states().await.unwrap()[0].state,
            VanityState::Lost
        );
    }

    #[tokio::test]
    async fn free_suspected_triggers_a_fast_recheck() {
        // Interval is an hour; two free results are needed. Without the fast re-check the
        // second one would never come within the test.
        let mut r = rig(RigOpts {
            mode: false,
            has_claimer: false,
            confirm: 2,
            confirm_override: 2,
            recheck_delay: Duration::from_millis(100),
            ..opts()
        })
        .await;
        arm_and_recheck(&r, false).await;
        let free = wait_event(&mut r, |e| matches!(e, Event::BecameFree { .. })).await;
        assert!(matches!(free, Event::BecameFree { .. }));
        // Exactly three invite lookups: taken, free (suspected), free (confirmed).
        assert_eq!(requests_to(&r, "GET", "/invites/abc").await, 3);
    }

    #[tokio::test]
    async fn fast_recheck_waits_for_the_delay_and_goes_through_the_scheduler() {
        let mut r = rig(RigOpts {
            mode: false,
            has_claimer: false,
            confirm: 2,
            confirm_override: 2,
            recheck_delay: Duration::from_millis(600),
            ..opts()
        })
        .await;
        arm_and_recheck(&r, false).await;
        wait_snapshot(&r, |s| {
            matches!(s.targets[0].state, VanityState::FreeSuspected { .. })
        })
        .await;
        let suspected_at = tokio::time::Instant::now();
        wait_event(&mut r, |e| matches!(e, Event::BecameFree { .. })).await;
        assert!(
            suspected_at.elapsed() >= Duration::from_millis(500),
            "{:?}",
            suspected_at.elapsed()
        );
    }

    #[tokio::test]
    async fn mode_toggles_are_persisted_to_the_config() {
        let r = rig(RigOpts {
            mode: false,
            ..opts()
        })
        .await;
        let ask = |c: EngineCommand| r.handle.request(c);

        assert!(matches!(
            ask(EngineCommand::SetAutoClaim { enabled: true })
                .await
                .unwrap(),
            CommandReply::Done { .. }
        ));
        let raw = std::fs::read_to_string(&r.config).unwrap();
        assert!(raw.contains("enabled = true  # keep"), "{raw}");
        // Switching on runs the precheck and arms the claimer.
        let snap = wait_snapshot(&r, |s| s.claim.active()).await;
        assert!(snap.claim.mode);
        assert_eq!(requests_to(&r, "GET", &format!("/guilds/{GUILD}")).await, 1);

        assert!(matches!(
            ask(EngineCommand::SetTargetAutoClaim {
                code: "ABC".into(),
                enabled: false
            })
            .await
            .unwrap(),
            CommandReply::Done { .. }
        ));
        let raw = std::fs::read_to_string(&r.config).unwrap();
        let loaded = Config::parse(&raw, Secrets::default()).unwrap();
        assert_eq!(loaded.config.targets[0].auto_claim, Some(false));
        assert_eq!(
            r.handle.snapshot().await.unwrap().targets[0].auto_claim,
            Some(false)
        );

        assert!(matches!(
            ask(EngineCommand::SetAutoClaim { enabled: false })
                .await
                .unwrap(),
            CommandReply::Done { .. }
        ));
        let raw = std::fs::read_to_string(&r.config).unwrap();
        assert!(raw.contains("enabled = false  # keep"), "{raw}");
        let snap = r.handle.snapshot().await.unwrap();
        assert!(!snap.claim.mode && !snap.claim.active());
        // Turning it on again reuses the passed precheck (no second GET).
        ask(EngineCommand::SetAutoClaim { enabled: true })
            .await
            .unwrap();
        assert!(r.handle.snapshot().await.unwrap().claim.active());
        assert_eq!(requests_to(&r, "GET", &format!("/guilds/{GUILD}")).await, 1);

        assert!(matches!(
            ask(EngineCommand::SetTargetAutoClaim {
                code: "nope".into(),
                enabled: true
            })
            .await
            .unwrap(),
            CommandReply::Rejected { .. }
        ));
    }

    #[tokio::test]
    async fn without_token_the_mode_cannot_be_enabled_and_the_file_is_untouched() {
        let r = rig(RigOpts {
            mode: false,
            has_claimer: false,
            ..opts()
        })
        .await;
        let before = std::fs::read_to_string(&r.config).unwrap();
        let reply = r
            .handle
            .request(EngineCommand::SetAutoClaim { enabled: true })
            .await
            .unwrap();
        assert!(
            matches!(&reply, CommandReply::Rejected { reason } if reason.contains("unavailable")),
            "{reply:?}"
        );
        assert_eq!(std::fs::read_to_string(&r.config).unwrap(), before);
        let snap = r.handle.snapshot().await.unwrap();
        assert!(matches!(
            snap.claim.precheck,
            PrecheckState::Unavailable { .. }
        ));
    }
}
