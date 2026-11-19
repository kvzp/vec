//! Socket I/O bounded by a deadline for a whole exchange.
//!
//! `SO_RCVTIMEO` / `SO_SNDTIMEO` bound each `read(2)` / `write(2)` call, not a
//! whole message: a peer that sends or reads one byte now and then would never
//! time out. [`DeadlineStream`] sets the time left until its deadline before
//! every call, so a message must be complete by the deadline however the peer
//! spreads its bytes.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

/// A view of a stream whose reads and writes all end by `deadline`.
///
/// A missed deadline is reported as `TimedOut` (before a call) or
/// `WouldBlock` (the kernel's timeout, during one); see [`is_timeout`].
pub(crate) struct DeadlineStream<'a> {
    stream: &'a UnixStream,
    deadline: Instant,
}

impl<'a> DeadlineStream<'a> {
    pub(crate) fn new(stream: &'a UnixStream, deadline: Instant) -> Self {
        Self { stream, deadline }
    }

    /// A deadline `budget` from now.
    pub(crate) fn within(stream: &'a UnixStream, budget: Duration) -> Self {
        Self::new(stream, Instant::now() + budget)
    }

    fn left(&self) -> io::Result<Duration> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            Err(io::ErrorKind::TimedOut.into())
        } else {
            Ok(left)
        }
    }
}

impl Read for DeadlineStream<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.stream.set_read_timeout(Some(self.left()?))?;
        (&*self.stream).read(buf)
    }
}

impl Write for DeadlineStream<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.stream.set_write_timeout(Some(self.left()?))?;
        (&*self.stream).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The kinds a missed deadline is reported as (`EAGAIN` from a socket
/// timeout is `WouldBlock`).
pub(crate) fn is_timeout(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    // Test values (not tunables of the product).
    const BUDGET: Duration = Duration::from_millis(300);
    const STEP: Duration = Duration::from_millis(100);

    /// A peer trickling one byte per `STEP` cannot stretch a read past its
    /// deadline, although no single `read(2)` waits longer than `STEP`.
    #[test]
    fn trickled_read_fails_at_the_deadline() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let writer = thread::spawn(move || {
            for _ in 0..20 {
                thread::sleep(STEP);
                if b.write_all(b"x").is_err() {
                    return;
                }
            }
        });
        let t0 = Instant::now();
        let mut buf = [0u8; 20];
        let err = DeadlineStream::within(&a, BUDGET)
            .read_exact(&mut buf)
            .unwrap_err();
        assert!(is_timeout(&err), "{err}");
        assert!(t0.elapsed() < BUDGET + STEP * 3, "{:?}", t0.elapsed());
        drop(a);
        writer.join().unwrap();
    }

    #[test]
    fn complete_message_is_read() {
        let (a, mut b) = UnixStream::pair().unwrap();
        b.write_all(b"hello").unwrap();
        let mut buf = [0u8; 5];
        DeadlineStream::within(&a, BUDGET)
            .read_exact(&mut buf)
            .unwrap();
        assert_eq!(&buf, b"hello");
    }
}
