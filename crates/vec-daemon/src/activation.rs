//! systemd socket activation (`sd_listen_fds(3)` protocol, without libsystemd).
//!
//! systemd passes listening sockets as file descriptors starting at 3 and
//! sets `LISTEN_PID` (the pid they are meant for) and `LISTEN_FDS` (how many).

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use nix::fcntl::{FcntlArg, FdFlag, fcntl};
use nix::sys::socket::{SockType, UnixAddr, getsockname, getsockopt, sockopt};

/// First file descriptor passed by systemd (`SD_LISTEN_FDS_START`, fixed by
/// the protocol).
const LISTEN_FDS_START: RawFd = 3;

/// Set once the passed descriptor has been taken: owning it twice would close
/// it twice. Production calls [`activated_listener`] once per process, but
/// `run_daemon` is public and may be called again (after the first listener
/// was dropped, descriptor 3 may belong to another file); this flag is what
/// makes the `unsafe` below sound regardless of callers, so it stays.
static TAKEN: AtomicBool = AtomicBool::new(false);

/// How many descriptors systemd passed to process `own_pid`, from the values
/// of `LISTEN_PID` and `LISTEN_FDS`. `None` when not socket-activated (or
/// the variables are meant for another process, e.g. a parent).
pub(crate) fn passed_fds(
    listen_pid: Option<&str>,
    listen_fds: Option<&str>,
    own_pid: u32,
) -> Result<Option<usize>> {
    let Some(pid) = listen_pid else {
        return Ok(None);
    };
    let pid: u32 = pid
        .trim()
        .parse()
        .with_context(|| format!("LISTEN_PID {pid:?} is not a process id"))?;
    if pid != own_pid {
        return Ok(None);
    }
    let fds = listen_fds.context("LISTEN_PID is set but LISTEN_FDS is not")?;
    let n: usize = fds
        .trim()
        .parse()
        .with_context(|| format!("LISTEN_FDS {fds:?} is not a number"))?;
    Ok((n > 0).then_some(n))
}

/// The listening Unix socket passed by systemd, or `None` when the process
/// was not socket-activated.
pub(crate) fn activated_listener() -> Result<Option<UnixListener>> {
    let var = |k| std::env::var(k).ok();
    let Some(n) = passed_fds(
        var("LISTEN_PID").as_deref(),
        var("LISTEN_FDS").as_deref(),
        std::process::id(),
    )?
    else {
        return Ok(None);
    };
    if n != 1 {
        bail!("systemd passed {n} sockets (LISTEN_FDS); the vec daemon expects exactly one");
    }
    if TAKEN.swap(true, Ordering::SeqCst) {
        bail!("the socket passed by systemd was already taken");
    }
    // SAFETY: LISTEN_PID names this process, so systemd passed descriptor 3 to
    // it and nothing else owns it; TAKEN ensures it is wrapped only once.
    let fd = unsafe { OwnedFd::from_raw_fd(LISTEN_FDS_START) };
    check_listening_unix_stream(&fd)?;
    fcntl(fd.as_raw_fd(), FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))
        .context("setting close-on-exec on the passed socket")?;
    Ok(Some(UnixListener::from(fd)))
}

/// Refuse a passed descriptor that is not a listening Unix stream socket
/// (e.g. a .socket unit with ListenDatagram= or a TCP port).
fn check_listening_unix_stream(fd: &OwnedFd) -> Result<()> {
    let ty = getsockopt(fd, sockopt::SockType).context("passed descriptor is not a socket")?;
    if ty != SockType::Stream {
        bail!("the socket passed by systemd is not a stream socket (ListenStream=)");
    }
    if !getsockopt(fd, sockopt::AcceptConn).context("reading SO_ACCEPTCONN")? {
        bail!("the socket passed by systemd is not listening");
    }
    getsockname::<UnixAddr>(fd.as_raw_fd())
        .context("the socket passed by systemd is not a Unix socket (ListenStream=/path)")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_activated_without_listen_pid() {
        assert_eq!(passed_fds(None, Some("1"), 42).unwrap(), None);
    }

    #[test]
    fn variables_for_another_process_are_ignored() {
        assert_eq!(passed_fds(Some("41"), Some("1"), 42).unwrap(), None);
    }

    #[test]
    fn activated_with_matching_pid() {
        assert_eq!(passed_fds(Some("42"), Some("1"), 42).unwrap(), Some(1));
        assert_eq!(passed_fds(Some("42"), Some("0"), 42).unwrap(), None);
    }

    #[test]
    fn malformed_variables_are_errors() {
        assert!(passed_fds(Some("x"), Some("1"), 42).is_err());
        assert!(passed_fds(Some("42"), None, 42).is_err());
        assert!(passed_fds(Some("42"), Some("two"), 42).is_err());
    }
}
