//! What the TUI needs from the engine, in one small abstraction: an update stream, a
//! snapshot and request/reply. [`Client::Local`] talks to an in-process engine,
//! [`Client::Remote`] to a daemon over its Unix socket (see `socket.rs`).

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::timeout;
use tracing::debug;

use crate::engine::{CommandReply, EngineCommand, EngineError, EngineHandle, Snapshot, Update};
use crate::socket::{Line, Request, ServerMessage, SocketError, read_line, write_json};

/// Largest line accepted from the daemon (snapshots can be big).
const MAX_SERVER_LINE_BYTES: usize = 16 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// One item of the update stream as the UI sees it.
#[derive(Debug)]
pub enum FeedEvent {
    Update(Update),
    /// Updates were missed; the caller should fetch a new snapshot.
    Lagged(u64),
    /// The daemon sent a fresh snapshot instead of missed updates.
    Resync(Snapshot),
}

pub enum Feed {
    Local(broadcast::Receiver<Update>),
    Remote(mpsc::UnboundedReceiver<FeedEvent>),
}

impl Feed {
    /// Next event; `None` once the engine or the connection is gone.
    pub async fn recv(&mut self) -> Option<FeedEvent> {
        match self {
            Self::Local(rx) => match rx.recv().await {
                Ok(update) => Some(FeedEvent::Update(update)),
                Err(broadcast::error::RecvError::Lagged(n)) => Some(FeedEvent::Lagged(n)),
                Err(broadcast::error::RecvError::Closed) => None,
            },
            Self::Remote(rx) => rx.recv().await,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Client {
    Local(EngineHandle),
    Remote(RemoteClient),
}

impl Client {
    /// Subscribe before calling [`Client::snapshot`] for a gap-free view. A remote
    /// client has a single feed: later calls return an already closed one.
    pub fn subscribe(&self) -> Feed {
        match self {
            Self::Local(h) => Feed::Local(h.subscribe()),
            Self::Remote(r) => r.take_feed(),
        }
    }

    pub async fn snapshot(&self) -> Result<Snapshot, EngineError> {
        match self {
            Self::Local(h) => h.snapshot().await,
            Self::Remote(r) => r.snapshot().await,
        }
    }

    pub async fn request(&self, command: EngineCommand) -> Result<CommandReply, EngineError> {
        match self {
            Self::Local(h) => h.request(command).await,
            Self::Remote(r) => r.request(command).await,
        }
    }
}

type Pending = Mutex<Option<HashMap<u64, oneshot::Sender<CommandReply>>>>;

#[derive(Debug)]
struct Inner {
    writer: tokio::sync::Mutex<OwnedWriteHalf>,
    /// `None` once the connection is gone.
    pending: Pending,
    next_id: AtomicU64,
    /// The snapshot sent on connect, until first used.
    initial: Mutex<Option<Snapshot>>,
    feed: Mutex<Option<mpsc::UnboundedReceiver<FeedEvent>>>,
}

#[derive(Debug, Clone)]
pub struct RemoteClient {
    inner: Arc<Inner>,
}

fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl RemoteClient {
    /// Connects and waits for the initial snapshot.
    pub async fn connect(path: &Path) -> Result<Self, SocketError> {
        let io = |source| SocketError::Io {
            path: path.to_owned(),
            source,
        };
        let stream = timeout(CONNECT_TIMEOUT, UnixStream::connect(path))
            .await
            .map_err(|_| SocketError::Protocol("connect timed out".into()))?
            .map_err(io)?;
        let (read_half, writer) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let mut buf = Vec::new();
        let first = timeout(CONNECT_TIMEOUT, next_message(&mut reader, &mut buf))
            .await
            .map_err(|_| SocketError::Protocol("no snapshot from the daemon".into()))??;
        let Some(ServerMessage::Snapshot { snapshot }) = first else {
            return Err(SocketError::Protocol(
                "the daemon did not start with a snapshot".into(),
            ));
        };
        let (feed_tx, feed_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Inner {
            writer: tokio::sync::Mutex::new(writer),
            pending: Mutex::new(Some(HashMap::new())),
            next_id: AtomicU64::new(1),
            initial: Mutex::new(Some(snapshot)),
            feed: Mutex::new(Some(feed_rx)),
        });
        tokio::spawn(read_loop(reader, buf, inner.clone(), feed_tx));
        Ok(Self { inner })
    }

    fn take_feed(&self) -> Feed {
        let rx = locked(&self.inner.feed)
            .take()
            .unwrap_or_else(|| mpsc::unbounded_channel().1);
        Feed::Remote(rx)
    }

    async fn snapshot(&self) -> Result<Snapshot, EngineError> {
        if let Some(s) = locked(&self.inner.initial).take() {
            return Ok(s);
        }
        match self.request(EngineCommand::GetSnapshot).await? {
            CommandReply::Snapshot(s) => Ok(s),
            _ => Err(EngineError::Closed),
        }
    }

    async fn request(&self, command: EngineCommand) -> Result<CommandReply, EngineError> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        match locked(&self.inner.pending).as_mut() {
            Some(pending) => pending.insert(id, tx),
            None => return Err(EngineError::Closed),
        };
        let sent = {
            let mut writer = self.inner.writer.lock().await;
            write_json(&mut *writer, &Request { id, command }).await
        };
        if sent.is_err() {
            if let Some(pending) = locked(&self.inner.pending).as_mut() {
                pending.remove(&id);
            }
            return Err(EngineError::Closed);
        }
        rx.await.map_err(|_| EngineError::Closed)
    }
}

async fn next_message<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> Result<Option<ServerMessage>, SocketError> {
    loop {
        buf.clear();
        match read_line(reader, buf, MAX_SERVER_LINE_BYTES)
            .await
            .map_err(|source| SocketError::Io {
                path: "<connection>".into(),
                source,
            })? {
            Line::Eof => return Ok(None),
            Line::TooLong => return Err(SocketError::Protocol("server line too long".into())),
            Line::Data if buf.iter().all(u8::is_ascii_whitespace) => {}
            Line::Data => {
                return serde_json::from_slice(buf)
                    .map(Some)
                    .map_err(|e| SocketError::Protocol(e.to_string()));
            }
        }
    }
}

/// Routes server messages until the connection ends, then fails every waiting request.
async fn read_loop<R: tokio::io::AsyncBufRead + Unpin>(
    mut reader: R,
    mut buf: Vec<u8>,
    inner: Arc<Inner>,
    feed: mpsc::UnboundedSender<FeedEvent>,
) {
    loop {
        let message = match next_message(&mut reader, &mut buf).await {
            Ok(Some(m)) => m,
            Ok(None) => break,
            Err(e) => {
                debug!("connection to the daemon ended: {e}");
                break;
            }
        };
        let reply_to = |id: u64, reply: CommandReply| {
            let waiter = locked(&inner.pending)
                .as_mut()
                .and_then(|pending| pending.remove(&id));
            if let Some(waiter) = waiter {
                let _ = waiter.send(reply);
            }
        };
        match message {
            ServerMessage::Reply { id, reply } => reply_to(id, reply),
            ServerMessage::Error {
                id: Some(id),
                message,
            } => reply_to(id, CommandReply::Rejected { reason: message }),
            ServerMessage::Error { id: None, message } => {
                debug!("daemon reported an error: {message}");
            }
            ServerMessage::Update { update } => {
                let _ = feed.send(FeedEvent::Update(update));
            }
            ServerMessage::Snapshot { snapshot } => {
                let _ = feed.send(FeedEvent::Resync(snapshot));
            }
        }
    }
    // Dropping the senders fails the waiting requests and closes the feed.
    locked(&inner.pending).take();
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::engine::UpdateKind;
    use crate::socket::tests::fixture;

    const WAIT: Duration = Duration::from_secs(5);

    async fn recv(feed: &mut Feed) -> FeedEvent {
        timeout(WAIT, feed.recv()).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn remote_client_round_trip_matches_local_semantics() {
        let f = fixture().await;
        let client = Client::Remote(RemoteClient::connect(&f.socket).await.unwrap());
        let mut feed = client.subscribe();
        let snapshot = client.snapshot().await.unwrap();
        assert!(snapshot.targets.is_empty());

        let reply = timeout(
            WAIT,
            client.request(EngineCommand::AddTarget { code: "abc".into() }),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(matches!(reply, CommandReply::Done { .. }), "{reply:?}");
        loop {
            if let FeedEvent::Update(u) = recv(&mut feed).await
                && matches!(&u.kind, UpdateKind::Target(v) if v.code == "abc")
            {
                assert!(u.seq > snapshot.seq);
                break;
            }
        }

        // Later snapshots come from the server and reflect the change.
        let fresh = client.snapshot().await.unwrap();
        assert_eq!(fresh.targets.len(), 1);

        // Rejections travel as replies too.
        let reply = client
            .request(EngineCommand::RemoveTarget {
                code: "nope".into(),
            })
            .await
            .unwrap();
        assert!(matches!(reply, CommandReply::Rejected { .. }), "{reply:?}");
    }

    #[tokio::test]
    async fn requests_fail_and_feed_closes_when_the_daemon_goes_away() {
        let mut f = fixture().await;
        let remote = RemoteClient::connect(&f.socket).await.unwrap();
        let client = Client::Remote(remote);
        let mut feed = client.subscribe();
        client.snapshot().await.unwrap();
        f.shutdown_server().await;
        assert!(timeout(WAIT, feed.recv()).await.unwrap().is_none());
        assert!(client.request(EngineCommand::TestNotify).await.is_err());
    }

    #[tokio::test]
    async fn local_client_wraps_the_engine_handle() {
        let f = fixture().await;
        let client = Client::Local(f.handle.clone());
        let mut feed = client.subscribe();
        let snapshot = client.snapshot().await.unwrap();
        client
            .request(EngineCommand::AddTarget { code: "xyz".into() })
            .await
            .unwrap();
        let FeedEvent::Update(u) = recv(&mut feed).await else {
            panic!("expected update");
        };
        assert!(u.seq > snapshot.seq);
    }
}
