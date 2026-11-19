//! util.rs — small cross-platform helpers.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

/// Returns `true` if the current process has read access to `path`.
///
/// On Unix (Linux, macOS, BSDs) this calls the `access(R_OK)` syscall, which
/// checks the *real* UID and GID of the process (not the effective ones) against
/// the file's permission bits, without opening the file. For a setuid program
/// this answers "may the invoking user read it?"; `faccessat(AT_EACCESS)` would
/// check the effective IDs instead.
///
/// On non-Unix platforms (Windows) we fall back to a simple open-for-read
/// attempt: if `File::open` succeeds we have read access.
pub fn can_read(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use nix::unistd::AccessFlags;
        nix::unistd::access(path.as_os_str(), AccessFlags::R_OK).is_ok()
    }
    #[cfg(not(unix))]
    {
        std::fs::File::open(path).is_ok()
    }
}

/// Resolve `.` and `..` components without touching the filesystem (`..` at
/// the root stays at the root; symlinks are not resolved).
#[must_use]
pub fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                // ".." at the root stays at the root.
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => out.push(comp),
            },
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => out.push(comp),
        }
    }
    out
}

/// `path` made absolute against the current directory, with `.` and `..`
/// resolved lexically: the form in which the index stores paths (the walker
/// does not resolve symlinks either).
///
/// # Errors
///
/// The current directory cannot be determined (only for a relative path).
pub fn absolute_lexical(path: &Path) -> Result<PathBuf> {
    let abs = std::path::absolute(path)
        .with_context(|| format!("resolving {} against the current directory", path.display()))?;
    Ok(normalize_lexically(&abs))
}

/// Read `path` as UTF-8 text, refusing anything but a regular file of at most
/// `max_bytes` bytes.
///
/// The final component is opened without following a symlink and without
/// blocking (a FIFO swapped in cannot hang the reader); the type and size are
/// checked on the open handle, and at most `max_bytes + 1` bytes are read, so
/// a file that grows meanwhile cannot exhaust memory.
///
/// # Errors
///
/// The file cannot be opened, is a symlink, is not a regular file, is larger
/// than `max_bytes`, or is not valid UTF-8.
pub fn read_text_capped(path: &Path, max_bytes: u64) -> Result<String> {
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use nix::fcntl::OFlag;
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK).bits());
    }
    let file = opts
        .open(path)
        .with_context(|| format!("opening {} (symlinks are not followed)", path.display()))?;
    let meta = file
        .metadata()
        .with_context(|| format!("reading the metadata of {}", path.display()))?;
    if !meta.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    if meta.len() > max_bytes {
        bail!(
            "{} is {} bytes, larger than the {max_bytes}-byte limit ([index] max_file_size)",
            path.display(),
            meta.len()
        );
    }
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {}", path.display()))?;
    if bytes.len() as u64 > max_bytes {
        bail!(
            "{} grew past the {max_bytes}-byte limit ([index] max_file_size) while being read",
            path.display()
        );
    }
    String::from_utf8(bytes).map_err(|_| anyhow!("{} is not valid UTF-8 text", path.display()))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readable_file_returns_true() {
        // Create a real temp file — the current process can definitely read it.
        let tmp = tempfile::NamedTempFile::new().expect("create temp file");
        assert!(
            can_read(tmp.path()),
            "should be able to read a just-created temp file"
        );
    }

    #[test]
    fn nonexistent_file_returns_false() {
        let path = std::path::Path::new("/tmp/vec_test_this_path_does_not_exist_12345678");
        assert!(!can_read(path), "non-existent path should return false");
    }

    #[test]
    fn normalize_lexically_resolves_dots() {
        assert_eq!(
            normalize_lexically(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
        assert_eq!(normalize_lexically(Path::new("/../x")), PathBuf::from("/x"));
    }

    #[test]
    fn absolute_lexical_resolves_relative_paths() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(
            absolute_lexical(Path::new("a/../b")).unwrap(),
            cwd.join("b")
        );
        assert_eq!(
            absolute_lexical(Path::new("/x/./y")).unwrap(),
            PathBuf::from("/x/y")
        );
    }

    #[test]
    fn read_text_capped_reads_small_text() {
        let dir = tempfile::TempDir::new().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, "héllo").unwrap();
        assert_eq!(read_text_capped(&f, 6).unwrap(), "héllo");
    }

    #[test]
    fn read_text_capped_refuses_large_binary_and_special_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let big = dir.path().join("big.txt");
        std::fs::write(&big, "0123456789").unwrap();
        let err = format!("{:#}", read_text_capped(&big, 9).unwrap_err());
        assert!(err.contains("limit"), "{err}");

        let bin = dir.path().join("bin");
        std::fs::write(&bin, [0xffu8, 0xfe]).unwrap();
        assert!(read_text_capped(&bin, 100).is_err());

        let err = format!("{:#}", read_text_capped(dir.path(), 100).unwrap_err());
        assert!(err.contains("not a regular file"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn read_text_capped_does_not_follow_symlinks_or_block_on_fifos() {
        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("t.txt");
        std::fs::write(&target, "secret").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(read_text_capped(&link, 100).is_err());

        let fifo = dir.path().join("fifo");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).unwrap();
        let err = format!("{:#}", read_text_capped(&fifo, 100).unwrap_err());
        assert!(err.contains("not a regular file"), "{err}");
    }

    #[test]
    fn directory_does_not_panic() {
        // Calling can_read on a directory must not panic.
        // The return value (true or false) is platform-defined and we just assert
        // the call completes without panicking.
        let dir = tempfile::TempDir::new().expect("create temp dir");
        let _result = can_read(dir.path()); // must not panic
    }
}
