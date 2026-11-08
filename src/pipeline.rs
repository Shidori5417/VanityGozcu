//! Glue between the scheduler output, the state engine, the store and the event bus.

use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use crate::model::{CheckResult, EventRecord, VanityState};
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
    let before = engine.state(&check.code);
    let event = engine.apply(&check.code, &check.result, check.at);
    let after = engine.state(&check.code);

    log_result(&check.code, &check.result, &before, &after);

    let record = event.map(|event| EventRecord {
        event,
        at: check.at,
    });
    let persisted = if before != after {
        let updated_at = engine
            .record(&check.code)
            .map_or(check.at, |r| r.updated_at);
        store
            .save(
                &check.code,
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
        error!(code = %check.code, "failed to persist state: {e}");
    }
    if let Some(record) = record {
        info!(code = %check.code, kind = record.event.kind(), "event");
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
    notifiers: Notifiers,
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
