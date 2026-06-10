//! Resolve a real upstream tool binary on PATH, skipping dense's own shim
//! directory so a persisted shim never resolves to itself.

use std::path::PathBuf;

use crate::Result;
use crate::config::Config;
use crate::error::{Context, Error};

pub fn resolve_real(cfg: &Config, name: &str) -> Result<PathBuf> {
    let shim = cfg.shim_dir();
    let path = std::env::var_os("PATH").ok_or_else(|| Error::msg("PATH is not set"))?;
    let without_shims = std::env::join_paths(std::env::split_paths(&path).filter(|d| *d != shim))
        .ctx("rebuilding PATH without the shim dir")?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    which::which_in(name, Some(without_shims), cwd).map_err(|_| {
        Error::Tool(format!(
            "`{name}` was not found on PATH.\nInstall it and re-run — see https://docs.claude.com/en/docs/claude-code"
        ))
    })
}
