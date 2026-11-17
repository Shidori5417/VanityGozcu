//! SQLite persistence: last known state per code and an append-only event log.
//!
//! Blocking `rusqlite` calls run on `spawn_blocking`. No secrets are ever stored; only
//! codes, states and [`Event`]s.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, params};
use thiserror::Error;

use crate::model::{Event, VanityState};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS target_state (
    code       TEXT PRIMARY KEY,
    state      TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS events (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    code    TEXT NOT NULL,
    kind    TEXT NOT NULL,
    payload TEXT NOT NULL,
    at      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS events_code_idx ON events (code, id);
";

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("cannot create database directory {path}")]
    CreateDir {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("cannot (de)serialize stored value: {0}")]
    Json(#[from] serde_json::Error),
    #[error("database lock poisoned")]
    Poisoned,
    #[error("database task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

/// A persisted event row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredEvent {
    pub id: i64,
    pub event: Event,
    pub at: SystemTime,
}

/// A persisted state row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredState {
    pub code: String,
    pub state: VanityState,
    pub updated_at: SystemTime,
}

/// Event-log retention: delete rows older than `days` and keep at most
/// `max_events` rows. `0` disables the corresponding limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    pub days: u64,
    pub max_events: u64,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            days: 30,
            max_events: 50_000,
        }
    }
}

impl Retention {
    /// No limits; used by in-memory stores so tests keep their fixtures.
    pub const UNLIMITED: Self = Self {
        days: 0,
        max_events: 0,
    };

    fn enabled(self) -> bool {
        self.days > 0 || self.max_events > 0
    }
}

/// How many event writes between automatic retention passes.
const PRUNE_EVERY_WRITES: u64 = 256;

#[derive(Debug, Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
    retention: Retention,
    writes: Arc<AtomicU64>,
}

fn to_secs(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn from_secs(s: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(u64::try_from(s).unwrap_or(0))
}

/// Deletes events outside the retention window and returns how many rows went away.
fn prune(conn: &Connection, retention: Retention) -> Result<usize, StoreError> {
    let mut removed = 0;
    if retention.days > 0 {
        let max_age = i64::try_from(retention.days.saturating_mul(86_400)).unwrap_or(i64::MAX);
        let cutoff = to_secs(SystemTime::now()).saturating_sub(max_age);
        removed += conn.execute("DELETE FROM events WHERE at < ?1", [cutoff])?;
    }
    if retention.max_events > 0 {
        let keep = i64::try_from(retention.max_events).unwrap_or(i64::MAX);
        removed += conn.execute(
            "DELETE FROM events WHERE id NOT IN
             (SELECT id FROM events ORDER BY id DESC LIMIT ?1)",
            [keep],
        )?;
    }
    Ok(removed)
}

/// Counts an event write and runs retention every [`PRUNE_EVERY_WRITES`] calls.
fn maybe_prune(
    conn: &mut Connection,
    retention: Retention,
    writes: &AtomicU64,
) -> Result<(), StoreError> {
    if !retention.enabled() {
        return Ok(());
    }
    let n = writes.fetch_add(1, Ordering::Relaxed) + 1;
    if n.is_multiple_of(PRUNE_EVERY_WRITES) {
        prune(conn, retention)?;
    }
    Ok(())
}

impl Store {
    /// Opens (creating parent directories and tables as needed) the database at `path`.
    /// Uses [`Retention::UNLIMITED`]; the daemon calls [`Store::open_with_retention`].
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        Self::open_with_retention(path, Retention::UNLIMITED)
    }

    /// Opens the database and immediately applies `retention`.
    pub fn open_with_retention(path: &Path, retention: Retention) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|source| StoreError::CreateDir {
                path: parent.to_owned(),
                source,
            })?;
        }
        Self::init(Connection::open(path)?, retention)
    }

    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::init(Connection::open_in_memory()?, Retention::UNLIMITED)
    }

    fn init(conn: Connection, retention: Retention) -> Result<Self, StoreError> {
        conn.execute_batch(SCHEMA)?;
        prune(&conn, retention)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            retention,
            writes: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Applies the configured retention now (called on startup and periodically while
    /// writing). Returns the number of deleted event rows.
    pub async fn prune(&self) -> Result<usize, StoreError> {
        let retention = self.retention;
        self.blocking(move |conn| prune(conn, retention)).await
    }

    fn lock(conn: &Mutex<Connection>) -> Result<MutexGuard<'_, Connection>, StoreError> {
        conn.lock().map_err(|_| StoreError::Poisoned)
    }

    async fn blocking<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, StoreError> + Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = Self::lock(&conn)?;
            f(&mut guard)
        })
        .await?
    }

    pub async fn load_states(&self) -> Result<Vec<StoredState>, StoreError> {
        self.blocking(|conn| {
            let mut stmt = conn.prepare("SELECT code, state, updated_at FROM target_state")?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (code, state, updated_at) = row?;
                out.push(StoredState {
                    code,
                    state: serde_json::from_str(&state)?,
                    updated_at: from_secs(updated_at),
                });
            }
            Ok(out)
        })
        .await
    }

    /// Persists `state` and, in the same transaction, the optional `event`.
    pub async fn save(
        &self,
        code: &str,
        state: &VanityState,
        updated_at: SystemTime,
        event: Option<(&Event, SystemTime)>,
    ) -> Result<(), StoreError> {
        let code = code.to_owned();
        let state = serde_json::to_string(state)?;
        let has_event = event.is_some();
        let event = event
            .map(|(e, at)| -> Result<_, StoreError> {
                Ok((
                    e.code().to_owned(),
                    e.kind(),
                    serde_json::to_string(e)?,
                    to_secs(at),
                ))
            })
            .transpose()?;
        let retention = self.retention;
        let writes = Arc::clone(&self.writes);
        self.blocking(move |conn| {
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT INTO target_state (code, state, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(code) DO UPDATE SET state = excluded.state,
                                                 updated_at = excluded.updated_at",
                params![code, state, to_secs(updated_at)],
            )?;
            if let Some((ecode, kind, payload, at)) = event {
                tx.execute(
                    "INSERT INTO events (code, kind, payload, at) VALUES (?1, ?2, ?3, ?4)",
                    params![ecode, kind, payload, at],
                )?;
            }
            tx.commit()?;
            if has_event {
                maybe_prune(conn, retention, &writes)?;
            }
            Ok(())
        })
        .await
    }

    /// Appends an event without touching state (e.g. `CheckerDegraded`).
    pub async fn append_event(&self, event: &Event, at: SystemTime) -> Result<(), StoreError> {
        let code = event.code().to_owned();
        let kind = event.kind();
        let payload = serde_json::to_string(event)?;
        let retention = self.retention;
        let writes = Arc::clone(&self.writes);
        self.blocking(move |conn| {
            conn.execute(
                "INSERT INTO events (code, kind, payload, at) VALUES (?1, ?2, ?3, ?4)",
                params![code, kind, payload, to_secs(at)],
            )?;
            maybe_prune(conn, retention, &writes)?;
            Ok(())
        })
        .await
    }

    /// Most recent events first.
    pub async fn recent_events(&self, limit: u32) -> Result<Vec<StoredEvent>, StoreError> {
        self.blocking(move |conn| {
            let mut stmt =
                conn.prepare("SELECT id, payload, at FROM events ORDER BY id DESC LIMIT ?1")?;
            let rows = stmt.query_map([limit], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (id, payload, at) = row?;
                out.push(StoredEvent {
                    id,
                    event: serde_json::from_str(&payload)?,
                    at: from_secs(at),
                });
            }
            Ok(out)
        })
        .await
    }

    /// Call on shutdown: waits for in-flight writes (every `save` is already a durable
    /// transaction) and lets SQLite refresh its query statistics.
    pub async fn close(&self) -> Result<(), StoreError> {
        self.blocking(|conn| {
            conn.execute_batch("PRAGMA optimize")?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn t(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[tokio::test]
    async fn state_and_event_round_trip_through_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/dir/vanity.db");
        let event = Event::BecameFree {
            code: "abc".into(),
            previous_owner: Some("Old".into()),
        };
        {
            let store = Store::open(&path).unwrap();
            let state = VanityState::Taken {
                guild_id: Some("1".into()),
                guild_name: Some("Old".into()),
            };
            store.save("abc", &state, t(10), None).await.unwrap();
            store
                .save("abc", &VanityState::Free, t(20), Some((&event, t(20))))
                .await
                .unwrap();
            store.close().await.unwrap();
        }
        let store = Store::open(&path).unwrap();
        let states = store.load_states().await.unwrap();
        assert_eq!(
            states,
            vec![StoredState {
                code: "abc".into(),
                state: VanityState::Free,
                updated_at: t(20)
            }]
        );
        let events = store.recent_events(10).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, event);
        assert_eq!(events[0].at, t(20));
    }

    #[tokio::test]
    async fn append_event_and_ordering() {
        let store = Store::open_in_memory().unwrap();
        for i in 0..3 {
            store
                .append_event(
                    &Event::CheckerDegraded {
                        code: format!("c{i}"),
                        reason: "x".into(),
                    },
                    t(i),
                )
                .await
                .unwrap();
        }
        let events = store.recent_events(2).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event.code(), "c2");
        assert_eq!(events[1].event.code(), "c1");
    }

    #[tokio::test]
    async fn retention_removes_old_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("retention.db");
        let store = Store::open_with_retention(
            &path,
            Retention {
                days: 1,
                max_events: 0,
            },
        )
        .unwrap();
        let old = SystemTime::now() - Duration::from_secs(2 * 86_400);
        for (code, at) in [("old", old), ("new", SystemTime::now())] {
            store
                .append_event(
                    &Event::CheckerDegraded {
                        code: code.into(),
                        reason: "x".into(),
                    },
                    at,
                )
                .await
                .unwrap();
        }
        assert_eq!(store.prune().await.unwrap(), 1);
        let events = store.recent_events(10).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.code(), "new");
    }

    #[tokio::test]
    async fn retention_keeps_only_the_newest_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("retention-cap.db");
        let store = Store::open_with_retention(
            &path,
            Retention {
                days: 0,
                max_events: 3,
            },
        )
        .unwrap();
        for i in 0..5 {
            store
                .append_event(
                    &Event::CheckerDegraded {
                        code: format!("c{i}"),
                        reason: "x".into(),
                    },
                    t(u64::try_from(i).unwrap()),
                )
                .await
                .unwrap();
        }
        store.prune().await.unwrap();
        let events = store.recent_events(10).await.unwrap();
        let codes: Vec<_> = events.iter().map(|e| e.event.code()).collect();
        assert_eq!(codes, ["c4", "c3", "c2"]);
    }

    #[tokio::test]
    async fn suspected_state_survives_round_trip() {
        let store = Store::open_in_memory().unwrap();
        let s = VanityState::FreeSuspected {
            count: 1,
            previous_owner: None,
        };
        store.save("abc", &s, t(1), None).await.unwrap();
        assert_eq!(store.load_states().await.unwrap()[0].state, s);
    }
}
