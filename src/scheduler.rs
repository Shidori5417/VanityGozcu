//! Runtime-mutable check scheduler.
//!
//! A single actor task owns the target map and receives [`Command`]s over an mpsc
//! channel. Each target runs in its own task, so removing a target aborts it
//! (including any in-flight request) and `CheckNow` wakes it immediately. Results are
//! emitted as [`CheckEvent`]s on an output channel.
//!
//! Every check first passes the shared [`RateLimiter`]; `RateLimited` results are
//! reported but not counted as errors and the target is retried once the global
//! pause ends. `Error` results back off exponentially per target.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use rand::RngExt;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tracing::{debug, warn};

use crate::checker::Checker;
use crate::model::{CheckResult, VanityTarget};
use crate::ratelimit::{RateLimiter, backoff_delay};

const COMMAND_CHANNEL_SIZE: usize = 64;
const EVENT_CHANNEL_SIZE: usize = 256;

/// Runtime changes to the watched set.
#[derive(Debug, Clone)]
pub enum Command {
    AddTarget(VanityTarget),
    RemoveTarget(String),
    CheckNow(String),
}

/// One finished check.
#[derive(Debug, Clone)]
pub struct CheckEvent {
    pub code: String,
    pub result: CheckResult,
    pub at: SystemTime,
}

#[derive(Debug, Clone)]
pub struct SchedulerSettings {
    pub default_interval: Duration,
    pub jitter_percent: u8,
    pub backoff_max: Duration,
}

/// Handle to a running scheduler.
#[derive(Debug)]
pub struct Scheduler {
    pub commands: mpsc::Sender<Command>,
    pub events: mpsc::Receiver<CheckEvent>,
    pub task: JoinHandle<()>,
}

/// Starts the scheduler with `initial` targets. It stops when the command sender and
/// all its clones are dropped, or when the event receiver is dropped.
pub fn spawn(
    settings: SchedulerSettings,
    checker: Arc<Checker>,
    limiter: Arc<RateLimiter>,
    initial: Vec<VanityTarget>,
) -> Scheduler {
    let (command_tx, command_rx) = mpsc::channel(COMMAND_CHANNEL_SIZE);
    let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_SIZE);
    let ctx = Arc::new(Context {
        settings,
        checker,
        limiter,
    });
    let task = tokio::spawn(run_actor(ctx, initial, command_rx, event_tx));
    Scheduler {
        commands: command_tx,
        events: event_rx,
        task,
    }
}

struct Context {
    settings: SchedulerSettings,
    checker: Arc<Checker>,
    limiter: Arc<RateLimiter>,
}

/// Aborts the per-target task when dropped.
struct TargetTask {
    wake: Arc<Notify>,
    handle: JoinHandle<()>,
}

impl Drop for TargetTask {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn start_target(
    ctx: &Arc<Context>,
    target: VanityTarget,
    events: &mpsc::Sender<CheckEvent>,
) -> TargetTask {
    let wake = Arc::new(Notify::new());
    let handle = tokio::spawn(run_target(
        ctx.clone(),
        target,
        wake.clone(),
        events.clone(),
    ));
    TargetTask { wake, handle }
}

async fn run_actor(
    ctx: Arc<Context>,
    initial: Vec<VanityTarget>,
    mut commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<CheckEvent>,
) {
    let mut targets: HashMap<String, TargetTask> = HashMap::new();
    for target in initial {
        targets.insert(target.code.clone(), start_target(&ctx, target, &events));
    }
    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else { break };
                match command {
                    Command::AddTarget(target) => {
                        if targets.contains_key(&target.code) {
                            warn!(code = %target.code, "target already scheduled, ignoring add");
                        } else {
                            targets.insert(target.code.clone(), start_target(&ctx, target, &events));
                        }
                    }
                    Command::RemoveTarget(code) => {
                        if targets.remove(&code).is_none() {
                            debug!(%code, "remove for unknown target");
                        }
                    }
                    Command::CheckNow(code) => match targets.get(&code) {
                        Some(task) => task.wake.notify_one(),
                        None => debug!(%code, "check-now for unknown target"),
                    },
                }
            }
            () = events.closed() => break,
        }
    }
}

async fn run_target(
    ctx: Arc<Context>,
    target: VanityTarget,
    wake: Arc<Notify>,
    events: mpsc::Sender<CheckEvent>,
) {
    let interval = target
        .effective_interval(ctx.settings.default_interval)
        .max(Duration::from_millis(1));
    let mut failures: u32 = 0;
    loop {
        ctx.limiter.acquire().await;
        let checked = ctx.checker.check(&target.code).await;
        ctx.limiter.observe(&checked.result, &checked.rate);

        let delay = match &checked.result {
            CheckResult::Error(_) => {
                failures = failures.saturating_add(1);
                backoff_delay(interval, failures, ctx.settings.backoff_max)
            }
            // The global pause makes the next `acquire` wait; retry right after it.
            CheckResult::RateLimited { .. } => Duration::ZERO,
            _ => {
                failures = 0;
                interval
            }
        };
        let event = CheckEvent {
            code: target.code.clone(),
            result: checked.result,
            at: SystemTime::now(),
        };
        if events.send(event).await.is_err() {
            return;
        }
        let delay = if delay.is_zero() {
            delay
        } else {
            jittered(delay, ctx.settings.jitter_percent)
        };
        tokio::select! {
            () = sleep(delay) => {}
            () = wake.notified() => {}
        }
    }
}

/// Scales `base` by a random factor in `1 ± percent/100`.
pub fn jittered(base: Duration, percent: u8) -> Duration {
    if percent == 0 {
        return base;
    }
    let spread = f64::from(percent.min(100)) / 100.0;
    let factor = 1.0 + rand::rng().random_range(-spread..=spread);
    base.mul_f64(factor)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::time::timeout;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const WAIT: Duration = Duration::from_secs(5);

    fn settings(default_interval: Duration) -> SchedulerSettings {
        SchedulerSettings {
            default_interval,
            jitter_percent: 0,
            backoff_max: Duration::from_secs(60),
        }
    }

    async fn setup(
        server: &MockServer,
        default_interval: Duration,
        initial: Vec<VanityTarget>,
    ) -> Scheduler {
        let checker = Arc::new(Checker::new(&server.uri(), Duration::from_secs(2)).unwrap());
        let limiter = Arc::new(RateLimiter::with_burst(60_000, 100));
        spawn(settings(default_interval), checker, limiter, initial)
    }

    async fn mount_taken(server: &MockServer, code: &str) {
        Mock::given(method("GET"))
            .and(path(format!("/api/v10/invites/{code}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"guild": {"id": "1", "name": "G"}})),
            )
            .mount(server)
            .await;
    }

    async fn count_requests(server: &MockServer, code: &str) -> usize {
        let p = format!("/api/v10/invites/{code}");
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() == p)
            .count()
    }

    #[test]
    fn jitter_stays_in_range() {
        let base = Duration::from_secs(100);
        for _ in 0..1000 {
            let d = jittered(base, 15);
            assert!(
                d >= Duration::from_secs(85) && d <= Duration::from_secs(115),
                "{d:?}"
            );
        }
        assert_eq!(jittered(base, 0), base);
    }

    #[tokio::test]
    async fn added_target_is_checked_repeatedly() {
        let server = MockServer::start().await;
        mount_taken(&server, "abc").await;
        let mut s = setup(&server, Duration::from_millis(50), vec![]).await;
        s.commands
            .send(Command::AddTarget(VanityTarget::new("abc")))
            .await
            .unwrap();
        for _ in 0..3 {
            let ev = timeout(WAIT, s.events.recv()).await.unwrap().unwrap();
            assert_eq!(ev.code, "abc");
            assert!(matches!(ev.result, CheckResult::Taken { .. }));
        }
    }

    #[tokio::test]
    async fn remove_target_stops_checks() {
        let server = MockServer::start().await;
        mount_taken(&server, "abc").await;
        let mut s = setup(
            &server,
            Duration::from_millis(30),
            vec![VanityTarget::new("abc")],
        )
        .await;
        timeout(WAIT, s.events.recv()).await.unwrap().unwrap();
        s.commands
            .send(Command::RemoveTarget("abc".into()))
            .await
            .unwrap();
        sleep(Duration::from_millis(150)).await;
        let after_remove = count_requests(&server, "abc").await;
        sleep(Duration::from_millis(300)).await;
        assert_eq!(count_requests(&server, "abc").await, after_remove);
    }

    #[tokio::test]
    async fn check_now_triggers_immediately() {
        let server = MockServer::start().await;
        mount_taken(&server, "abc").await;
        let mut s = setup(
            &server,
            Duration::from_secs(3600),
            vec![VanityTarget::new("abc")],
        )
        .await;
        // First check runs on start, then the target sleeps for an hour.
        timeout(WAIT, s.events.recv()).await.unwrap().unwrap();
        sleep(Duration::from_millis(100)).await;
        s.commands
            .send(Command::CheckNow("abc".into()))
            .await
            .unwrap();
        let ev = timeout(WAIT, s.events.recv()).await.unwrap().unwrap();
        assert_eq!(ev.code, "abc");
        assert_eq!(count_requests(&server, "abc").await, 2);
    }

    #[tokio::test]
    async fn errors_back_off() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let mut s = setup(
            &server,
            Duration::from_millis(100),
            vec![VanityTarget::new("abc")],
        )
        .await;
        let first = timeout(WAIT, s.events.recv()).await.unwrap().unwrap();
        assert!(matches!(first.result, CheckResult::Error(_)));
        // After one failure the next check waits 2x the interval (200ms).
        sleep(Duration::from_millis(120)).await;
        assert_eq!(count_requests(&server, "abc").await, 1);
        timeout(WAIT, s.events.recv()).await.unwrap().unwrap();
        assert_eq!(count_requests(&server, "abc").await, 2);
    }

    #[tokio::test]
    async fn rate_limit_pauses_all_targets() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v10/invites/aa"))
            .respond_with(
                ResponseTemplate::new(429)
                    .set_body_json(json!({"retry_after": 0.4, "global": false})),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        mount_taken(&server, "aa").await;
        mount_taken(&server, "bb").await;
        let started = tokio::time::Instant::now();
        let mut s = setup(&server, Duration::from_secs(3600), vec![]).await;
        s.commands
            .send(Command::AddTarget(VanityTarget::new("aa")))
            .await
            .unwrap();
        let first = timeout(WAIT, s.events.recv()).await.unwrap().unwrap();
        assert!(matches!(first.result, CheckResult::RateLimited { .. }));
        // A different target added during the pause must wait for it too.
        s.commands
            .send(Command::AddTarget(VanityTarget::new("bb")))
            .await
            .unwrap();
        for _ in 0..2 {
            let ev = timeout(WAIT, s.events.recv()).await.unwrap().unwrap();
            assert!(matches!(ev.result, CheckResult::Taken { .. }));
            assert!(
                started.elapsed() >= Duration::from_millis(400),
                "{:?}",
                started.elapsed()
            );
        }
    }

    #[tokio::test]
    async fn scheduler_stops_when_commands_close() {
        let server = MockServer::start().await;
        let s = setup(&server, Duration::from_secs(1), vec![]).await;
        let Scheduler {
            commands,
            events,
            task,
        } = s;
        drop(commands);
        timeout(WAIT, task).await.unwrap().unwrap();
        drop(events);
    }
}
