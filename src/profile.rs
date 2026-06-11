//! A profile is an environment descriptor: a name, the condense api it targets,
//! and whether that api gates auth. Only `prod` is baked into the binary
//! (zero-config default); every other environment is registered at install
//! time or by hand and cached per-profile — the binary hardcodes no other host.

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::config::{Config, DEFAULT_API_URL};
use crate::error::Error;
use crate::{api, hosts};

#[derive(Clone, Serialize, Deserialize)]
pub struct Profile {
    pub api_url: String,
    #[serde(default)]
    pub auth_required: Option<bool>,
    pub name: String,
    /// Where release assets come from; `None` means the baked GitHub
    /// releases default. Dev sets it to serve its own local builds.
    #[serde(default)]
    pub release_url: Option<String>,
}

impl Profile {
    /// The built-in default — condense prod, the only host baked into the binary.
    pub fn prod() -> Self {
        Self {
            name: "prod".to_string(),
            api_url: DEFAULT_API_URL.to_string(),
            auth_required: Some(true),
            release_url: None,
        }
    }
}

/// `dense profile` — print the active profile name.
pub fn current(cfg: &Config) {
    println!("{}", cfg.profile());
}

/// `dense profile -l` — list the registered profiles, marking the active one.
pub fn list(cfg: &Config) -> Result<()> {
    let active = cfg.profile().to_string();
    let mut names = vec!["prod".to_string()];
    names.extend(cfg.list_cached_profiles());
    names.sort();
    names.dedup();
    for name in names {
        let api = if name == "prod" {
            DEFAULT_API_URL.to_string()
        } else {
            cfg.load_cached_profile(&name)
                .map(|p| p.api_url)
                .unwrap_or_default()
        };
        let marker = if name == active { "*" } else { " " };
        println!("{marker} {name:<14} {api}");
    }
    Ok(())
}

/// `dense profile <name> [--url <zone|url>]` — register and switch to a
/// profile. `prod` with no source clears the target back to the baked default.
pub async fn switch(cfg: &Config, name: &str, source: Option<&str>) -> Result<()> {
    if name == "prod" {
        if source.is_some() {
            return Err(Error::Profile(
                "`prod` is baked into the binary and cannot be re-pointed".into(),
            ));
        }
        cfg.clear_target()?;
        println!("switched to prod (baked default).");
        return Ok(());
    }
    let profile = match source {
        Some(src) => from_source(name, src).await?,
        None => resolve_unsourced(cfg, name).await?,
    };
    cfg.save_cached_profile(&profile)?;
    cfg.write_target(name)?;
    println!("switched to profile `{name}` -> {}", profile.api_url);
    Ok(())
}

/// Build a profile from a user-supplied source. A full api URL (contains
/// `://`) is taken verbatim. A bare zone (`stage.condense.chat`) is expanded:
/// we try its `cli.<zone>/profile` for the authoritative descriptor and fall
/// back to `api.<zone>` if that host can't be reached.
async fn from_source(name: &str, source: &str) -> Result<Profile> {
    let source = source.trim();
    if source.contains("://") {
        return Ok(Profile {
            name: name.to_string(),
            api_url: source.trim_end_matches('/').to_string(),
            auth_required: None,
            release_url: None,
        });
    }
    let zone = source.trim_matches('/');
    let scheme = hosts::default_scheme_for(zone);
    match api::profile::fetch(&format!("{scheme}://cli.{zone}")).await {
        Ok(mut p) => {
            p.name = name.to_string();
            Ok(p)
        }
        Err(_) => Ok(Profile {
            name: name.to_string(),
            api_url: format!("{scheme}://api.{zone}"),
            auth_required: None,
            release_url: None,
        }),
    }
}

/// Switch to a profile given no source: a previously-registered one, or a
/// fetch from `$DENSE_PROFILE_URL`. The binary won't guess an unknown host.
async fn resolve_unsourced(cfg: &Config, name: &str) -> Result<Profile> {
    if let Some(p) = cfg.load_cached_profile(name) {
        return Ok(p);
    }
    let base = cfg.profile_url().ok_or_else(|| {
        Error::Profile(format!(
            "unknown profile `{name}` — give its zone or url \
             (`dense profile {name} --url stage.condense.chat`), or set DENSE_PROFILE_URL"
        ))
    })?;
    let mut p = api::profile::fetch(base).await?;
    p.name = name.to_string();
    Ok(p)
}
