//! One-shot move of a pre-XDG macOS install out of
//! `~/Library/Application Support/dense` into the unix dirs, re-wiring the
//! PATH env file the shell profiles source.

use crate::Result;
use crate::config::Config;

#[cfg(target_os = "macos")]
pub fn run(cfg: &Config) -> Result<()> {
    use crate::env_file;

    let old = cfg
        .home()
        .join("Library")
        .join("Application Support")
        .join("dense");
    if !old.is_dir() {
        return Ok(());
    }
    // Re-add profile blocks only where they already were — `--no-modify-path`
    // users keep their profiles untouched; the env file is refreshed either way.
    let rewire = env_file::is_wired(cfg);
    move_entries(&old, cfg.config_dir(), &cfg.data_dir())?;
    if rewire {
        env_file::unwire(cfg)?;
    }
    if let env_file::PathWiring::Manual(notes) = env_file::ensure_env(cfg, rewire)? {
        for note in notes {
            eprintln!("note: {note}");
        }
    }
    println!(
        "moved dense files from {} to {} and {}",
        old.display(),
        cfg.config_dir().display(),
        cfg.data_dir().display()
    );
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn run(_cfg: &Config) -> Result<()> {
    Ok(())
}

/// Sort the old flat dir (config and data shared it) into the two new homes,
/// skipping anything the new dirs already hold. The old dir is removed only
/// once it is empty.
#[cfg(any(target_os = "macos", test))]
fn move_entries(
    old: &std::path::Path,
    config_dir: &std::path::Path,
    data_dir: &std::path::Path,
) -> Result<()> {
    use crate::error::Context;

    const DATA_ENTRIES: &[&str] = &["bin", "env", "persist.toml"];
    std::fs::create_dir_all(config_dir).ctx("creating dense config dir")?;
    std::fs::create_dir_all(data_dir).ctx("creating dense data dir")?;
    for entry in std::fs::read_dir(old).ctx("reading the old dense dir")? {
        let entry = entry.ctx("reading the old dense dir")?;
        let name = entry.file_name();
        let is_data = name.to_str().is_some_and(|n| DATA_ENTRIES.contains(&n));
        let dest = if is_data { data_dir } else { config_dir }.join(&name);
        if dest.exists() {
            continue;
        }
        std::fs::rename(entry.path(), &dest).ctx(format!(
            "moving {} to {}",
            entry.path().display(),
            dest.display()
        ))?;
    }
    let _ = std::fs::remove_dir(old);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorts_entries_and_removes_emptied_old_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let old = tmp.path().join("Application Support/dense");
        std::fs::create_dir_all(old.join("bin")).expect("mkdir");
        std::fs::create_dir_all(old.join("stage")).expect("mkdir");
        for (path, body) in [
            ("token", "tok"),
            ("target", "stage\n"),
            ("stage/profile.toml", "name = \"stage\"\n"),
            ("persist.toml", "[tools]\n"),
            ("env", "#!/bin/sh\n"),
            ("bin/claude", "#!/bin/sh\n"),
        ] {
            std::fs::write(old.join(path), body).expect("seed");
        }

        let config_dir = tmp.path().join(".config/dense");
        let data_dir = tmp.path().join(".local/share/dense");
        move_entries(&old, &config_dir, &data_dir).expect("migrate");

        for p in ["token", "target", "stage/profile.toml"] {
            assert!(config_dir.join(p).is_file(), "missing config entry {p}");
        }
        for p in ["persist.toml", "env", "bin/claude"] {
            assert!(data_dir.join(p).is_file(), "missing data entry {p}");
        }
        assert!(!old.exists());
    }

    #[test]
    fn keeps_existing_destination_entries() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let old = tmp.path().join("dense-old");
        std::fs::create_dir_all(&old).expect("mkdir");
        std::fs::write(old.join("token"), "old").expect("seed");

        let config_dir = tmp.path().join("dense-config");
        std::fs::create_dir_all(&config_dir).expect("mkdir");
        std::fs::write(config_dir.join("token"), "new").expect("seed");

        let data_dir = tmp.path().join("dense-data");
        move_entries(&old, &config_dir, &data_dir).expect("migrate");

        assert_eq!(
            std::fs::read_to_string(config_dir.join("token")).expect("read"),
            "new"
        );
        // The conflicting old entry stays put, so the old dir survives too.
        assert!(old.join("token").is_file());
    }
}
