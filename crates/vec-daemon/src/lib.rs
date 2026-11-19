//! vec-daemon — the persistent embedding daemon (`vec daemon`) and its client.
//!
//! Loading an ONNX model compiles its graph, which takes seconds. The daemon
//! loads the model once and serves embeddings over a Unix socket, so
//! interactive queries skip that cost. Callers use [`client`] and fall back to
//! in-process embedding when no daemon listens.
//!
//! - [`run_daemon`] serves one thread per connection (at most
//!   `[daemon] max_connections`; a client beyond that gets an immediate
//!   "busy" reply), with a deadline for each whole request and reply
//!   (`[daemon] io_timeout_ms`), limits on request size and text count, and
//!   an optional exit after `[daemon] idle_exit_secs` without connections. It accepts a socket passed by systemd socket activation
//!   (`LISTEN_FDS`/`LISTEN_PID`) or binds `[daemon] socket` itself.
//! - [`client`] connects, checks the daemon's model identity against the one
//!   the caller expects, and embeds batches of texts.
//! - [`protocol`] describes the wire format.

pub mod protocol;

#[cfg(unix)]
mod activation;
#[cfg(unix)]
pub mod client;
#[cfg(unix)]
mod deadline;
#[cfg(unix)]
mod server;

#[cfg(unix)]
pub use server::run_daemon;
