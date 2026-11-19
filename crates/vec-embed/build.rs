//! Records the version of `tract-onnx` this crate is built against, for
//! [`Embedder::runtime`](crate::Embedder::runtime): tract exports no version
//! constant, so it is read from the workspace `Cargo.lock`. A build without a
//! lock file (e.g. from a published crate) records an empty string and the
//! runtime falls back to vec's own version.

use std::path::{Path, PathBuf};

/// Package whose resolved version names the ONNX runtime.
const RUNTIME_PACKAGE: &str = "tract-onnx";
/// Environment variable the version is passed to the crate in.
const VERSION_VAR: &str = "VEC_EMBED_TRACT_VERSION";

fn main() {
    let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from);
    let lock = manifest_dir.as_deref().and_then(|dir| {
        dir.ancestors()
            .map(|a| a.join("Cargo.lock"))
            .find(|p| p.is_file())
    });
    let version = match &lock {
        Some(lock) => {
            println!("cargo:rerun-if-changed={}", lock.display());
            locked_version(lock).unwrap_or_default()
        }
        None => {
            println!("cargo:rerun-if-changed=build.rs");
            String::new()
        }
    };
    println!("cargo:rustc-env={VERSION_VAR}={version}");
}

/// The version of [`RUNTIME_PACKAGE`] in a Cargo.lock (`[[package]]` entries
/// with `name = "..."` followed by `version = "..."`).
fn locked_version(lock: &Path) -> Option<String> {
    let text = std::fs::read_to_string(lock).ok()?;
    let wanted = format!("name = \"{RUNTIME_PACKAGE}\"");
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        if line.trim() == wanted {
            let version = lines.next()?.trim().strip_prefix("version = ")?;
            return Some(version.trim_matches('"').to_owned());
        }
    }
    None
}
