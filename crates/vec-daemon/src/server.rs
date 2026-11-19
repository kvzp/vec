//! The daemon side: listening socket, connection threads, request handling.

use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use vec_core::config::DaemonConfig;
use vec_embed::{Embedder, ModelIdentity};

use crate::activation;
use crate::deadline::DeadlineStream;
use crate::protocol::{
    self, FIELD_LEN, HELLO_LEN, STATUS_ERROR, STATUS_OK, encode_reply, encode_vectors_reply,
    read_u32, read_u32_or_eof,
};

/// Message sent to a client when embedding fails. The details (which may name
/// internal paths) go to the daemon's log only.
const EMBED_FAILED: &str = "embedding failed (details in the daemon log)";

/// Serve embeddings until idle for `[daemon] idle_exit_secs` (never when 0).
///
/// Uses the socket passed by systemd socket activation when there is one
/// (`LISTEN_PID`/`LISTEN_FDS`; its permissions come from the .socket unit).
/// Otherwise binds `[daemon] socket` with `[daemon] socket_mode`, after
/// removing a stale socket left by a crash; it refuses to replace a socket
/// another daemon still listens on, or anything that is not a socket. A
/// socket it bound itself is removed when it returns.
///
/// The model file is hashed before the first connection is accepted (the
/// handshake reply carries the hash): with socket activation, the client
/// that started the daemon waits for this, bounded by its `[daemon]
/// reply_timeout_ms`.
///
/// Each connection gets its own thread, at most `[daemon] max_connections`
/// at once: a client arriving while all are in use gets an immediate "busy"
/// error reply (it embeds in-process instead of waiting). `[daemon]
/// io_timeout_ms` is a deadline for each whole request (from the moment the
/// daemon waits for it to its last byte) and each whole reply write, so a
/// client cannot hold a connection by trickling bytes or reading slowly.
/// These bound only waits on the client; computing a reply is never cut
/// short, however long it takes. A request may carry at most `[daemon]
/// max_texts_per_request` texts in at most `[daemon] max_request_bytes`.
///
/// # Errors
///
/// When the model identity cannot be computed, the socket cannot be set up,
/// or waiting for connections fails.
pub fn run_daemon(embedder: Embedder, cfg: &DaemonConfig) -> Result<()> {
    // Eager: hash the model before accepting (see `[daemon] idle_exit_secs`
    // in defaults.toml for the trade-off).
    let cache = vec_core::model_sha::ShaCache::for_user()?;
    let identity = vec_core::model_sha::writer_identity(&embedder, cache.as_ref())
        .context("computing the model identity")?;
    let ctx = Arc::new(Ctx::new(embedder, &identity, cfg)?);
    let (listener, bound) = match activation::activated_listener()? {
        Some(l) => (l, None),
        None => {
            let l = bind(&cfg.socket, cfg.socket_mode.bits())?;
            (l, Some(BoundSocket(cfg.socket.clone())))
        }
    };
    anstream::eprintln!(
        "vec daemon ready  model={identity}  socket={}  activated={}  pid={}",
        cfg.socket.display(),
        bound.is_none(),
        std::process::id(),
    );
    serve(&listener, &ctx, &Limits::from_config(cfg))
}

/// A socket file this process created; removed on drop.
struct BoundSocket(PathBuf);

impl Drop for BoundSocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Bind `path`, replacing only a stale socket.
fn bind(path: &Path, mode: u32) -> Result<UnixListener> {
    remove_stale_socket(path)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating socket directory {}", parent.display()))?;
    }
    let listener = UnixListener::bind(path)
        .with_context(|| format!("binding Unix socket at {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("setting permissions {mode:04o} on {}", path.display()))?;
    Ok(listener)
}

/// Remove `path` if it is a socket nobody listens on. Anything else at
/// `path` is an error: a live socket belongs to a running daemon, and a
/// non-socket is probably a misconfigured `[daemon] socket`.
fn remove_stale_socket(path: &Path) -> Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("inspecting {}", path.display())),
    };
    if !meta.file_type().is_socket() {
        bail!(
            "{} exists and is not a socket; refusing to replace it (check [daemon] socket)",
            path.display()
        );
    }
    match UnixStream::connect(path) {
        Ok(_) => bail!("another daemon is already listening on {}", path.display()),
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => std::fs::remove_file(path)
            .with_context(|| format!("removing the stale socket {}", path.display())),
        Err(e) => Err(e).with_context(|| format!("probing the existing socket {}", path.display())),
    }
}

/// Accept-loop settings from `[daemon]`.
struct Limits {
    max_connections: usize,
    idle_exit: Option<Duration>,
    accept_retry: Duration,
}

impl Limits {
    fn from_config(cfg: &DaemonConfig) -> Self {
        Self {
            max_connections: cfg.max_connections,
            idle_exit: (cfg.idle_exit_secs > 0).then(|| Duration::from_secs(cfg.idle_exit_secs)),
            accept_retry: Duration::from_millis(cfg.accept_retry_ms),
        }
    }
}

fn serve(listener: &UnixListener, ctx: &Arc<Ctx>, limits: &Limits) -> Result<()> {
    // Non-blocking, so a client that disconnects between poll() and accept()
    // cannot block the loop.
    listener
        .set_nonblocking(true)
        .context("making the listening socket non-blocking")?;
    let gate = Arc::new(Gate::new(limits.max_connections));
    loop {
        let timeout = match limits.idle_exit {
            None => None,
            Some(limit) => match gate.idle_for() {
                Some(idle) if idle >= limit => {
                    anstream::eprintln!(
                        "vec daemon: no request for {} s, exiting",
                        limit.as_secs()
                    );
                    return Ok(());
                }
                Some(idle) => Some(limit - idle),
                // Connections are open: check again later.
                None => Some(limit),
            },
        };
        if !wait_readable(listener, timeout)? {
            continue;
        }
        match listener.accept() {
            Ok((stream, _)) => match Slot::try_take(&gate) {
                Some(slot) => spawn_connection(stream, ctx, slot),
                None => reject_busy(&stream, limits.max_connections),
            },
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::Interrupted
                        | io::ErrorKind::ConnectionAborted
                ) => {}
            Err(e) => {
                // E.g. EMFILE: pause instead of spinning on the error.
                anstream::eprintln!(
                    "vec daemon: accept failed: {e}; retrying in {} ms",
                    limits.accept_retry.as_millis()
                );
                std::thread::sleep(limits.accept_retry);
            }
        }
    }
}

/// Wait until a connection is pending; `false` on timeout or signal.
fn wait_readable(listener: &UnixListener, timeout: Option<Duration>) -> Result<bool> {
    let timeout = timeout.map_or(PollTimeout::NONE, |d| {
        PollTimeout::try_from(d).unwrap_or(PollTimeout::MAX)
    });
    let mut fds = [PollFd::new(listener.as_fd(), PollFlags::POLLIN)];
    match poll(&mut fds, timeout) {
        Ok(n) => Ok(n > 0),
        Err(Errno::EINTR) => Ok(false),
        Err(e) => Err(e).context("waiting for connections"),
    }
}

/// Tell a client that all connections are in use, without blocking the
/// accept loop, and close the connection.
fn reject_busy(stream: &UnixStream, max_connections: usize) {
    let msg = format!(
        "busy: all {max_connections} connections ([daemon] max_connections) are in use; \
         embed in-process"
    );
    // Best effort, never blocking: a fresh socket has room for this reply.
    // The client's hello is read first, so closing with it unread cannot
    // reset the connection before the client has read the reply.
    if stream.set_nonblocking(true).is_ok() {
        let mut hello = [0u8; HELLO_LEN];
        let _ = (&*stream).read(&mut hello);
        let _ = (&*stream).write_all(&encode_reply(STATUS_ERROR, msg.as_bytes()));
    }
}

fn spawn_connection(stream: UnixStream, ctx: &Arc<Ctx>, slot: Slot) {
    let ctx = Arc::clone(ctx);
    let spawned = std::thread::Builder::new()
        .name("vec-daemon-conn".into())
        .spawn(move || {
            let _slot = slot;
            if let Err(e) = handle_connection(stream, &ctx) {
                anstream::eprintln!("vec daemon: connection: {e:#}");
            }
        });
    // On failure the closure, its stream and its slot are dropped: the
    // client sees the connection close and the slot is free again.
    if let Err(e) = spawned {
        anstream::eprintln!("vec daemon: cannot start a connection thread: {e}");
    }
}

// ---------------------------------------------------------------------------
// Connection limit and idle tracking
// ---------------------------------------------------------------------------

struct Gate {
    load: Mutex<Load>,
    max: usize,
}

struct Load {
    active: usize,
    /// When the last connection ended (or the daemon started).
    last_done: Instant,
}

impl Gate {
    fn new(max: usize) -> Self {
        Self {
            load: Mutex::new(Load {
                active: 0,
                last_done: Instant::now(),
            }),
            max,
        }
    }

    /// Lock, recovering from poisoning (the data is two plain fields).
    fn lock(&self) -> MutexGuard<'_, Load> {
        self.load.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// How long no connection has been open; `None` while one is.
    fn idle_for(&self) -> Option<Duration> {
        let load = self.lock();
        (load.active == 0).then(|| load.last_done.elapsed())
    }
}

/// One connection's share of the gate, released on drop (also on panic).
struct Slot(Arc<Gate>);

impl Slot {
    /// A slot, or `None` when all `max_connections` are in use.
    fn try_take(gate: &Arc<Gate>) -> Option<Self> {
        let mut load = gate.lock();
        if load.active >= gate.max {
            return None;
        }
        load.active += 1;
        Some(Self(Arc::clone(gate)))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut load = self.0.lock();
        load.active -= 1;
        load.last_done = Instant::now();
    }
}

// ---------------------------------------------------------------------------
// One connection
// ---------------------------------------------------------------------------

/// What every connection thread shares.
struct Ctx {
    embedder: Embedder,
    /// The handshake reply (the model identity), encoded once.
    hello_reply: Vec<u8>,
    limits: RequestLimits,
    io_timeout: Duration,
}

/// What one request may hold.
#[derive(Debug, Clone, Copy)]
struct RequestLimits {
    /// `[daemon] max_request_bytes`.
    max_bytes: usize,
    /// `[daemon] max_texts_per_request`.
    max_texts: usize,
}

impl Ctx {
    fn new(embedder: Embedder, identity: &ModelIdentity, cfg: &DaemonConfig) -> Result<Self> {
        Ok(Self {
            hello_reply: encode_reply(
                STATUS_OK,
                &serde_json::to_vec(identity).context("encoding the model identity")?,
            ),
            embedder,
            limits: RequestLimits {
                max_bytes: cfg.max_request_bytes,
                max_texts: cfg.max_texts_per_request,
            },
            io_timeout: Duration::from_millis(cfg.io_timeout_ms),
        })
    }
}

/// A request the daemon answers with an error.
struct Rejection {
    message: String,
    /// The rest of the request was not read: the connection cannot go on.
    fatal: bool,
}

/// Serve one connection. Every read and write goes through a
/// [`DeadlineStream`] of `io_timeout`: the handshake, each whole request
/// (including the wait for it) and each whole reply.
fn handle_connection(stream: UnixStream, ctx: &Ctx) -> Result<()> {
    stream
        .set_nonblocking(false)
        .context("making the connection blocking")?;
    let io = || DeadlineStream::within(&stream, ctx.io_timeout);

    let mut hello = [0u8; HELLO_LEN];
    io().read_exact(&mut hello)
        .context("reading the handshake")?;
    if hello != protocol::hello() {
        let msg = format!(
            "unsupported protocol: this daemon speaks vec daemon protocol version {}",
            protocol::VERSION
        );
        // Best effort: the client may already be gone.
        let _ = io().write_all(&encode_reply(STATUS_ERROR, msg.as_bytes()));
        bail!("a client sent an unknown handshake (an older vec?)");
    }
    io().write_all(&ctx.hello_reply)
        .context("writing the handshake reply")?;

    loop {
        // One deadline for the wait for the request and all of its bytes.
        let mut request = io();
        let Some(count) = read_u32_or_eof(&mut request).context("reading a request")? else {
            return Ok(()); // the client is done
        };
        let reply = match read_texts(&mut request, count, ctx.limits)? {
            Ok(texts) => ctx.embed_reply(&texts),
            Err(r) => {
                io().write_all(&encode_reply(STATUS_ERROR, r.message.as_bytes()))
                    .context("writing an error reply")?;
                if r.fatal {
                    return Ok(());
                }
                continue;
            }
        };
        io().write_all(&reply).context("writing a reply")?;
    }
}

/// Read the `count` texts of a request, bounded by `limits` (bytes in total,
/// number of texts). The outer error is an I/O failure (the connection is
/// over); the inner one a request the daemon refuses.
fn read_texts(
    stream: &mut impl Read,
    count: u32,
    limits: RequestLimits,
) -> Result<std::result::Result<Vec<String>, Rejection>> {
    let max = limits.max_bytes;
    let too_large = || Rejection {
        message: format!("request too large (limit {max} bytes)"),
        fatal: true,
    };
    let count = count as usize;
    if count > limits.max_texts {
        return Ok(Err(Rejection {
            message: format!(
                "too many texts in one request: {count} (limit {}, [daemon] \
                 max_texts_per_request)",
                limits.max_texts
            ),
            fatal: true,
        }));
    }
    let mut total = FIELD_LEN.saturating_add(count.saturating_mul(FIELD_LEN));
    if total > max {
        return Ok(Err(too_large()));
    }
    let mut texts = Vec::with_capacity(count);
    let mut bad_utf8 = false;
    for _ in 0..count {
        let len = read_u32(stream).context("reading a text length")? as usize;
        total = total.saturating_add(len);
        if total > max {
            return Ok(Err(too_large()));
        }
        let mut buf = vec![0u8; len];
        stream.read_exact(&mut buf).context("reading a text")?;
        match String::from_utf8(buf) {
            Ok(s) => texts.push(s),
            // Keep reading, so the connection stays in step.
            Err(_) => bad_utf8 = true,
        }
    }
    if bad_utf8 {
        return Ok(Err(Rejection {
            message: "request text is not valid UTF-8".into(),
            fatal: false,
        }));
    }
    Ok(Ok(texts))
}

impl Ctx {
    fn embed_reply(&self, texts: &[String]) -> Vec<u8> {
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        match self.embedder.embed_batch(&refs) {
            Ok(vectors) => encode_vectors_reply(&vectors),
            Err(e) => {
                anstream::eprintln!("vec daemon: embedding failed: {e:#}");
                encode_reply(STATUS_ERROR, EMBED_FAILED.as_bytes())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests (they need Unix sockets, which some sandboxes block)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{self, Client, IdentityMismatch};
    use crate::protocol::{encode_request, hello};
    use std::net::Shutdown;
    use std::thread::JoinHandle;
    use vec_core::config::Config;

    // Test values (not tunables of the product).
    const DIM: usize = 16;
    const IO_TIMEOUT_MS: u64 = 300;
    const MAX_REQUEST: usize = 4096;
    const WAIT_STEP: Duration = Duration::from_millis(10);
    const WAIT_LIMIT: Duration = Duration::from_secs(10);

    fn test_cfg(dir: &tempfile::TempDir, idle_exit_secs: u64) -> DaemonConfig {
        let mut d = Config::defaults().unwrap().daemon;
        d.socket = dir.path().join("run/embed.sock");
        d.io_timeout_ms = IO_TIMEOUT_MS;
        d.max_request_bytes = MAX_REQUEST;
        d.idle_exit_secs = idle_exit_secs;
        d
    }

    fn stub_identity() -> ModelIdentity {
        Embedder::stub(DIM).identity().clone()
    }

    /// Start a daemon and wait until it accepts connections.
    fn start(cfg: &DaemonConfig) -> JoinHandle<Result<()>> {
        let c = cfg.clone();
        let handle = std::thread::spawn(move || run_daemon(Embedder::stub(DIM), &c));
        let t0 = Instant::now();
        while UnixStream::connect(&cfg.socket).is_err() {
            assert!(t0.elapsed() < WAIT_LIMIT, "daemon did not start");
            assert!(!handle.is_finished(), "daemon exited early");
            std::thread::sleep(WAIT_STEP);
        }
        handle
    }

    fn connect_raw(cfg: &DaemonConfig) -> UnixStream {
        let mut s = UnixStream::connect(&cfg.socket).unwrap();
        s.set_read_timeout(Some(WAIT_LIMIT)).unwrap();
        s.write_all(&hello()).unwrap();
        let status = read_u32(&mut s).unwrap();
        let len = read_u32(&mut s).unwrap() as usize;
        let mut payload = vec![0u8; len];
        s.read_exact(&mut payload).unwrap();
        assert_eq!(status, STATUS_OK);
        s
    }

    fn read_reply(s: &mut UnixStream) -> (u32, Vec<u8>) {
        let status = read_u32(s).unwrap();
        let len = read_u32(s).unwrap() as usize;
        let mut payload = vec![0u8; len];
        s.read_exact(&mut payload).unwrap();
        (status, payload)
    }

    #[test]
    fn lifecycle_embeds_batches_then_exits_when_idle() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = test_cfg(&dir, 1);
        let handle = start(&cfg);
        let mode = std::fs::metadata(&cfg.socket).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, cfg.socket_mode.bits());

        let texts = ["alpha", "beta", "gamma"];
        let got = client::embed(&cfg, &stub_identity(), &texts)
            .unwrap()
            .expect("daemon is listening");
        assert_eq!(got, Embedder::stub(DIM).embed_batch(&texts).unwrap());
        assert_eq!(client::identity(&cfg).unwrap(), Some(stub_identity()));

        handle.join().unwrap().unwrap();
        assert!(!cfg.socket.exists(), "a bound socket is removed on exit");
    }

    /// Regression: with socket activation the client connects and sends its
    /// hello before the daemon has started; it must wait for the start-up
    /// (model load + hash) even when that outlasts `io_timeout_ms`.
    #[test]
    fn client_waits_for_a_socket_activated_daemon_to_start() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = test_cfg(&dir, 1);
        std::fs::create_dir_all(cfg.socket.parent().unwrap()).unwrap();
        // Plays systemd: the socket listens before the daemon runs.
        let listener = UnixListener::bind(&cfg.socket).unwrap();
        let c = cfg.clone();
        let daemon = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(IO_TIMEOUT_MS * 3));
            let ctx = Arc::new(Ctx::new(Embedder::stub(DIM), &stub_identity(), &c)?);
            serve(&listener, &ctx, &Limits::from_config(&c))
        });
        let got = client::embed(&cfg, &stub_identity(), &["x"])
            .unwrap()
            .expect("the socket is listening");
        assert_eq!(got, Embedder::stub(DIM).embed_batch(&["x"]).unwrap());
        daemon.join().unwrap().unwrap();
    }

    #[test]
    fn client_reports_no_daemon_as_none() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = test_cfg(&dir, 0);
        assert!(
            client::embed(&cfg, &stub_identity(), &["x"])
                .unwrap()
                .is_none()
        );
        // A stale socket file with nobody listening: also "no daemon".
        std::fs::create_dir_all(cfg.socket.parent().unwrap()).unwrap();
        drop(UnixListener::bind(&cfg.socket).unwrap());
        assert!(client::identity(&cfg).unwrap().is_none());
    }

    #[test]
    fn identity_mismatch_is_an_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = test_cfg(&dir, 1);
        let handle = start(&cfg);
        for expected in [
            ModelIdentity::new("other-model", DIM),
            ModelIdentity::new(stub_identity().name, DIM + 1),
        ] {
            let err = Client::connect(&cfg, &expected).unwrap_err();
            let m = err.downcast_ref::<IdentityMismatch>().expect("typed error");
            assert_eq!(m.daemon, stub_identity());
        }
        handle.join().unwrap().unwrap();
    }

    #[test]
    fn stale_socket_is_replaced_but_live_socket_and_files_are_not() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = test_cfg(&dir, 1);
        std::fs::create_dir_all(cfg.socket.parent().unwrap()).unwrap();

        // Stale: left by a crash, nobody listens.
        drop(UnixListener::bind(&cfg.socket).unwrap());
        let handle = start(&cfg);

        // Live: a second instance must not steal it.
        let err = run_daemon(Embedder::stub(DIM), &cfg).unwrap_err();
        assert!(format!("{err:#}").contains("already listening"), "{err:#}");
        handle.join().unwrap().unwrap();

        // Not a socket: never deleted.
        std::fs::write(&cfg.socket, b"precious").unwrap();
        let err = run_daemon(Embedder::stub(DIM), &cfg).unwrap_err();
        assert!(format!("{err:#}").contains("not a socket"), "{err:#}");
        assert_eq!(std::fs::read(&cfg.socket).unwrap(), b"precious");
    }

    #[test]
    fn stalled_client_does_not_block_others_and_is_dropped() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = test_cfg(&dir, 1);
        let handle = start(&cfg);

        let mut stalled = UnixStream::connect(&cfg.socket).unwrap();
        stalled.set_read_timeout(Some(WAIT_LIMIT)).unwrap();
        // Served while the stalled client holds its connection open.
        assert!(
            client::embed(&cfg, &stub_identity(), &["x"])
                .unwrap()
                .is_some()
        );
        // The daemon drops it after io_timeout_ms: end of stream.
        let mut buf = [0u8; 1];
        assert_eq!(stalled.read(&mut buf).unwrap(), 0);
        handle.join().unwrap().unwrap();
    }

    #[test]
    fn short_body_closes_the_connection_without_a_reply() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = test_cfg(&dir, 1);
        let handle = start(&cfg);
        let mut s = connect_raw(&cfg);
        s.write_all(&1u32.to_le_bytes()).unwrap();
        s.write_all(&10u32.to_le_bytes()).unwrap();
        s.write_all(b"abc").unwrap();
        s.shutdown(Shutdown::Write).unwrap();
        let mut rest = Vec::new();
        s.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty(), "no reply to a truncated request");
        handle.join().unwrap().unwrap();
    }

    #[test]
    fn invalid_utf8_is_rejected_and_the_connection_goes_on() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = test_cfg(&dir, 1);
        let handle = start(&cfg);
        let mut s = connect_raw(&cfg);
        let mut req = Vec::new();
        req.extend_from_slice(&1u32.to_le_bytes());
        req.extend_from_slice(&2u32.to_le_bytes());
        req.extend_from_slice(&[0xff, 0xfe]);
        s.write_all(&req).unwrap();
        let (status, msg) = read_reply(&mut s);
        assert_eq!(status, STATUS_ERROR);
        assert!(String::from_utf8_lossy(&msg).contains("UTF-8"));

        s.write_all(&encode_request(&["fine"])).unwrap();
        let (status, payload) = read_reply(&mut s);
        assert_eq!(status, STATUS_OK);
        assert_eq!(payload.len(), DIM * 4);
        drop(s);
        handle.join().unwrap().unwrap();
    }

    #[test]
    fn oversized_request_is_rejected() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = test_cfg(&dir, 1);
        let handle = start(&cfg);
        let mut s = connect_raw(&cfg);
        s.write_all(&1u32.to_le_bytes()).unwrap();
        s.write_all(&(u32::try_from(MAX_REQUEST).unwrap()).to_le_bytes())
            .unwrap();
        let (status, msg) = read_reply(&mut s);
        assert_eq!(status, STATUS_ERROR);
        assert!(String::from_utf8_lossy(&msg).contains("too large"));
        drop(s);

        // The client splits big batches itself and refuses a text that can
        // never fit.
        let big = "x".repeat(MAX_REQUEST);
        let err = client::embed(&cfg, &stub_identity(), &[&big]).unwrap_err();
        assert!(format!("{err:#}").contains("max_request_bytes"), "{err:#}");
        let many: Vec<String> = (0..200).map(|i| format!("text number {i}")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let got = client::embed(&cfg, &stub_identity(), &refs)
            .unwrap()
            .unwrap();
        assert_eq!(got, Embedder::stub(DIM).embed_batch(&refs).unwrap());
        handle.join().unwrap().unwrap();
    }

    /// Regression (review 2026-11-19 #1): `io_timeout_ms` was applied per
    /// `read(2)`, so a client sending one byte now and then held its
    /// connection (and a `max_connections` slot) forever. Each request now
    /// has one deadline for all its bytes.
    #[test]
    fn trickling_client_is_dropped_after_io_timeout() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = test_cfg(&dir, 1);
        let handle = start(&cfg);
        let mut s = connect_raw(&cfg);
        let step = Duration::from_millis(IO_TIMEOUT_MS / 3);
        let t0 = Instant::now();
        // A request announcing one text of 100 bytes, sent a byte per step.
        let mut request = encode_request(&[&"x".repeat(100)]);
        let mut dropped = false;
        for byte in request.drain(..) {
            std::thread::sleep(step);
            if s.write_all(&[byte]).is_err() {
                dropped = true;
                break;
            }
            if t0.elapsed() > Duration::from_millis(IO_TIMEOUT_MS * 4) {
                break;
            }
        }
        if !dropped {
            // The daemon closed its end: reading gives end of stream.
            let mut buf = [0u8; 1];
            assert_eq!(s.read(&mut buf).unwrap_or(0), 0, "connection still open");
        }
        assert!(
            t0.elapsed() < Duration::from_millis(IO_TIMEOUT_MS * 4),
            "held for {:?}",
            t0.elapsed()
        );
        drop(s);
        handle.join().unwrap().unwrap();
    }

    /// Review 2026-11-19 #1: with every connection in use, a new client gets
    /// an immediate "busy" error (and embeds in-process) instead of waiting
    /// in the backlog until its reply timeout.
    #[test]
    fn client_beyond_max_connections_gets_an_immediate_busy_reply() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_cfg(&dir, 1);
        cfg.max_connections = 1;
        let handle = start(&cfg);
        // start()'s probe connection may still hold the slot for a moment.
        let held = loop {
            let c = Client::connect(&cfg, &stub_identity());
            match c {
                Ok(Some(c)) => break c, // takes the only slot
                _ => std::thread::sleep(WAIT_STEP),
            }
        };
        let t0 = Instant::now();
        let err = Client::connect(&cfg, &stub_identity()).unwrap_err();
        assert!(format!("{err:#}").contains("busy"), "{err:#}");
        assert!(
            t0.elapsed() < Duration::from_millis(IO_TIMEOUT_MS),
            "{:?}",
            t0.elapsed()
        );
        drop(held);
        handle.join().unwrap().unwrap();
    }

    /// Review 2026-11-19 #2: a request of many tiny texts is refused before
    /// any inference (each would cost an inference and dim * 4 reply bytes).
    #[test]
    fn too_many_texts_are_rejected() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_cfg(&dir, 1);
        cfg.max_texts_per_request = 4;
        let handle = start(&cfg);
        let mut s = connect_raw(&cfg);
        s.write_all(&encode_request(&[""; 5])).unwrap();
        let (status, msg) = read_reply(&mut s);
        assert_eq!(status, STATUS_ERROR);
        assert!(
            String::from_utf8_lossy(&msg).contains("max_texts_per_request"),
            "{}",
            String::from_utf8_lossy(&msg)
        );
        drop(s);
        // The client splits by count, so the same texts are served.
        let texts = [""; 9];
        let got = client::embed(&cfg, &stub_identity(), &texts)
            .unwrap()
            .unwrap();
        assert_eq!(got.len(), texts.len());
        handle.join().unwrap().unwrap();
    }

    #[test]
    fn version_one_client_gets_an_error_reply() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = test_cfg(&dir, 1);
        let handle = start(&cfg);
        let mut s = UnixStream::connect(&cfg.socket).unwrap();
        s.set_read_timeout(Some(WAIT_LIMIT)).unwrap();
        let text = b"hello daemon";
        s.write_all(&(text.len() as u32).to_le_bytes()).unwrap();
        s.write_all(text).unwrap();
        let (status, msg) = read_reply(&mut s);
        assert_eq!(status, STATUS_ERROR);
        assert!(String::from_utf8_lossy(&msg).contains("version"));
        drop(s);
        handle.join().unwrap().unwrap();
    }

    #[test]
    #[ignore = "needs a real ONNX model in VEC_TEST_MODEL"]
    fn real_model_daemon_vectors_equal_in_process_vectors() {
        let model = std::path::PathBuf::from(
            std::env::var("VEC_TEST_MODEL").expect("VEC_TEST_MODEL = path of an .onnx file"),
        );
        let cfg = Config::defaults().unwrap();
        let load = || Embedder::load(&model, "test-model", &cfg.onnx_options()).unwrap();
        let local = load();
        // Hash before the daemon starts: it exits after idle_exit_secs, which
        // a debug-build hash would outlast.
        let expected = local.identity_with_sha().unwrap();
        assert!(expected.sha256.is_some());
        let dir = tempfile::TempDir::new().unwrap();
        let mut dcfg = test_cfg(&dir, 1);
        dcfg.io_timeout_ms = cfg.daemon.io_timeout_ms;
        dcfg.max_request_bytes = cfg.daemon.max_request_bytes;
        let daemon_embedder = load();
        let c = dcfg.clone();
        let handle = std::thread::spawn(move || run_daemon(daemon_embedder, &c));
        let t0 = Instant::now();
        while UnixStream::connect(&dcfg.socket).is_err() {
            assert!(t0.elapsed() < WAIT_LIMIT * 6, "daemon did not start");
            std::thread::sleep(WAIT_STEP);
        }

        let mut client = Client::connect(&dcfg, &expected).unwrap().unwrap();
        assert_eq!(
            client.identity(),
            &expected,
            "the daemon reports the file hash"
        );
        let texts = [
            "fn main() { println!(\"hello\"); }",
            "Rotate the TLS certificates every 90 days.",
            "Die Katze schläft auf dem Sofa.",
        ];
        let remote = client.embed(&texts).unwrap();
        let in_process = local.embed_batch(&texts).unwrap();
        for (r, l) in remote.iter().zip(&in_process) {
            let max_diff = r
                .iter()
                .zip(l)
                .map(|(a, b)| (a - b).abs())
                .fold(0f32, f32::max);
            assert!(max_diff < 1e-4, "max diff {max_diff}");
        }
        drop(client);
        handle.join().unwrap().unwrap();
    }
}
