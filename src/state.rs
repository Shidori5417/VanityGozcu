//! Pure, synchronous state machine.
//!
//! ```text
//! Unknown -> Taken -> FreeSuspected -> Free -> Claimed / Lost
//!               ^          |             |
//!               +----------+             +-> Taken (someone else took it)
//! ```
//!
//! Notification rules (the "no re-notify on restart" guarantee):
//! * The first observation `Unknown -> Taken` emits nothing.
//! * `BecameFree` is emitted exactly when a code *enters* confirmed `Free` from any
//!   state other than `Free`. A persisted `Free` that is still free after a restart
//!   emits nothing, because the engine is seeded from the store.
//! * `BecameTaken` is emitted only for `Free -> Taken`.
//! * A claim that succeeded makes the code `Claimed` ([`StateEngine::mark_claimed`]);
//!   results are then ignored. After a *failed* claim ([`StateEngine::mark_claim_failed`])
//!   a `Free -> Taken` flip means someone else got it and the state becomes `Lost`.
//! * Delivery is at-most-once: state is persisted before the event is broadcast.

use std::collections::HashMap;
use std::time::SystemTime;

use crate::model::{CheckResult, Event, VanityState};

/// Consecutive errors after which `CheckerDegraded` is emitted (once per streak).
pub const DEFAULT_DEGRADED_THRESHOLD: u32 = 5;

/// Current state of a code plus when it last changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub state: VanityState,
    pub updated_at: SystemTime,
}

#[derive(Debug, Default)]
struct Entry {
    state: VanityState,
    updated_at: Option<SystemTime>,
    consecutive_errors: u32,
    degraded_reported: bool,
    /// Our claim attempt for the current `Free` period failed.
    claim_failed: bool,
}

#[derive(Debug)]
pub struct StateEngine {
    confirm_checks: u32,
    degraded_threshold: u32,
    entries: HashMap<String, Entry>,
}

impl StateEngine {
    pub fn new(confirm_checks: u32) -> Self {
        Self::with_degraded_threshold(confirm_checks, DEFAULT_DEGRADED_THRESHOLD)
    }

    /// Changes the number of consecutive free results that confirm `Free` (min 1).
    pub fn set_confirm_checks(&mut self, confirm_checks: u32) {
        self.confirm_checks = confirm_checks.max(1);
    }

    /// Records that our claim of `code` succeeded.
    pub fn mark_claimed(&mut self, code: &str, now: SystemTime) {
        let entry = self.entries.entry(code.to_owned()).or_default();
        entry.state = VanityState::Claimed;
        entry.updated_at = Some(now);
        entry.claim_failed = false;
    }

    /// Records that our claim of `code` failed, so a later `Taken` means it is lost.
    pub fn mark_claim_failed(&mut self, code: &str) {
        if let Some(entry) = self.entries.get_mut(code) {
            entry.claim_failed = true;
        }
    }

    pub fn with_degraded_threshold(confirm_checks: u32, degraded_threshold: u32) -> Self {
        Self {
            confirm_checks: confirm_checks.max(1),
            degraded_threshold: degraded_threshold.max(1),
            entries: HashMap::new(),
        }
    }

    /// Seeds persisted state (call before the first `apply`).
    pub fn restore(&mut self, code: &str, state: VanityState, updated_at: SystemTime) {
        self.entries.insert(
            code.to_owned(),
            Entry {
                state,
                updated_at: Some(updated_at),
                ..Entry::default()
            },
        );
    }

    pub fn state(&self, code: &str) -> VanityState {
        self.entries
            .get(code)
            .map(|e| e.state.clone())
            .unwrap_or_default()
    }

    pub fn record(&self, code: &str) -> Option<Record> {
        let entry = self.entries.get(code)?;
        Some(Record {
            state: entry.state.clone(),
            updated_at: entry.updated_at?,
        })
    }

    /// Feeds one check result; returns the event to publish, if any.
    pub fn apply(&mut self, code: &str, result: &CheckResult, now: SystemTime) -> Option<Event> {
        let confirm = self.confirm_checks;
        let threshold = self.degraded_threshold;
        let entry = self.entries.entry(code.to_owned()).or_default();

        match result {
            CheckResult::RateLimited { .. } => None,
            CheckResult::Error(_) | CheckResult::Unknown { .. } => {
                entry.consecutive_errors = entry.consecutive_errors.saturating_add(1);
                if entry.consecutive_errors >= threshold && !entry.degraded_reported {
                    entry.degraded_reported = true;
                    return Some(Event::CheckerDegraded {
                        code: code.to_owned(),
                        reason: format!(
                            "{} consecutive failures, last: {result}",
                            entry.consecutive_errors
                        ),
                    });
                }
                None
            }
            CheckResult::Taken {
                guild_id,
                guild_name,
            } => {
                entry.consecutive_errors = 0;
                entry.degraded_reported = false;
                let new = VanityState::Taken {
                    guild_id: guild_id.clone(),
                    guild_name: guild_name.clone(),
                };
                // Claimed is owned by the claimer; our own guild is expected to hold it.
                if matches!(entry.state, VanityState::Claimed) {
                    return None;
                }
                let was_free = entry.state == VanityState::Free;
                let claim_failed = std::mem::take(&mut entry.claim_failed);
                if was_free && claim_failed {
                    // We tried and failed, and now someone else holds it.
                    entry.state = VanityState::Lost;
                    entry.updated_at = Some(now);
                    return Some(Event::BecameTaken {
                        code: code.to_owned(),
                        guild_name: guild_name.clone(),
                    });
                }
                if entry.state == VanityState::Lost {
                    return None;
                }
                if entry.state != new {
                    entry.state = new;
                    entry.updated_at = Some(now);
                }
                was_free.then(|| Event::BecameTaken {
                    code: code.to_owned(),
                    guild_name: guild_name.clone(),
                })
            }
            CheckResult::Free => {
                entry.consecutive_errors = 0;
                entry.degraded_reported = false;
                let (count, previous_owner) = match &entry.state {
                    VanityState::Free | VanityState::Claimed => return None,
                    VanityState::FreeSuspected {
                        count,
                        previous_owner,
                    } => (count.saturating_add(1), previous_owner.clone()),
                    VanityState::Taken { guild_name, .. } => (1, guild_name.clone()),
                    VanityState::Unknown | VanityState::Lost => (1, None),
                };
                entry.updated_at = Some(now);
                if count >= confirm {
                    entry.claim_failed = false;
                    entry.state = VanityState::Free;
                    Some(Event::BecameFree {
                        code: code.to_owned(),
                        previous_owner,
                    })
                } else {
                    entry.state = VanityState::FreeSuspected {
                        count,
                        previous_owner,
                    };
                    None
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn taken(name: &str) -> CheckResult {
        CheckResult::Taken {
            guild_id: Some("1".into()),
            guild_name: Some(name.into()),
        }
    }

    fn err() -> CheckResult {
        CheckResult::Error("boom".into())
    }

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000)
    }

    #[test]
    fn startup_taken_emits_nothing() {
        let mut e = StateEngine::new(2);
        assert_eq!(e.apply("abc", &taken("G"), now()), None);
        assert_eq!(e.apply("abc", &taken("G"), now()), None);
        assert!(matches!(e.state("abc"), VanityState::Taken { .. }));
    }

    #[test]
    fn confirm_checks_consecutive_free_confirms() {
        let mut e = StateEngine::new(2);
        e.apply("abc", &taken("Old"), now());
        assert_eq!(e.apply("abc", &CheckResult::Free, now()), None);
        assert_eq!(
            e.state("abc"),
            VanityState::FreeSuspected {
                count: 1,
                previous_owner: Some("Old".into())
            }
        );
        assert_eq!(
            e.apply("abc", &CheckResult::Free, now()),
            Some(Event::BecameFree {
                code: "abc".into(),
                previous_owner: Some("Old".into())
            })
        );
        assert_eq!(e.state("abc"), VanityState::Free);
        // Still free: no repeat.
        assert_eq!(e.apply("abc", &CheckResult::Free, now()), None);
    }

    #[test]
    fn taken_in_between_resets_confirmation() {
        let mut e = StateEngine::new(2);
        e.apply("abc", &taken("G"), now());
        assert_eq!(e.apply("abc", &CheckResult::Free, now()), None);
        assert_eq!(e.apply("abc", &taken("G"), now()), None);
        assert!(matches!(e.state("abc"), VanityState::Taken { .. }));
        assert_eq!(e.apply("abc", &CheckResult::Free, now()), None);
        assert!(matches!(
            e.apply("abc", &CheckResult::Free, now()),
            Some(Event::BecameFree { .. })
        ));
    }

    #[test]
    fn single_confirm_is_immediate() {
        let mut e = StateEngine::new(1);
        assert!(matches!(
            e.apply("abc", &CheckResult::Free, now()),
            Some(Event::BecameFree {
                previous_owner: None,
                ..
            })
        ));
    }

    #[test]
    fn unknown_to_free_emits_with_no_previous_owner() {
        let mut e = StateEngine::new(2);
        e.apply("abc", &CheckResult::Free, now());
        assert_eq!(
            e.apply("abc", &CheckResult::Free, now()),
            Some(Event::BecameFree {
                code: "abc".into(),
                previous_owner: None
            })
        );
    }

    #[test]
    fn free_to_taken_emits_became_taken() {
        let mut e = StateEngine::new(1);
        e.apply("abc", &CheckResult::Free, now());
        assert_eq!(
            e.apply("abc", &taken("New"), now()),
            Some(Event::BecameTaken {
                code: "abc".into(),
                guild_name: Some("New".into())
            })
        );
        assert_eq!(e.apply("abc", &taken("New"), now()), None);
    }

    #[test]
    fn suspected_then_taken_emits_nothing() {
        let mut e = StateEngine::new(3);
        e.apply("abc", &taken("G"), now());
        e.apply("abc", &CheckResult::Free, now());
        assert_eq!(e.apply("abc", &taken("G"), now()), None);
    }

    #[test]
    fn rate_limit_error_unknown_do_not_change_state() {
        let mut e = StateEngine::new(2);
        e.apply("abc", &taken("G"), now());
        e.apply("abc", &CheckResult::Free, now());
        let before = e.state("abc");
        let rl = CheckResult::RateLimited {
            retry_after: Duration::from_secs(1),
            global: false,
        };
        assert_eq!(e.apply("abc", &rl, now()), None);
        assert_eq!(e.apply("abc", &err(), now()), None);
        assert_eq!(
            e.apply("abc", &CheckResult::Unknown { status: 418 }, now()),
            None
        );
        assert_eq!(e.state("abc"), before);
    }

    #[test]
    fn degraded_emitted_once_and_reset_on_success() {
        let mut e = StateEngine::with_degraded_threshold(2, 3);
        assert_eq!(e.apply("abc", &err(), now()), None);
        assert_eq!(e.apply("abc", &err(), now()), None);
        assert!(matches!(
            e.apply("abc", &err(), now()),
            Some(Event::CheckerDegraded { .. })
        ));
        assert_eq!(e.apply("abc", &err(), now()), None);
        assert_eq!(e.apply("abc", &err(), now()), None);
        e.apply("abc", &taken("G"), now());
        assert_eq!(e.apply("abc", &err(), now()), None);
        assert_eq!(e.apply("abc", &err(), now()), None);
        assert!(matches!(
            e.apply("abc", &err(), now()),
            Some(Event::CheckerDegraded { .. })
        ));
    }

    #[test]
    fn rate_limit_between_errors_does_not_reset_streak() {
        let mut e = StateEngine::with_degraded_threshold(2, 2);
        let rl = CheckResult::RateLimited {
            retry_after: Duration::from_secs(1),
            global: true,
        };
        e.apply("abc", &err(), now());
        e.apply("abc", &rl, now());
        assert!(e.apply("abc", &err(), now()).is_some());
    }

    #[test]
    fn restart_with_persisted_free_does_not_renotify() {
        let mut e = StateEngine::new(2);
        e.restore("abc", VanityState::Free, now());
        assert_eq!(e.apply("abc", &CheckResult::Free, now()), None);
        assert_eq!(e.apply("abc", &CheckResult::Free, now()), None);
        assert_eq!(e.state("abc"), VanityState::Free);
    }

    #[test]
    fn restart_with_persisted_taken_notifies_when_it_frees() {
        let mut e = StateEngine::new(1);
        e.restore(
            "abc",
            VanityState::Taken {
                guild_id: None,
                guild_name: Some("G".into()),
            },
            now(),
        );
        assert_eq!(
            e.apply("abc", &CheckResult::Free, now()),
            Some(Event::BecameFree {
                code: "abc".into(),
                previous_owner: Some("G".into())
            })
        );
    }

    #[test]
    fn claimed_ignores_results_and_lost_can_refree() {
        let mut e = StateEngine::new(1);
        e.restore("abc", VanityState::Claimed, now());
        assert_eq!(e.apply("abc", &taken("Us"), now()), None);
        assert_eq!(e.apply("abc", &CheckResult::Free, now()), None);
        assert_eq!(e.state("abc"), VanityState::Claimed);
        e.restore("xyz", VanityState::Lost, now());
        assert!(matches!(
            e.apply("xyz", &CheckResult::Free, now()),
            Some(Event::BecameFree { .. })
        ));
    }

    #[test]
    fn failed_claim_then_taken_is_lost_and_stays_lost() {
        let mut e = StateEngine::new(1);
        e.apply("abc", &taken("Old"), now());
        assert!(e.apply("abc", &CheckResult::Free, now()).is_some());
        e.mark_claim_failed("abc");
        assert!(matches!(
            e.apply("abc", &taken("Other"), now()),
            Some(Event::BecameTaken { .. })
        ));
        assert_eq!(e.state("abc"), VanityState::Lost);
        assert_eq!(e.apply("abc", &taken("Other"), now()), None);
        assert_eq!(e.state("abc"), VanityState::Lost);
        // Free again later: a fresh BecameFree.
        assert!(matches!(
            e.apply("abc", &CheckResult::Free, now()),
            Some(Event::BecameFree { .. })
        ));
    }

    #[test]
    fn without_a_failed_claim_taken_is_plain_taken() {
        let mut e = StateEngine::new(1);
        e.apply("abc", &CheckResult::Free, now());
        e.apply("abc", &taken("Other"), now());
        assert!(matches!(e.state("abc"), VanityState::Taken { .. }));
    }

    #[test]
    fn mark_claimed_sticks_and_confirm_checks_can_change() {
        let mut e = StateEngine::new(3);
        e.apply("abc", &CheckResult::Free, now());
        e.mark_claimed("abc", now());
        assert_eq!(e.state("abc"), VanityState::Claimed);
        assert_eq!(e.apply("abc", &taken("Us"), now()), None);
        assert_eq!(e.apply("abc", &CheckResult::Free, now()), None);
        e.set_confirm_checks(1);
        assert!(matches!(
            e.apply("new", &CheckResult::Free, now()),
            Some(Event::BecameFree { .. })
        ));
    }

    #[test]
    fn record_tracks_update_time() {
        let mut e = StateEngine::new(1);
        assert_eq!(e.record("abc"), None);
        e.apply("abc", &taken("G"), now());
        assert_eq!(e.record("abc").unwrap().updated_at, now());
    }
}
