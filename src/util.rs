//! util.rs — small cross-platform helpers.

/// Returns `true` if the current process has read access to `path`.
///
/// On Unix (Linux, macOS, BSDs) this calls the `access(R_OK)` syscall, which
/// tests the *effective* UID against the file's permission bits — correctly
/// honouring setuid/setgid without actually opening the file.
///
/// On non-Unix platforms (Windows) we fall back to a simple open-for-read
/// attempt: if `File::open` succeeds we have read access.
pub fn can_read(path: &std::path::Path) -> bool {
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
        assert!(can_read(tmp.path()), "should be able to read a just-created temp file");
    }

    #[test]
    fn nonexistent_file_returns_false() {
        let path = std::path::Path::new("/tmp/vec_test_this_path_does_not_exist_12345678");
        assert!(!can_read(path), "non-existent path should return false");
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
