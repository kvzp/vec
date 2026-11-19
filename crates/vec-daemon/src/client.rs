//! Client of the embedding daemon.
//!
//! ```no_run
//! # fn main() -> anyhow::Result<()> {
//! # let cfg = vec_core::config::Config::defaults()?;
//! // The identity the caller's vectors must match, e.g. the index's model
//! // (`vec_store::Store::model`) or the configured model and dimension.
//! let expected = vec_embed::ModelIdentity::new(cfg.embed.model.clone(), 768);
//! match vec_daemon::client::embed(&cfg.daemon, &expected, &["a query"])? {
//!     Some(vectors) => { /* use them */ }
//!     None => { /* no daemon: embed in-process */ }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! Two timeouts bound every exchange, so a misbehaving daemon can never hang
//! the caller:
//!
//! - `[daemon] reply_timeout_ms`: how long the client waits for the first
//!   bytes of a reply. A reply legitimately takes long: the handshake reply of
//!   a socket-activated daemon waits for the model to load and its file to be
//!   hashed, and a request's reply waits for the inference of its batch.
//! - `[daemon] io_timeout_ms`: once a reply has started, the rest of it must
//!   arrive within this; each write must progress within it too. It catches a
//!   stalled peer, not a slow model.
//!
//! Both are deadlines for the whole read, not per `read(2)` call, so a
//! daemon trickling bytes cannot stretch them. Replies are also bounded by
//! what the request asked for, so the client never allocates without limit.

use std::fmt;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use vec_core::config::DaemonConfig;
use vec_embed::ModelIdentity;

use crate::deadline::{DeadlineStream, is_timeout};
use crate::protocol::{FIELD_LEN, STATUS_OK, decode_vectors, encode_request, hello, request_len};

/// The daemon serves another model than the caller expects. Returned inside
/// the `anyhow::Error` of [`Client::connect`] and [`embed`]; find it with
/// `err.downcast_ref::<IdentityMismatch>()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityMismatch {
    /// What the caller expected.
    pub expected: ModelIdentity,
    /// What the daemon serves.
    pub daemon: ModelIdentity,
}

impl fmt::Display for IdentityMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the embedding daemon serves model {}, but {} is expected; restart the daemon \
             after changing [embed] model",
            self.daemon, self.expected
        )
    }
}

impl std::error::Error for IdentityMismatch {}

/// A connection to the daemon whose model identity has been checked. Reuse it
/// for several [`Client::embed`] calls.
#[derive(Debug)]
pub struct Client {
    stream: UnixStream,
    identity: ModelIdentity,
    max_request_bytes: usize,
    max_texts: usize,
    io_timeout: Duration,
    reply_timeout: Duration,
}

impl Client {
    /// Connect to `[daemon] socket` and check that the daemon's model is
    /// compatible with `expected` ([`ModelIdentity::is_compatible_with`]:
    /// same name and dimension, same sha256 when both are known).
    ///
    /// `Ok(None)` when no daemon can be used: the socket is missing, nobody
    /// listens on it, or this user may not connect to it.
    ///
    /// # Errors
    ///
    /// [`IdentityMismatch`] when the daemon serves another model; an error
    /// for a timeout or a protocol violation (e.g. a daemon of an older
    /// version).
    pub fn connect(cfg: &DaemonConfig, expected: &ModelIdentity) -> Result<Option<Self>> {
        let Some(client) = Self::handshake(cfg)? else {
            return Ok(None);
        };
        if !expected.is_compatible_with(&client.identity) {
            return Err(IdentityMismatch {
                expected: expected.clone(),
                daemon: client.identity,
            }
            .into());
        }
        Ok(Some(client))
    }

    /// The daemon's model identity (with the model file's sha256 for ONNX).
    #[must_use]
    pub fn identity(&self) -> &ModelIdentity {
        &self.identity
    }

    /// Embed `texts`: one unit vector of `identity().dim` floats per text, in
    /// order. Texts are sent in as many requests as `[daemon]
    /// max_request_bytes` and `[daemon] max_texts_per_request` require; each
    /// reply may take up to `[daemon] reply_timeout_ms`.
    ///
    /// The daemon closes a connection that sends no request for `[daemon]
    /// io_timeout_ms`, so reuse a `Client` for back-to-back batches, not
    /// across idle periods.
    ///
    /// # Errors
    ///
    /// A text longer than `max_request_bytes` allows, a timeout, an error
    /// reported by the daemon, or a malformed reply.
    pub fn embed(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for batch in split_requests(texts, self.max_request_bytes, self.max_texts)? {
            self.embed_request(batch, &mut out)?;
        }
        Ok(out)
    }

    fn handshake(cfg: &DaemonConfig) -> Result<Option<Self>> {
        let stream = match UnixStream::connect(&cfg.socket) {
            Ok(s) => s,
            Err(e) if no_daemon(&e) => return Ok(None),
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "connecting to the embedding daemon at {}",
                        cfg.socket.display()
                    )
                });
            }
        };
        Self::start(stream, cfg).map(Some)
    }

    /// Run the handshake on a connected `stream`. The reply may take up to
    /// `reply_timeout_ms`: with socket activation it comes once the daemon
    /// has loaded and hashed the model.
    fn start(stream: UnixStream, cfg: &DaemonConfig) -> Result<Self> {
        let io_timeout = Duration::from_millis(cfg.io_timeout_ms);
        // Reads set their own timeout from their deadline (read_exact_by).
        stream
            .set_write_timeout(Some(io_timeout))
            .context("setting the write timeout")?;
        let mut client = Self {
            stream,
            identity: ModelIdentity::new(String::new(), 0),
            max_request_bytes: cfg.max_request_bytes,
            max_texts: cfg.max_texts_per_request,
            io_timeout,
            reply_timeout: Duration::from_millis(cfg.reply_timeout_ms),
        };
        client
            .stream
            .write_all(&hello())
            .context("sending the handshake to the embedding daemon")?;
        let payload = client
            .read_reply(cfg.max_request_bytes)
            .context("embedding daemon handshake (a daemon of another vec version?)")?;
        let identity: ModelIdentity = serde_json::from_slice(&payload)
            .context("the embedding daemon sent an invalid model identity")?;
        if identity.dim == 0 {
            bail!("the embedding daemon reports model {identity} with dimension 0");
        }
        client.identity = identity;
        Ok(client)
    }

    fn embed_request(&mut self, texts: &[&str], out: &mut Vec<Vec<f32>>) -> Result<()> {
        self.stream
            .write_all(&encode_request(texts))
            .context("sending a request to the embedding daemon")?;
        let expected = texts.len() * self.identity.dim * size_of::<f32>();
        let payload = self.read_reply(expected)?;
        if payload.len() != expected {
            bail!(
                "the embedding daemon returned {} bytes for {} texts of dimension {}",
                payload.len(),
                texts.len(),
                self.identity.dim
            );
        }
        out.extend(decode_vectors(&payload, self.identity.dim));
        Ok(())
    }

    /// Read one reply; its payload must not exceed `limit` bytes (error
    /// messages may use up to `max_request_bytes`).
    ///
    /// The status field must arrive within `reply_timeout` (the daemon is
    /// working), the rest of the reply within `io_timeout` after it (the
    /// daemon is only sending).
    fn read_reply(&mut self, limit: usize) -> Result<Vec<u8>> {
        let mut field = [0u8; FIELD_LEN];
        let deadline = Instant::now() + self.reply_timeout;
        read_exact_by(&self.stream, &mut field, deadline)
            .map_err(|e| self.io_error(e, Wait::Reply))?;
        let status = u32::from_le_bytes(field);

        let deadline = Instant::now() + self.io_timeout;
        read_exact_by(&self.stream, &mut field, deadline)
            .map_err(|e| self.io_error(e, Wait::Rest))?;
        let len = u32::from_le_bytes(field) as usize;
        let allowed = if status == STATUS_OK {
            limit
        } else {
            self.max_request_bytes
        };
        if len > allowed {
            bail!("the embedding daemon sent a reply of {len} bytes, at most {allowed} expected");
        }
        let mut payload = vec![0u8; len];
        read_exact_by(&self.stream, &mut payload, deadline)
            .map_err(|e| self.io_error(e, Wait::Rest))?;
        if status != STATUS_OK {
            bail!(
                "the embedding daemon refused the request: {}",
                String::from_utf8_lossy(&payload)
            );
        }
        Ok(payload)
    }

    #[cold]
    fn io_error(&self, e: io::Error, wait: Wait) -> anyhow::Error {
        if !is_timeout(&e) {
            return anyhow::Error::new(e).context("reading the embedding daemon's reply");
        }
        match wait {
            Wait::Reply => anyhow::anyhow!(
                "the embedding daemon did not answer within [daemon] reply_timeout_ms ({} ms)",
                self.reply_timeout.as_millis()
            ),
            Wait::Rest => anyhow::anyhow!(
                "the embedding daemon started a reply but did not finish it within \
                 [daemon] io_timeout_ms ({} ms)",
                self.io_timeout.as_millis()
            ),
        }
    }
}

/// Which part of a reply a read was waiting for (selects the timeout named
/// in the error).
#[derive(Debug, Clone, Copy)]
enum Wait {
    /// The first bytes: the daemon computes the reply.
    Reply,
    /// The rest, once the reply has started.
    Rest,
}

/// Fill `buf` from `stream` before `deadline` (a whole-read deadline, see
/// [`DeadlineStream`]). The end of stream is `UnexpectedEof`.
fn read_exact_by(stream: &UnixStream, buf: &mut [u8], deadline: Instant) -> io::Result<()> {
    DeadlineStream::new(stream, deadline).read_exact(buf)
}

/// Connect errors that mean "no usable daemon here": fall back in-process.
fn no_daemon(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::PermissionDenied
    )
}

/// Split `texts` into consecutive runs whose request fits in `max` bytes
/// and holds at most `max_texts` texts.
fn split_requests<'a, 't>(
    texts: &'a [&'t str],
    max: usize,
    max_texts: usize,
) -> Result<Vec<&'a [&'t str]>> {
    let mut runs = Vec::new();
    let mut start = 0;
    while start < texts.len() {
        let mut end = start;
        while end < texts.len()
            && end - start < max_texts
            && request_len(&texts[start..=end]) <= max
        {
            end += 1;
        }
        if end == start {
            bail!(
                "a text of {} bytes does not fit in a daemon request ([daemon] \
                 max_request_bytes = {max})",
                texts[start].len()
            );
        }
        runs.push(&texts[start..end]);
        start = end;
    }
    Ok(runs)
}

/// The daemon's model identity, without checking it (for diagnostics such
/// as `vec doctor`). `Ok(None)` when no daemon can be used.
///
/// # Errors
///
/// A timeout or a protocol violation.
pub fn identity(cfg: &DaemonConfig) -> Result<Option<ModelIdentity>> {
    Ok(Client::handshake(cfg)?.map(|c| c.identity))
}

/// Embed `texts` with the daemon, after checking that it serves a model
/// compatible with `expected`. One connection per call; use [`Client`] to
/// send several batches over one connection.
///
/// `Ok(None)` when no daemon can be used (the caller embeds in-process).
///
/// # Errors
///
/// [`IdentityMismatch`], a timeout, an error reported by the daemon (e.g.
/// "busy": all its connections are in use), or a protocol violation (e.g. a
/// daemon of an older version). Callers may embed in-process instead, but
/// should say so (vec-query prints a one-line warning): these errors can mean
/// a misconfigured or outdated daemon.
pub fn embed(
    cfg: &DaemonConfig,
    expected: &ModelIdentity,
    texts: &[&str],
) -> Result<Option<Vec<Vec<f32>>>> {
    match Client::connect(cfg, expected)? {
        Some(mut c) => c.embed(texts).map(Some),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{encode_reply, encode_vectors_reply, read_u32};
    use std::thread::{self, JoinHandle};
    use vec_core::config::Config;

    // Test values (not tunables of the product).
    const DIM: usize = 4;
    const IO_TIMEOUT_MS: u64 = 100;
    const REPLY_TIMEOUT_MS: u64 = 1_500;

    fn test_cfg() -> DaemonConfig {
        let mut d = Config::defaults().unwrap().daemon;
        d.io_timeout_ms = IO_TIMEOUT_MS;
        d.reply_timeout_ms = REPLY_TIMEOUT_MS;
        d
    }

    /// Longer than `io_timeout_ms`, well within `reply_timeout_ms`.
    fn slow() -> Duration {
        Duration::from_millis(IO_TIMEOUT_MS * 4)
    }

    fn identity_reply() -> Vec<u8> {
        let id = serde_json::to_vec(&ModelIdentity::new("m", DIM)).unwrap();
        encode_reply(STATUS_OK, &id)
    }

    /// A scripted daemon on the other end of a socket pair; it has already
    /// read the client's hello when `script` runs.
    fn fake_daemon(
        script: impl FnOnce(&mut UnixStream) + Send + 'static,
    ) -> (UnixStream, JoinHandle<()>) {
        let (client, mut daemon) = UnixStream::pair().unwrap();
        let handle = thread::spawn(move || {
            let mut hello = [0u8; FIELD_LEN * 2];
            daemon.read_exact(&mut hello).unwrap();
            script(&mut daemon);
        });
        (client, handle)
    }

    /// Regression: a daemon that takes longer than `io_timeout_ms` to start
    /// (socket activation: model load + hash) and to embed is still waited
    /// for; only `reply_timeout_ms` bounds that.
    #[test]
    fn slow_but_alive_daemon_is_waited_for() {
        let (stream, daemon) = fake_daemon(|d| {
            thread::sleep(slow());
            d.write_all(&identity_reply()).unwrap();
            let count = read_u32(d).unwrap();
            let len = read_u32(d).unwrap() as usize;
            let mut text = vec![0u8; len];
            d.read_exact(&mut text).unwrap();
            assert_eq!(count, 1);
            thread::sleep(slow());
            d.write_all(&encode_vectors_reply(&[vec![0.5; DIM]]))
                .unwrap();
        });
        let mut client = Client::start(stream, &test_cfg()).unwrap();
        assert_eq!(client.identity(), &ModelIdentity::new("m", DIM));
        assert_eq!(client.embed(&["x"]).unwrap(), vec![vec![0.5; DIM]]);
        daemon.join().unwrap();
    }

    #[test]
    fn silent_daemon_fails_after_reply_timeout() {
        let (stream, daemon) = fake_daemon(|d| {
            // Say nothing; return once the client hangs up.
            let _ = d.read_to_end(&mut Vec::new());
        });
        let t0 = Instant::now();
        let err = Client::start(stream, &test_cfg()).unwrap_err();
        let waited = t0.elapsed();
        assert!(format!("{err:#}").contains("reply_timeout_ms"), "{err:#}");
        assert!(
            waited >= Duration::from_millis(REPLY_TIMEOUT_MS),
            "gave up after {waited:?}"
        );
        daemon.join().unwrap();
    }

    /// Regression: the rest of a reply is bounded as a whole, not per
    /// `read(2)`: a daemon sending a byte every half `io_timeout_ms` would
    /// otherwise never time out.
    #[test]
    fn trickling_reply_fails_after_io_timeout() {
        let (stream, daemon) = fake_daemon(|d| {
            let reply = identity_reply();
            let (status, rest) = reply.split_at(FIELD_LEN);
            d.write_all(status).unwrap();
            for byte in rest {
                thread::sleep(Duration::from_millis(IO_TIMEOUT_MS / 2));
                if d.write_all(std::slice::from_ref(byte)).is_err() {
                    return; // the client gave up
                }
            }
        });
        let err = Client::start(stream, &test_cfg()).unwrap_err();
        assert!(format!("{err:#}").contains("io_timeout_ms"), "{err:#}");
        daemon.join().unwrap();
    }

    #[test]
    fn daemon_hanging_up_is_not_a_timeout() {
        let (stream, daemon) = fake_daemon(drop_stream);
        let err = Client::start(stream, &test_cfg()).unwrap_err();
        let text = format!("{err:#}");
        assert!(!text.contains("timeout_ms"), "{text}");
        daemon.join().unwrap();
    }

    fn drop_stream(d: &mut UnixStream) {
        d.shutdown(std::net::Shutdown::Both).unwrap();
    }

    #[test]
    fn split_respects_the_request_limit() {
        let texts = ["aaaa", "bb", "cccccc", "d"];
        let max = request_len(&texts[..2]);
        let runs = split_requests(&texts, max, texts.len()).unwrap();
        assert_eq!(runs.concat(), texts);
        assert!(runs.iter().all(|r| request_len(r) <= max));
        assert_eq!(runs[0], &texts[..2]);
    }

    /// Review 2026-11-19 #2: requests never carry more than
    /// `max_texts_per_request` texts, however short they are.
    #[test]
    fn split_respects_the_text_count_limit() {
        let texts = [""; 7];
        let runs = split_requests(&texts, usize::MAX, 3).unwrap();
        assert_eq!(runs.iter().map(|r| r.len()).collect::<Vec<_>>(), [3, 3, 1]);
    }

    #[test]
    fn split_refuses_a_text_that_never_fits() {
        let err = split_requests(&["x"], request_len(&["x"]) - 1, 1).unwrap_err();
        assert!(err.to_string().contains("max_request_bytes"), "{err}");
    }

    #[test]
    fn split_of_nothing_is_nothing() {
        assert!(split_requests(&[], 1, 1).unwrap().is_empty());
    }

    #[test]
    fn mismatch_message_names_both_models() {
        let m = IdentityMismatch {
            expected: ModelIdentity::new("a", 1),
            daemon: ModelIdentity::new("b", 2),
        };
        let s = m.to_string();
        assert!(s.contains("a (dim 1)") && s.contains("b (dim 2)"), "{s}");
    }
}
