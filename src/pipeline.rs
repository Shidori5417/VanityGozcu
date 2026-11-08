//! Glue between the scheduler output, the state engine, the store and the event bus.

use std::sync::Arc;
use std::time::SystemTime;

use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use crate::claimer::ClaimOutcome;
use crate::model::{CheckResult, Event, EventRecord, VanityState};
use crate::notify::Notifiers;
use crate::scheduler::CheckEvent;
use crate::state::StateEngine;
use crate::store::{Store, StoreError};

pub const EVENT_BUS_SIZE: usize = 256;

/// Creates the event bus other components (notifier task, TUI) subscribe to.
pub fn event_bus() -> broadcast::Sender<EventRecord> {
    broadcast::channel(EVENT_BUS_SIZE).0
}

/// Loads persisted states into a fresh engine.
pub async fn restore_engine(store: &Store, confirm_checks: u32) -> Result<StateEngine, StoreError> {
    let mut engine = StateEngine::new(confirm_checks);
    let states = store.load_states().await?;
    info!(count = states.len(), "restored persisted states");
    for s in states {
        engine.restore(&s.code, s.state, s.updated_at);
    }
    Ok(engine)
}

/// Applies one check result: updates the engine, persists changes (state first, then
/// the event) and finally publishes the event. Persistence errors are logged and do
/// not stop publishing.
pub async fn process(
    engine: &mut StateEngine,
    store: &Store,
    bus: &broadcast::Sender<EventRecord>,
    check: &CheckEvent,
) {
    process_with(engine, store, bus, check, |_| {}).await;
}

/// Like [`process`], but calls `on_event` synchronously with the event the moment the
/// state machine produces it, before anything is persisted. The claim path hooks in
/// here so that the PATCH is spawned without waiting for the database.
pub async fn process_with<F: FnOnce(&Event)>(
    engine: &mut StateEngine,
    store: &Store,
    bus: &broadcast::Sender<EventRecord>,
    check: &CheckEvent,
    on_event: F,
) {
    let before = engine.state(&check.code);
    let event = engine.apply(&check.code, &check.result, check.at);
    if let Some(event) = &event {
        on_event(event);
    }
    let after = engine.state(&check.code);

    log_result(&check.code, &check.result, &before, &after);

    let record = event.map(|event| EventRecord {
        event,
        at: check.at,
    });
    persist_and_publish(engine, store, bus, &check.code, &before, record, check.at).await;
}

/// Records the outcome of a claim attempt: updates the state (`Claimed` on success),
/// persists, then publishes the matching event.
pub async fn process_claim(
    engine: &mut StateEngine,
    store: &Store,
    bus: &broadcast::Sender<EventRecord>,
    code: &str,
    outcome: &ClaimOutcome,
    at: SystemTime,
) {
    let before = engine.state(code);
    let event = match outcome {
        ClaimOutcome::Succeeded => {
            engine.mark_claimed(code, at);
            Event::ClaimSucceeded {
                code: code.to_owned(),
            }
        }
        ClaimOutcome::Failed { reason } => {
            engine.mark_claim_failed(code);
            Event::ClaimFailed {
                code: code.to_owned(),
                reason: reason.clone(),
            }
        }
        ClaimOutcome::NeedsVerification => {
            engine.mark_claim_failed(code);
            Event::ClaimNeedsVerification {
                code: code.to_owned(),
            }
        }
        ClaimOutcome::InvalidToken => {
            engine.mark_claim_failed(code);
            Event::ClaimFailed {
                code: code.to_owned(),
                reason: "invalid token (HTTP 401); auto-claim is disabled for this session"
                    .to_owned(),
            }
        }
    };
    let record = Some(EventRecord { event, at });
    persist_and_publish(engine, store, bus, code, &before, record, at).await;
}

async fn persist_and_publish(
    engine: &StateEngine,
    store: &Store,
    bus: &broadcast::Sender<EventRecord>,
    code: &str,
    before: &VanityState,
    record: Option<EventRecord>,
    at: SystemTime,
) {
    let after = engine.state(code);
    let persisted = if *before != after {
        let updated_at = engine.record(code).map_or(at, |r| r.updated_at);
        store
            .save(
                code,
                &after,
                updated_at,
                record.as_ref().map(|r| (&r.event, r.at)),
            )
            .await
    } else if let Some(r) = &record {
        store.append_event(&r.event, r.at).await
    } else {
        Ok(())
    };
    if let Err(e) = persisted {
        error!(%code, "failed to persist state: {e}");
    }
    if let Some(record) = record {
        info!(%code, kind = record.event.kind(), "event");
        // Err only means nobody is subscribed.
        let _ = bus.send(record);
    }
}

fn log_result(code: &str, result: &CheckResult, before: &VanityState, after: &VanityState) {
    match result {
        CheckResult::RateLimited { .. } | CheckResult::Error(_) | CheckResult::Unknown { .. } => {
            warn!(%code, "{result}");
        }
        _ if before != after => info!(%code, from = %before, to = %after, "state changed"),
        _ => debug!(%code, state = %after, "unchanged"),
    }
}

/// Spawns the task that turns bus events into notifications. It ends once the bus
/// sender is dropped and every pending event has been delivered.
pub fn spawn_notifier(
    notifiers: Arc<Notifiers>,
    mut rx: broadcast::Receiver<EventRecord>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(record) => notifiers.dispatch(&record).await,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("notifier lagged, {n} events were not delivered");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::model::Event;
    use std::time::Duration;
    use std::time::SystemTime;

    fn check(code: &str, result: CheckResult) -> CheckEvent {
        CheckEvent {
            code: code.to_owned(),
            result,
            at: SystemTime::UNIX_EPOCH + Duration::from_secs(100),
            next_at: SystemTime::UNIX_EPOCH + Duration::from_secs(130),
        }
    }

    fn taken() -> CheckResult {
        CheckResult::Taken {
            guild_id: Some("1".into()),
            guild_name: Some("G".into()),
        }
    }

    #[tokio::test]
    async fn restart_does_not_renotify_an_already_notified_free() {
        let store = Store::open_in_memory().unwrap();
        let bus = event_bus();
        let mut rx = bus.subscribe();

        // First run: taken, then free -> one BecameFree is published and persisted.
        let mut engine = restore_engine(&store, 1).await.unwrap();
        process(&mut engine, &store, &bus, &check("abc", taken())).await;
        process(&mut engine, &store, &bus, &check("abc", CheckResult::Free)).await;
        let first = rx.try_recv().unwrap();
        assert_eq!(
            first.event,
            Event::BecameFree {
                code: "abc".into(),
                previous_owner: Some("G".into())
            }
        );
        assert!(rx.try_recv().is_err());

        // "Restart": brand new engine seeded from the store, still free.
        let mut engine = restore_engine(&store, 1).await.unwrap();
        process(&mut engine, &store, &bus, &check("abc", CheckResult::Free)).await;
        process(&mut engine, &store, &bus, &check("abc", CheckResult::Free)).await;
        assert!(rx.try_recv().is_err(), "must not re-notify after restart");
        assert_eq!(store.recent_events(10).await.unwrap().len(), 1);

        // Becoming taken afterwards is a real change and still works.
        process(&mut engine, &store, &bus, &check("abc", taken())).await;
        assert!(matches!(
            rx.try_recv().unwrap().event,
            Event::BecameTaken { .. }
        ));
    }

    #[tokio::test]
    async fn claim_success_is_persisted_and_not_renotified_after_restart() {
        let store = Store::open_in_memory().unwrap();
        let bus = event_bus();
        let mut rx = bus.subscribe();
        let mut engine = restore_engine(&store, 1).await.unwrap();
        process(&mut engine, &store, &bus, &check("abc", taken())).await;
        process(&mut engine, &store, &bus, &check("abc", CheckResult::Free)).await;
        rx.try_recv().unwrap(); // BecameFree
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(105);
        process_claim(
            &mut engine,
            &store,
            &bus,
            "abc",
            &ClaimOutcome::Succeeded,
            at,
        )
        .await;
        assert_eq!(
            rx.try_recv().unwrap().event,
            Event::ClaimSucceeded { code: "abc".into() }
        );
        assert_eq!(engine.state("abc"), VanityState::Claimed);

        // Restart: still claimed, free/taken results are ignored and nothing is published.
        let mut engine = restore_engine(&store, 1).await.unwrap();
        assert_eq!(engine.state("abc"), VanityState::Claimed);
        process(&mut engine, &store, &bus, &check("abc", CheckResult::Free)).await;
        process(&mut engine, &store, &bus, &check("abc", taken())).await;
        assert!(rx.try_recv().is_err());
        assert_eq!(store.recent_events(10).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn failed_claim_keeps_state_and_taken_afterwards_is_lost() {
        let store = Store::open_in_memory().unwrap();
        let bus = event_bus();
        let mut rx = bus.subscribe();
        let mut engine = restore_engine(&store, 1).await.unwrap();
        process(&mut engine, &store, &bus, &check("abc", taken())).await;
        process(&mut engine, &store, &bus, &check("abc", CheckResult::Free)).await;
        rx.try_recv().unwrap();
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(105);
        let failed = ClaimOutcome::Failed {
            reason: "Discord error 50035".into(),
        };
        process_claim(&mut engine, &store, &bus, "abc", &failed, at).await;
        assert!(matches!(
            rx.try_recv().unwrap().event,
            Event::ClaimFailed { .. }
        ));
        assert_eq!(engine.state("abc"), VanityState::Free);
        process(&mut engine, &store, &bus, &check("abc", taken())).await;
        assert_eq!(engine.state("abc"), VanityState::Lost);
        let states = store.load_states().await.unwrap();
        assert_eq!(states[0].state, VanityState::Lost);
    }

    #[tokio::test]
    async fn verification_and_invalid_token_publish_their_events() {
        let store = Store::open_in_memory().unwrap();
        let bus = event_bus();
        let mut rx = bus.subscribe();
        let mut engine = StateEngine::new(1);
        let at = SystemTime::UNIX_EPOCH;
        process_claim(
            &mut engine,
            &store,
            &bus,
            "abc",
            &ClaimOutcome::NeedsVerification,
            at,
        )
        .await;
        assert_eq!(
            rx.try_recv().unwrap().event,
            Event::ClaimNeedsVerification { code: "abc".into() }
        );
        process_claim(
            &mut engine,
            &store,
            &bus,
            "abc",
            &ClaimOutcome::InvalidToken,
            at,
        )
        .await;
        assert!(matches!(
            rx.try_recv().unwrap().event,
            Event::ClaimFailed { reason, .. } if reason.contains("invalid token")
        ));
    }

    #[tokio::test]
    async fn startup_taken_is_persisted_without_event() {
        let store = Store::open_in_memory().unwrap();
        let bus = event_bus();
        let mut rx = bus.subscribe();
        let mut engine = restore_engine(&store, 2).await.unwrap();
        process(&mut engine, &store, &bus, &check("abc", taken())).await;
        assert!(rx.try_recv().is_err());
        assert!(store.recent_events(10).await.unwrap().is_empty());
        let states = store.load_states().await.unwrap();
        assert!(matches!(states[0].state, VanityState::Taken { .. }));
    }

    #[tokio::test]
    async fn degraded_event_is_logged_in_the_event_table() {
        let store = Store::open_in_memory().unwrap();
        let bus = event_bus();
        let mut rx = bus.subscribe();
        let mut engine = StateEngine::with_degraded_threshold(2, 2);
        for _ in 0..2 {
            process(
                &mut engine,
                &store,
                &bus,
                &check("abc", CheckResult::Error("x".into())),
            )
            .await;
        }
        assert!(matches!(
            rx.try_recv().unwrap().event,
            Event::CheckerDegraded { .. }
        ));
        assert_eq!(store.recent_events(10).await.unwrap().len(), 1);
    }
}
