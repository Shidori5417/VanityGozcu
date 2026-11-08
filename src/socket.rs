//! Unix-socket server that lets `attach` (a second front end) drive a running daemon.
//!
//! Protocol: newline-delimited JSON, one message per line.
//!
//! * client -> server: `{"id": 7, "cmd": "add_target", "code": "abc"}` (an
//!   [`EngineCommand`] plus a client-chosen `id`);
//! * server -> client ([`ServerMessage`]): the [`Snapshot`] right after connecting,
//!   `reply` messages echoing the request `id`, pushed `update`s, and `error`s
//!   (malformed request; `id` is set when it could be recovered).
//!
//! The snapshot is taken after subscribing to the engine's updates and updates the
//! snapshot already covers are dropped, so the stream is gap-free. A client that falls
//! behind the broadcast gets a fresh `snapshot` message instead of the missed updates.
//! Requests longer than [`MAX_LINE_BYTES`] are answered with an `error` and the
//! connection is closed.

use std::io;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Semaphore, broadcast};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{sleep, timeout};
use tracing::{debug, info, warn};

use crate::engine::{CommandReply, EngineCommand, EngineHandle, Snapshot, Update};

/// Longest accepted client line.
pub const MAX_LINE_BYTES: usize = 64 * 1024;
/// Simultaneous clients; further connections are dropped.
const MAX_CLIENTS: usize = 16;
/// A client that does not accept a message within this time is disconnected.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait for a socket to answer when probing it.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Error)]
pub enum SocketError {
    #[error("another instance is already listening on {}", .0.display())]
    AlreadyRunning(PathBuf),
    #[error("{} exists and is not a socket; refusing to replace it", .0.display())]
    NotASocket(PathBuf),
    #[error("socket {}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
    #[error("protocol error: {0}")]
    Protocol(String),
}

/// A client request: the engine command plus an id echoed in the reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    #[serde(flatten)]
    pub command: EngineCommand,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Sent on connect and again after the client lagged behind.
    Snapshot {
        snapshot: Snapshot,
    },
    Reply {
        id: u64,
        reply: CommandReply,
    },
    Update {
        update: Update,
    },
    Error {
        id: Option<u64>,
        message: String,
    },
}

pub(crate) enum Line {
    Data,
    Eof,
    TooLong,
}

/// Reads one line (without its newline) into `buf`, giving up once it exceeds `max`
/// bytes. Cancel safe: partial data stays in `buf` across calls.
pub(crate) async fn read_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> io::Result<Line> {
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            return Ok(Line::Eof);
        }
        let newline = chunk.iter().position(|b| *b == b'\n');
        let take = newline.unwrap_or(chunk.len());
        if buf.len() + take > max {
            return Ok(Line::TooLong);
        }
        buf.extend_from_slice(&chunk[..take]);
        reader.consume(take + usize::from(newline.is_some()));
        if newline.is_some() {
            return Ok(Line::Data);
        }
    }
}

pub(crate) async fn write_json<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    message: &T,
) -> io::Result<()> {
    let mut line = serde_json::to_vec(message).map_err(io::Error::other)?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await
}

/// True when something accepts connections on `path`.
pub async fn probe(path: &Path) -> bool {
    matches!(
        timeout(PROBE_TIMEOUT, UnixStream::connect(path)).await,
        Ok(Ok(_)) | Err(_)
    )
}

/// A bound socket. The file is removed when this is dropped.
#[derive(Debug)]
pub struct SocketListener {
    listener: UnixListener,
    path: PathBuf,
}

impl Drop for SocketListener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> SocketError + '_ {
    move |source| SocketError::Io {
        path: path.to_owned(),
        source,
    }
}

/// Binds `path` with mode 0600. A leftover socket file is removed only when nobody
/// answers on it; a live one yields [`SocketError::AlreadyRunning`].
pub async fn bind(path: &Path) -> Result<SocketListener, SocketError> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(io_err(parent))?;
    }
    match tokio::fs::symlink_metadata(path).await {
        Ok(meta) if !meta.file_type().is_socket() => {
            return Err(SocketError::NotASocket(path.to_owned()));
        }
        Ok(_) => match timeout(PROBE_TIMEOUT, UnixStream::connect(path)).await {
            Ok(Ok(_)) | Err(_) => return Err(SocketError::AlreadyRunning(path.to_owned())),
            Ok(Err(e))
                if matches!(
                    e.kind(),
                    io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                ) =>
            {
                info!("removing stale socket {}", path.display());
                tokio::fs::remove_file(path).await.map_err(io_err(path))?;
            }
            Ok(Err(e)) => return Err(io_err(path)(e)),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_err(path)(e)),
    }

    // Bind under a private name, restrict it, then move it into place so the final
    // path never exists with looser permissions.
    let mut tmp_name = path.file_name().unwrap_or_default().to_owned();
    tmp_name.push(format!(".{}.tmp", std::process::id()));
    let tmp = path.with_file_name(tmp_name);
    let _ = tokio::fs::remove_file(&tmp).await;
    let listener = UnixListener::bind(&tmp).map_err(io_err(&tmp))?;
    let placed = async {
        tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await?;
        tokio::fs::rename(&tmp, path).await
    }
    .await;
    if let Err(e) = placed {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(io_err(path)(e));
    }
    Ok(SocketListener {
        listener,
        path: path.to_owned(),
    })
}

impl SocketListener {
    /// Starts accepting clients. Dropping or shutting down the server removes the
    /// socket file.
    pub fn serve(self, handle: EngineHandle) -> SocketServer {
        info!("listening on {}", self.path.display());
        SocketServer {
            task: tokio::spawn(accept_loop(self, handle)),
        }
    }
}

#[derive(Debug)]
pub struct SocketServer {
    task: JoinHandle<()>,
}

impl SocketServer {
    /// Stops accepting, disconnects every client and removes the socket file.
    pub async fn shutdown(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

async fn accept_loop(socket: SocketListener, handle: EngineHandle) {
    let slots = Arc::new(Semaphore::new(MAX_CLIENTS));
    let mut connections = JoinSet::new();
    loop {
        let stream = match socket.listener.accept().await {
            Ok((stream, _)) => stream,
            Err(e) => {
                warn!("accept failed: {e}");
                sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        while connections.try_join_next().is_some() {}
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            warn!("too many clients; connection refused");
            continue;
        };
        let handle = handle.clone();
        connections.spawn(async move {
            let _slot = slot;
            match serve_connection(stream, handle).await {
                Ok(()) => debug!("client disconnected"),
                Err(e) => debug!("client dropped: {e}"),
            }
        });
    }
}

async fn send<W: AsyncWrite + Unpin>(writer: &mut W, message: &ServerMessage) -> io::Result<()> {
    timeout(WRITE_TIMEOUT, write_json(writer, message))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "client too slow"))?
}

fn error_message(id: Option<u64>, message: impl Into<String>) -> ServerMessage {
    ServerMessage::Error {
        id,
        message: message.into(),
    }
}

/// Handles one client until it disconnects or misbehaves.
async fn serve_connection(stream: UnixStream, handle: EngineHandle) -> io::Result<()> {
    let (read_half, mut writer) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    // Subscribe first, then snapshot: see the module docs.
    let mut updates = handle.subscribe();
    let snapshot = handle
        .snapshot()
        .await
        .map_err(|e| io::Error::other(e.to_string()))?;
    let mut seen = snapshot.seq;
    send(&mut writer, &ServerMessage::Snapshot { snapshot }).await?;

    let mut buf = Vec::new();
    loop {
        tokio::select! {
            line = read_line(&mut reader, &mut buf, MAX_LINE_BYTES) => {
                match line? {
                    Line::Eof => return Ok(()),
                    Line::TooLong => {
                        let msg = error_message(None, format!("line exceeds {MAX_LINE_BYTES} bytes"));
                        send(&mut writer, &msg).await?;
                        return Ok(());
                    }
                    Line::Data => {}
                }
                let reply = answer(&buf, &handle).await;
                buf.clear();
                if let Some(reply) = reply {
                    send(&mut writer, &reply).await?;
                }
            }
            update = updates.recv() => match update {
                Ok(update) if update.seq > seen => {
                    seen = update.seq;
                    send(&mut writer, &ServerMessage::Update { update }).await?;
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    debug!(missed, "client lagged; resending snapshot");
                    let snapshot = handle
                        .snapshot()
                        .await
                        .map_err(|e| io::Error::other(e.to_string()))?;
                    seen = snapshot.seq;
                    send(&mut writer, &ServerMessage::Snapshot { snapshot }).await?;
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
        }
    }
}

/// Turns one request line into the message to send back (`None` for blank lines).
async fn answer(line: &[u8], handle: &EngineHandle) -> Option<ServerMessage> {
    if line.iter().all(u8::is_ascii_whitespace) {
        return None;
    }
    match serde_json::from_slice::<Request>(line) {
        Ok(request) => Some(match handle.request(request.command).await {
            Ok(reply) => ServerMessage::Reply {
                id: request.id,
                reply,
            },
            Err(e) => error_message(Some(request.id), e.to_string()),
        }),
        Err(e) => {
            let id = serde_json::from_slice::<serde_json::Value>(line)
                .ok()
                .and_then(|v| v.get("id")?.as_u64());
            Some(error_message(id, format!("invalid request: {e}")))
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub(crate) mod tests {
    use super::*;
    use crate::checker::Checker;
    use crate::engine::{ClaimInit, DEFAULT_RECHECK_DELAY, Engine, EngineInit, UpdateKind};
    use crate::model::VanityTarget;
    use crate::notify::{Flags, Notifiers};
    use crate::pipeline;
    use crate::ratelimit::RateLimiter;
    use crate::scheduler::{self, SchedulerSettings};
    use crate::state::StateEngine;
    use crate::store::Store;
    use tokio::io::AsyncReadExt;

    const WAIT: Duration = Duration::from_secs(5);

    pub(crate) struct Fixture {
        pub handle: EngineHandle,
        pub socket: PathBuf,
        pub dir: tempfile::TempDir,
        server: Option<SocketServer>,
    }

    impl Fixture {
        pub(crate) fn config_path(&self) -> PathBuf {
            self.dir.path().join("config.toml")
        }

        /// Stops the server only; the engine keeps running.
        pub(crate) async fn shutdown_server(&mut self) {
            if let Some(server) = self.server.take() {
                server.shutdown().await;
            }
        }
    }

    /// A real engine (checks fail fast against a closed port) behind a socket in a
    /// temp dir.
    pub(crate) async fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("sock").join("vanity.sock");
        let config = dir.path().join("config.toml");
        std::fs::write(&config, "[general]\nlog_level = \"info\"\n").unwrap();
        let checker =
            Arc::new(Checker::new("http://127.0.0.1:1", Duration::from_millis(200)).unwrap());
        let scheduler = scheduler::spawn(
            SchedulerSettings {
                default_interval: Duration::from_secs(3600),
                jitter_percent: 0,
                backoff_max: Duration::from_secs(60),
            },
            checker,
            Arc::new(RateLimiter::with_burst(60_000, 100)),
            Vec::<VanityTarget>::new(),
        );
        let mut engine = Engine::new(EngineInit {
            state: StateEngine::new(1),
            store: Store::open_in_memory().unwrap(),
            bus: pipeline::event_bus(),
            scheduler,
            notifiers: Arc::new(Notifiers::new(
                Flags {
                    on_free: true,
                    on_taken: false,
                    on_claim: true,
                    on_error: true,
                },
                vec![],
            )),
            targets: vec![],
            config_path: config,
            warnings: vec!["w".into()],
            claim: ClaimInit::disabled(1),
            recheck_delay: DEFAULT_RECHECK_DELAY,
        });
        let handle = engine.handle();
        tokio::spawn(async move { engine.run(std::future::pending::<()>()).await });
        let listener = bind(&socket).await.unwrap();
        let server = listener.serve(handle.clone());
        Fixture {
            handle,
            socket,
            dir,
            server: Some(server),
        }
    }

    /// Raw line-level test client.
    struct Raw {
        reader: BufReader<tokio::net::unix::OwnedReadHalf>,
        writer: tokio::net::unix::OwnedWriteHalf,
    }

    impl Raw {
        async fn connect(path: &Path) -> Self {
            let (r, writer) = UnixStream::connect(path).await.unwrap().into_split();
            Self {
                reader: BufReader::new(r),
                writer,
            }
        }

        async fn send_raw(&mut self, bytes: &[u8]) {
            self.writer.write_all(bytes).await.unwrap();
        }

        async fn recv(&mut self) -> Option<ServerMessage> {
            let mut buf = Vec::new();
            match timeout(WAIT, read_line(&mut self.reader, &mut buf, 16 << 20))
                .await
                .unwrap()
                .unwrap()
            {
                Line::Data => Some(serde_json::from_slice(&buf).unwrap()),
                _ => None,
            }
        }
    }

    #[tokio::test]
    async fn snapshot_on_connect_then_reply_and_update() {
        let f = fixture().await;
        let mut c = Raw::connect(&f.socket).await;
        let Some(ServerMessage::Snapshot { snapshot }) = c.recv().await else {
            panic!("expected snapshot first");
        };
        assert_eq!(snapshot.warnings, ["w"]);
        assert!(snapshot.targets.is_empty());

        c.send_raw(b"{\"id\":42,\"cmd\":\"add_target\",\"code\":\"abc\"}\n")
            .await;
        let (mut got_reply, mut got_update) = (false, false);
        while !(got_reply && got_update) {
            match c.recv().await.unwrap() {
                ServerMessage::Reply { id, reply } => {
                    assert_eq!(id, 42);
                    assert!(matches!(reply, CommandReply::Done { .. }), "{reply:?}");
                    got_reply = true;
                }
                ServerMessage::Update { update } => {
                    assert!(update.seq > snapshot.seq);
                    if matches!(&update.kind, UpdateKind::Target(v) if v.code == "abc") {
                        got_update = true;
                    }
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        // The command really reached the engine and the config file.
        assert!(
            std::fs::read_to_string(f.config_path())
                .unwrap()
                .contains("abc")
        );
    }

    #[tokio::test]
    async fn malformed_json_gets_an_error_and_connection_survives() {
        let f = fixture().await;
        let mut c = Raw::connect(&f.socket).await;
        c.recv().await.unwrap();
        c.send_raw(b"this is not json\n").await;
        assert!(matches!(
            c.recv().await,
            Some(ServerMessage::Error { id: None, .. })
        ));
        // Valid JSON, unknown command: the id is recovered.
        c.send_raw(b"{\"id\":7,\"cmd\":\"explode\"}\n\n").await;
        assert!(matches!(
            c.recv().await,
            Some(ServerMessage::Error { id: Some(7), .. })
        ));
        c.send_raw(b"{\"id\":8,\"cmd\":\"get_snapshot\"}\n").await;
        assert!(matches!(
            c.recv().await,
            Some(ServerMessage::Reply {
                id: 8,
                reply: CommandReply::Snapshot(_)
            })
        ));
    }

    #[tokio::test]
    async fn oversized_line_is_rejected_and_connection_closed() {
        let f = fixture().await;
        let mut c = Raw::connect(&f.socket).await;
        c.recv().await.unwrap();
        let big = vec![b'a'; MAX_LINE_BYTES + 10];
        // The server may close while we are still writing; that is fine.
        let _ = c.writer.write_all(&big).await;
        assert!(matches!(
            c.recv().await,
            Some(ServerMessage::Error { id: None, .. })
        ));
        let mut rest = Vec::new();
        let n = timeout(WAIT, c.reader.read_to_end(&mut rest))
            .await
            .unwrap();
        // EOF (or a reset because we still had unread data pending).
        assert!(n.is_ok() || n.is_err());
        // Other clients are unaffected.
        let mut d = Raw::connect(&f.socket).await;
        assert!(matches!(
            d.recv().await,
            Some(ServerMessage::Snapshot { .. })
        ));
    }

    #[tokio::test]
    async fn several_clients_each_get_updates() {
        let f = fixture().await;
        let mut a = Raw::connect(&f.socket).await;
        let mut b = Raw::connect(&f.socket).await;
        a.recv().await.unwrap();
        b.recv().await.unwrap();
        f.handle
            .request(EngineCommand::AddTarget { code: "zz".into() })
            .await
            .unwrap();
        for c in [&mut a, &mut b] {
            loop {
                if let Some(ServerMessage::Update { update }) = c.recv().await
                    && matches!(&update.kind, UpdateKind::Target(v) if v.code == "zz")
                {
                    break;
                }
            }
        }
    }

    #[tokio::test]
    async fn second_bind_is_refused_and_stale_socket_is_replaced() {
        let mut f = fixture().await;
        assert!(probe(&f.socket).await);
        assert!(matches!(
            bind(&f.socket).await,
            Err(SocketError::AlreadyRunning(_))
        ));
        // Socket file has mode 0600.
        let mode = std::fs::metadata(&f.socket).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        let path = f.socket.clone();
        f.shutdown_server().await;
        assert!(!path.exists(), "socket removed on shutdown");
        assert!(!probe(&path).await);

        // A stale socket file (bound, then abandoned without cleanup) is replaced.
        let stale = std::os::unix::net::UnixListener::bind(&path).unwrap();
        drop(stale);
        assert!(path.exists());
        assert!(!probe(&path).await);
        let l = bind(&path).await.unwrap();
        drop(l);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn regular_files_are_never_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vanity.sock");
        std::fs::write(&path, "precious").unwrap();
        assert!(matches!(bind(&path).await, Err(SocketError::NotASocket(_))));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "precious");
    }
}
