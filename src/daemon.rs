// daemon.rs — persistent embedding daemon for `vec daemon`.
//
// Solves Gap 9: `vec <query>` recompiles the tract ONNX graph on every
// invocation (seconds for large models). The daemon loads the model once at
// startup and keeps the compiled plan in memory indefinitely.
//
// `vec <query>` tries the Unix socket first; if the daemon isn't running it
// falls back to in-process model loading with no change in behaviour.
//
// Protocol (little-endian throughout):
//
//   Request:  [4-byte u32 text_len] [text_len bytes of UTF-8 text]
//   Response: [4-byte u32 status  ] [4-byte u32 data_len] [data_len bytes]
//
//   status = 0  → success; data = raw f32 LE bytes of the embedding vector
//   status != 0 → error;   data = UTF-8 error message

// Only meaningful on Unix; the subcommand is still registered on all platforms
// so clap parses cleanly.

#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(unix)]
use std::io::{Read, Write};

use std::path::Path;
use anyhow::{Context, Result};
use crate::embed::Embedder;

/// Start the embedding daemon.
///
/// Loads the embedder, binds a Unix domain socket at `socket_path`, then
/// serves embed requests in an infinite loop (one at a time). Designed to
/// run as a systemd service.
///
/// # Errors
///
/// Returns an error if the model cannot be loaded or the socket cannot be
/// bound (e.g. permission denied or parent directory missing).
#[cfg(unix)]
pub fn run_daemon(mut embedder: Embedder, socket_path: &Path) -> Result<()> {
    // Remove a stale socket left by a previous crash.
    let _ = std::fs::remove_file(socket_path);

    // Ensure the parent directory exists (systemd RuntimeDirectory=vec handles
    // this in production; the explicit create_dir_all covers dev runs).
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating socket directory {}", parent.display()))?;
    }

    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("binding Unix socket at {}", socket_path.display()))?;

    // Make the socket world-writable so any user can connect.
    // The daemon produces only float vectors — no sensitive data is returned.
    std::fs::set_permissions(
        socket_path,
        std::os::unix::fs::PermissionsExt::from_mode(0o666),
    )
    .with_context(|| format!("setting permissions on {}", socket_path.display()))?;

    anstream::eprintln!(
        "vec-embed daemon ready  socket={}  pid={}",
        socket_path.display(),
        std::process::id(),
    );

    for stream in listener.incoming() {
        match stream {
            Ok(mut conn) => {
                if let Err(e) = handle_connection(&mut conn, &mut embedder) {
                    anstream::eprintln!("connection error: {e}");
                }
            }
            Err(e) => anstream::eprintln!("accept error: {e}"),
        }
    }

    Ok(())
}

/// Handle one embed request.  Reads text, embeds it, writes response.
#[cfg(unix)]
pub(crate) fn handle_connection(
    stream: &mut UnixStream,
    embedder: &mut Embedder,
) -> Result<()> {
    // Read text length.
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).context("reading text length")?;
    let text_len = u32::from_le_bytes(len_buf) as usize;

    // Guard against absurdly large requests (>= 1 MiB).
    if text_len > 1_048_576 {
        let msg = b"request too large";
        stream.write_all(&1u32.to_le_bytes()).context("writing error status")?;
        stream.write_all(&(msg.len() as u32).to_le_bytes()).context("writing error len")?;
        stream.write_all(msg).context("writing error body")?;
        return Ok(());
    }

    // Read text.
    let mut text_buf = vec![0u8; text_len];
    stream.read_exact(&mut text_buf).context("reading text")?;
    let text = String::from_utf8_lossy(&text_buf);

    // Embed.
    let (status, data): (u32, Vec<u8>) = match embedder.embed_one(&text) {
        Ok(vec) => {
            let bytes: Vec<u8> = vec.iter().flat_map(|f| f.to_le_bytes()).collect();
            (0, bytes)
        }
        Err(e) => {
            let msg = format!("{e:?}");
            (1, msg.into_bytes())
        }
    };

    // Write response.
    stream.write_all(&status.to_le_bytes()).context("writing status")?;
    stream
        .write_all(&(data.len() as u32).to_le_bytes())
        .context("writing data length")?;
    stream.write_all(&data).context("writing data")?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use crate::store::unpack_f32;

    /// Spin up a stub embedder on a temp socket, send one request, check response.
    #[test]
    fn daemon_handles_embed_request() {
        let tmp = tempfile::TempDir::new().unwrap();
        let socket_path = tmp.path().join("test.sock");

        let listener = UnixListener::bind(&socket_path).unwrap();

        // Serve exactly one request in a background thread.
        let handle = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut embedder = Embedder::stub(128);
            handle_connection(&mut conn, &mut embedder).unwrap();
        });

        // Connect and send a request.
        let mut client = UnixStream::connect(&socket_path).unwrap();
        let text = b"hello daemon";
        client.write_all(&(text.len() as u32).to_le_bytes()).unwrap();
        client.write_all(text).unwrap();

        // Read response.
        let mut status_buf = [0u8; 4];
        client.read_exact(&mut status_buf).unwrap();
        let status = u32::from_le_bytes(status_buf);

        let mut len_buf = [0u8; 4];
        client.read_exact(&mut len_buf).unwrap();
        let data_len = u32::from_le_bytes(len_buf) as usize;

        let mut data = vec![0u8; data_len];
        client.read_exact(&mut data).unwrap();

        handle.join().unwrap();

        assert_eq!(status, 0, "daemon should return status 0 on success");
        assert_eq!(data_len, 128 * 4, "expected 128 f32s = 512 bytes");

        let floats = unpack_f32(&data);
        assert_eq!(floats.len(), 128);
        let norm: f32 = floats.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-4,
            "embedding must be unit-length, norm={norm}"
        );
    }

    #[test]
    fn daemon_rejects_oversized_request() {
        let tmp = tempfile::TempDir::new().unwrap();
        let socket_path = tmp.path().join("big.sock");

        let listener = UnixListener::bind(&socket_path).unwrap();

        let handle = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut embedder = Embedder::stub(128);
            handle_connection(&mut conn, &mut embedder).unwrap();
        });

        let mut client = UnixStream::connect(&socket_path).unwrap();
        // Send a length > 1 MiB but don't actually send data.
        let huge_len: u32 = 2_000_000;
        client.write_all(&huge_len.to_le_bytes()).unwrap();
        // Connection will be answered with an error response.

        let mut status_buf = [0u8; 4];
        client.read_exact(&mut status_buf).unwrap();
        let status = u32::from_le_bytes(status_buf);

        // Drain remaining response fields.
        let mut len_buf = [0u8; 4];
        client.read_exact(&mut len_buf).unwrap();
        let data_len = u32::from_le_bytes(len_buf) as usize;
        let mut _body = vec![0u8; data_len];
        client.read_exact(&mut _body).unwrap();

        handle.join().unwrap();

        assert_ne!(status, 0, "daemon must reject oversized requests");
    }
}
