//! `dense self update` / `dense self uninstall`.
//!
//! Update fetches the per-platform manifest from the release host (GitHub
//! releases unless the profile serves its own builds), compares versions,
//! downloads the new binary, verifies its sha256, and atomically replaces
//! the running executable. Uninstall removes shims, the env wiring, dense's
//! data dir, and the binary — leaving credentials in place.

use backon::{ExponentialBuilder, Retryable};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::api::Api;
use crate::config::Config;
use crate::error::{Context, Error};
use crate::{Result, daemon, desktop, env_file, persist, ui};

#[derive(Deserialize)]
struct Manifest {
    sha256: String,
    #[serde(default)]
    url: Option<String>,
    version: String,
}

/// The latest release when it is newer than this binary; `None` on any
/// failure or a non-semver (dev) manifest.
pub async fn newer(cfg: &Config) -> Option<String> {
    let api = Api::anonymous(cfg.release_base()).ok()?;
    let path = format!("/releases/latest/download/manifest-{}.json", platform());
    let manifest = api.get_json::<Manifest>(&path).await.ok()?;
    newer_than(env!("CARGO_PKG_VERSION"), &manifest.version)
}

pub async fn uninstall(cfg: &Config) -> Result<()> {
    ui::title("Uninstall");
    let mut complete = true;
    // The daemon first, or a managed one wraps the engines again behind this.
    if daemon::autostart::installed(cfg).is_some()
        && let Err(e) = daemon::remove_autostart(cfg)
    {
        complete = false;
        ui::warning(format!(
            "Could not remove background service autostart: {e}"
        ));
    }
    match desktop::restore(cfg, None) {
        Ok(report) if report.changed => ui::success("Claude Desktop's original engines restored"),
        Ok(_) => {}
        Err(e) => {
            complete = false;
            ui::warning(format!("Could not restore Claude Desktop's engines: {e}"));
        }
    }
    daemon::managed::report_uninstall(cfg).await;
    persist::unwire_editors(cfg);
    for name in persist::load_record(cfg).tools.keys() {
        let path = persist::shim_path(cfg, name);
        complete &= removed(std::fs::remove_file(&path), &path);
    }
    if let Err(e) = env_file::unwire(cfg) {
        complete = false;
        ui::warning(format!(
            "{e}; remove the `# >>> dense >>>` block from it by hand"
        ));
    }
    crate::harness::commands::cleanup(cfg);
    complete &= removed(std::fs::remove_dir_all(cfg.data_dir()), &cfg.data_dir());
    match std::env::current_exe() {
        Ok(exe) => complete &= removed(std::fs::remove_file(&exe), &exe),
        Err(e) => {
            complete = false;
            ui::warning(format!(
                "Could not locate the Dense binary to remove it: {e}"
            ));
        }
    }
    if complete {
        ui::success("Dense removed");
    } else {
        ui::warning("Uninstall finished with issues. Review the warnings above.");
    }
    ui::field(
        "Credentials",
        format!("Kept in {}", cfg.config_dir().display()),
    );
    Ok(())
}

pub async fn update(cfg: &Config) -> Result<()> {
    ui::title("Update");
    let progress = ui::progress("Checking for updates...");
    let platform = platform();
    let api = Api::anonymous(cfg.release_base())?;

    let manifest_path = format!("/releases/latest/download/manifest-{platform}.json");
    let manifest: Manifest = (|| api.get_json::<Manifest>(&manifest_path))
        .retry(ExponentialBuilder::default().with_max_times(2))
        .await?;
    drop(progress);

    let current = semver::Version::parse(env!("CARGO_PKG_VERSION")).ctx("own version")?;
    match semver::Version::parse(&manifest.version) {
        Ok(latest) if latest <= current => {
            ui::success(format!("Dense {current} is already up to date"));
            return Ok(());
        }
        Ok(latest) => ui::note(format!("Updating Dense {current} to {latest}...")),
        // A non-semver version (a dev broker's "dev") can't be ordered;
        // the manifest hash against the running binary decides instead.
        Err(_) => {
            if own_sha256()?.eq_ignore_ascii_case(&manifest.sha256) {
                ui::success(format!(
                    "Dense {current} is already up to date ({} build)",
                    manifest.version
                ));
                return Ok(());
            }
            ui::note(format!(
                "Updating Dense {current} to {}...",
                manifest.version
            ));
        }
    }

    let url = manifest
        .url
        .clone()
        .unwrap_or_else(|| format!("/releases/latest/download/{}", asset_name(&platform)));
    let progress = ui::progress("Downloading and verifying the update...");
    let bytes = api.get_bytes(&url).await?;

    let got = sha256_hex(&bytes);
    if !got.eq_ignore_ascii_case(&manifest.sha256) {
        return Err(Error::msg(format!(
            "sha256 mismatch: manifest {}, downloaded {got}",
            manifest.sha256
        )));
    }

    // Resolved first: linux reports the running binary as deleted once replaced.
    let exe = std::env::current_exe().ctx("locating the running binary")?;
    replace_self(&bytes)?;
    drop(progress);
    ui::success(format!("Updated to Dense {}", manifest.version));
    if desktop::has_wrapped_engines(cfg) {
        refresh_desktop_engines(&exe);
    }
    Ok(())
}

fn removed(result: std::io::Result<()>, path: &std::path::Path) -> bool {
    match result {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => {
            ui::warning(format!("Could not remove {}: {e}", path.display()));
            false
        }
    }
}

/// The new binary re-copies itself over Desktop's engines.
fn refresh_desktop_engines(exe: &std::path::Path) {
    let done = std::process::Command::new(exe)
        .args([desktop::COMMAND, "install", "--no-watch"])
        .stdout(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !done {
        ui::warning("Run `dense claude-desktop install` to update Desktop's engines");
    }
}

/// Release asset name for a platform key (e.g. `dense-linux-x86_64`).
fn asset_name(platform: &str) -> String {
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    format!("dense-{platform}{suffix}")
}

fn newer_than(current: &str, latest: &str) -> Option<String> {
    let current = semver::Version::parse(current).ok()?;
    let latest = semver::Version::parse(latest).ok()?;
    (latest > current).then(|| latest.to_string())
}

/// Digest of the running binary — what a non-semver manifest compares to.
fn own_sha256() -> Result<String> {
    let exe = std::env::current_exe().ctx("locating the running binary")?;
    let bytes = std::fs::read(&exe).ctx("reading the running binary")?;
    Ok(sha256_hex(&bytes))
}

/// `<os>-<arch>` — the platform key release assets and manifests are
/// published under (e.g. `linux-x86_64`, `macos-aarch64`).
fn platform() -> String {
    let os = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    };
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    };
    format!("{os}-{arch}")
}

fn replace_self(bytes: &[u8]) -> Result<()> {
    let exe = std::env::current_exe().ctx("locating the running binary")?;
    let dir = exe
        .parent()
        .ok_or_else(|| Error::msg("binary has no parent directory"))?;
    let tmp = dir.join(".dense.update");
    std::fs::write(&tmp, bytes).ctx("staging the new binary")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    // self-replace handles the running-binary swap on every platform,
    // including the Windows rename-and-delete-after-exit dance.
    let res = self_replace::self_replace(&tmp).ctx("installing the new binary");
    let _ = std::fs::remove_file(&tmp);
    res
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_than_only_reports_a_strictly_newer_semver() {
        assert_eq!(newer_than("0.14.0", "0.15.0").as_deref(), Some("0.15.0"));
        assert_eq!(newer_than("0.14.0", "0.14.0"), None);
        assert_eq!(newer_than("0.15.0", "0.14.0"), None);
        assert_eq!(newer_than("0.14.0", "dev"), None);
    }
}
