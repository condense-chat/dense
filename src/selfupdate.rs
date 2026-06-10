//! `dense self update` / `dense self uninstall`.
//!
//! Update fetches the per-platform manifest from the cli host, compares
//! versions, downloads the new binary, verifies its sha256, and atomically
//! replaces the running executable. Uninstall removes shims, the env wiring,
//! dense's data dir, and the binary — leaving credentials in place.

use backon::{ExponentialBuilder, Retryable};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::api::Api;
use crate::config::Config;
use crate::error::{Context, Error};
use crate::{Result, env_file, hosts, persist};

#[derive(Deserialize)]
struct Manifest {
    sha256: String,
    #[serde(default)]
    url: Option<String>,
    version: String,
}

pub fn uninstall(cfg: &Config) -> Result<()> {
    for name in persist::load_record(cfg).tools.keys() {
        let _ = std::fs::remove_file(persist::shim_path(cfg, name));
    }
    env_file::unwire(cfg)?;
    let _ = std::fs::remove_dir_all(cfg.data_dir());
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(&exe);
    }
    println!(
        "dense removed. Credentials under {} were kept.",
        cfg.config_dir().display()
    );
    Ok(())
}

pub async fn update(cfg: &Config) -> Result<()> {
    let platform = platform();
    let api = Api::anonymous(&cli_base(cfg))?;

    let manifest_path = format!("/{platform}/dense/manifest.json");
    let manifest: Manifest = (|| api.get_json::<Manifest>(&manifest_path))
        .retry(ExponentialBuilder::default().with_max_times(2))
        .await?;

    let current = semver::Version::parse(env!("CARGO_PKG_VERSION")).ctx("own version")?;
    let latest = semver::Version::parse(&manifest.version).ctx("manifest version")?;
    if latest <= current {
        println!("dense {current} is already up to date.");
        return Ok(());
    }
    println!("updating dense {current} -> {latest} ...");

    let url = manifest
        .url
        .clone()
        .unwrap_or_else(|| format!("/{platform}/dense/{}", manifest.version));
    let bytes = api.get_bytes(&url).await?;

    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let got = hex::encode(hasher.finalize());
    if !got.eq_ignore_ascii_case(&manifest.sha256) {
        return Err(Error::msg(format!(
            "sha256 mismatch: manifest {}, downloaded {got}",
            manifest.sha256
        )));
    }

    replace_self(&bytes)?;
    println!("updated to {latest}.");
    Ok(())
}

fn cli_base(cfg: &Config) -> String {
    let scheme = hosts::default_scheme_for(&cfg.api_host);
    hosts::sibling(&cfg.api_host, "cli", scheme)
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
