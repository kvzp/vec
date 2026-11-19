//! `vec init`, `vec config show|check`, `vec model download`.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use vec_core::config::Config;
use vec_core::init;

use super::load_config;

/// `vec init [--user]`: print a starter configuration generated from the
/// compiled-in defaults.
pub fn init(user: bool) -> Result<()> {
    let text = if user {
        for bin in init::system_binaries()? {
            if bin.exists() {
                eprintln!(
                    "warning: a system-wide vec is installed ({}). A userland install is only \
                     needed without one; PATH order decides which binary runs.",
                    bin.display()
                );
                break;
            }
        }
        init::user_template(&init::UserPaths::current()?)
    } else {
        init::system_template()?
    };
    init::check(&text)?;
    print!("{text}");
    Ok(())
}

/// `vec config show`: every effective key with the layer(s) that set it.
pub fn config_show(cfg_path: Option<&Path>) -> Result<()> {
    let cfg = Config::load_unvalidated(cfg_path).context("loading config")?;
    let layers: Vec<String> = cfg.layers().iter().map(ToString::to_string).collect();
    println!("# layers, lowest priority first: {}", layers.join("; "));
    let mut section = "";
    for origin in cfg.provenance() {
        let (sec, key) = origin.key.split_once('.').unwrap_or(("", &origin.key));
        if sec != section {
            println!("\n[{sec}]");
            section = sec;
        }
        let set_by: Vec<String> = origin.set_by.iter().map(ToString::to_string).collect();
        println!("{key} = {}    # {}", origin.value, set_by.join(" + "));
    }
    if let Err(e) = cfg.validate() {
        eprintln!("warning: {e:#}");
    }
    Ok(())
}

/// `vec config check [FILE]`: validate FILE alone over the defaults, or the
/// effective configuration. An error exit when invalid.
pub fn config_check(file: Option<&Path>, cfg_path: Option<&Path>) -> Result<()> {
    let cfg = match file {
        Some(f) => {
            let cfg = Config::from_file(f)?;
            for w in cfg.warnings() {
                eprintln!("warning: {w}");
            }
            cfg
        }
        None => load_config(cfg_path)?,
    };
    let layers: Vec<String> = cfg.layers().iter().map(ToString::to_string).collect();
    println!("OK: {}", layers.join("; "));
    Ok(())
}

/// Quote `s` for a POSIX shell.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `vec model download`: print the commands that fetch the configured
/// model's files into the first model directory. Downloads nothing.
pub fn model_download(cfg_path: Option<&Path>) -> Result<()> {
    let cfg = load_config(cfg_path)?;
    let e = &cfg.embed;
    if Path::new(&e.model).is_absolute() {
        bail!(
            "[embed] model is a file path ({}): put the model and its tokenizer.json there \
             yourself, or set [embed] model to a short name",
            e.model
        );
    }
    let dest_root = e
        .model_search_path
        .first()
        .ok_or_else(|| anyhow!("[embed] model_search_path is empty: nowhere to put the model"))?;
    let model_dir = dest_root.join(&e.model);
    let dir = model_dir.display().to_string();

    println!(
        "# Files of model {:?} from {}:",
        e.model, e.model_download_url
    );
    println!("mkdir -p {}", shell_quote(&dir));
    for remote in &e.model_download_files {
        let name = remote.rsplit('/').next().unwrap_or(remote);
        let url = format!("{}/{remote}", e.model_download_url.trim_end_matches('/'));
        let dest = model_dir.join(name).display().to_string();
        println!("curl -fL -o {} {}", shell_quote(&dest), shell_quote(&url));
    }
    println!();
    println!("# Then build the index: vec updatedb");
    let fetched: Vec<&str> = e
        .model_download_files
        .iter()
        .map(|f| f.rsplit('/').next().unwrap_or(f))
        .collect();
    if !e.model_files.iter().any(|f| fetched.contains(&f.as_str())) {
        eprintln!(
            "warning: none of the downloaded files is in [embed] model_files ({}); vec will \
             not find the model",
            e.model_files.join(", ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_survives_quotes() {
        assert_eq!(shell_quote("/a b/it's"), r"'/a b/it'\''s'");
    }

    #[test]
    fn config_check_rejects_a_bad_file() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("bad.toml");
        std::fs::write(&f, "[search]\ndefault_limit = 0\n").unwrap();
        let err = format!("{:#}", config_check(Some(&f), None).unwrap_err());
        assert!(
            err.contains("default_limit") && err.contains("bad.toml"),
            "{err}"
        );
        std::fs::write(&f, "[serch]\nx = 1\n").unwrap();
        assert!(config_check(Some(&f), None).is_err());
        std::fs::write(&f, "[search]\ndefault_limit = 5\n").unwrap();
        config_check(Some(&f), None).unwrap();
    }

    #[test]
    fn model_download_refuses_an_empty_search_path() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("c.toml");
        std::fs::write(&f, "[embed]\nmodel_search_path = []\n").unwrap();
        // Validation rejects it before the command could index into it.
        assert!(model_download(Some(&f)).is_err());
    }
}
