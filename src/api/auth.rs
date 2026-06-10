//! Acquire and persist a condense identity.
//!
//! Two flows, picked by whether the api host requires Clerk auth:
//! - device-flow (`/v1/device/{start,poll}`) → a token stored at `token`;
//! - register (`/v1/register`) → a bypass UUID stored at `user`.
//!
//! Both write under the active profile's credential dir
//! (`~/.config/dense/` for prod, `~/.config/dense/<profile>/` otherwise).

use std::time::Duration;

use serde::Deserialize;

use crate::api::Api;
use crate::config::Config;
use crate::error::{Context, Error};
use crate::{Result, hosts};

const EXPIRES_FALLBACK_SECS: u64 = 600;
const POLL_FALLBACK_SECS: u64 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    Device,
    Register,
}

/// Stored credentials for the active api host.
pub struct Creds {
    pub token: Option<String>,
    pub user_id: Option<String>,
}

#[derive(Deserialize)]
struct DevicePoll {
    status: String,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    user_id: Option<String>,
}

#[derive(Deserialize)]
struct DeviceStart {
    device_code: String,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    interval: Option<u64>,
    user_code: String,
}

impl Creds {
    pub fn is_authenticated(&self) -> bool {
        self.token.is_some() || self.user_id.is_some()
    }
}

/// Ensure usable creds exist, logging in if absent or rejected. Returns the
/// creds the run path should forward.
pub async fn ensure_auth(cfg: &Config) -> Result<Creds> {
    let creds = load_creds(cfg);
    if let Some(token) = creds.token.as_deref() {
        match probe_token(cfg, token).await {
            401 | 403 => {
                tracing::warn!("stored token rejected; re-running login");
                login(cfg).await?;
            }
            _ => {}
        }
        return Ok(load_creds(cfg));
    }
    if creds.user_id.is_some() {
        return Ok(creds);
    }
    login(cfg).await?;
    Ok(load_creds(cfg))
}

pub fn load_creds(cfg: &Config) -> Creds {
    Creds {
        token: read_cred(&cfg.token_file()),
        user_id: read_cred(&cfg.user_file()),
    }
}

/// Run the login flow appropriate for the host and persist the result.
pub async fn login(cfg: &Config) -> Result<()> {
    match resolve_mode(&cfg.api_host, cfg.auth_required) {
        AuthMode::Device => device_flow(cfg).await,
        AuthMode::Register => register(cfg).await,
    }
}

pub fn logout(cfg: &Config) -> Result<()> {
    for path in [cfg.token_file(), cfg.user_file()] {
        match std::fs::remove_file(&path) {
            Ok(()) => tracing::info!("removed {}", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).ctx(format!("removing {}", path.display())),
        }
    }
    println!("logged out of {}", cfg.api_host);
    Ok(())
}

/// Probe `/v1/me` with the token via the same header the run path uses, so a
/// green probe is a real green light. Returns the HTTP status (0 on error).
/// The timeout is short — this runs on every launch, before the tool starts.
pub async fn probe_token(cfg: &Config, token: &str) -> u16 {
    let creds = Creds {
        token: Some(token.to_string()),
        user_id: None,
    };
    match Api::authed(cfg, &creds) {
        Ok(api) => api.status_of("/v1/me", Duration::from_secs(3)).await,
        Err(_) => 0,
    }
}

/// Whether the host runs the Clerk device-flow (`Device`) or the plain
/// `/register` UUID dance (`Register`). `auth_required` (resolved by [`Config`]
/// from `$CONDENSE_AUTH_REQUIRED` or the profile) wins; otherwise a host
/// heuristic — Tailscale zones bypass, everything else requires auth — mirroring
/// `auth_required_for_host` on the server.
pub fn resolve_mode(api_host: &str, auth_required: Option<bool>) -> AuthMode {
    if let Some(required) = auth_required {
        return mode_for(required);
    }
    if hosts::zone_of(api_host)
        .to_ascii_lowercase()
        .starts_with("ts.")
    {
        AuthMode::Register
    } else {
        AuthMode::Device
    }
}

async fn device_flow(cfg: &Config) -> Result<()> {
    let api = Api::anonymous(&cfg.api_base_url)?;
    eprintln!("starting authorization with {} ...", cfg.api_base_url);
    let start: DeviceStart = api
        .post_json("/v1/device/start", &serde_json::json!({}))
        .await?;

    let scheme = hosts::default_scheme_for(&cfg.api_host);
    let login_base = hosts::sibling(&cfg.api_host, "login", scheme);
    let link = format!("{login_base}/cli?code={}", start.user_code);
    eprintln!(
        "\nOpen this URL in your browser to authorise this terminal:\n\n  {link}\n\nCode: {}\n",
        start.user_code
    );
    maybe_open(cfg, &link);

    let spinner = cliclack::spinner();
    spinner.start("waiting for browser authorization...");
    let result = poll_device(cfg, &api, &start).await;
    match &result {
        Ok(()) => spinner.stop("authorized."),
        Err(e) => spinner.error(format!("{e}")),
    }
    result
}

fn maybe_open(cfg: &Config, url: &str) {
    if cfg.open_links() {
        let _ = webbrowser::open(url);
    }
}

fn mode_for(auth_required: bool) -> AuthMode {
    if auth_required {
        AuthMode::Device
    } else {
        AuthMode::Register
    }
}

/// Poll `/v1/device/poll` until the code is consumed, expired, or rejected.
async fn poll_device(cfg: &Config, api: &Api, start: &DeviceStart) -> Result<()> {
    let interval = Duration::from_secs(start.interval.unwrap_or(POLL_FALLBACK_SECS).max(1));
    let mut remaining = start.expires_in.unwrap_or(EXPIRES_FALLBACK_SECS);
    loop {
        tokio::time::sleep(interval).await;
        remaining = remaining.saturating_sub(interval.as_secs());
        if remaining == 0 {
            return Err(Error::Auth("authorization timed out".into()));
        }
        let resp = api
            .post_response(
                "/v1/device/poll",
                &serde_json::json!({ "device_code": start.device_code }),
            )
            .await;
        // 4xx (except 429) means the server will never accept this code —
        // fail now instead of polling out the clock. 5xx/network: retry.
        let poll: DevicePoll = match resp {
            Ok(r) if r.status().is_client_error() && r.status().as_u16() != 429 => {
                return Err(Error::Auth(format!(
                    "device/poll rejected ({})",
                    r.status()
                )));
            }
            Ok(r) if !r.status().is_success() => continue,
            Ok(r) => match r.json().await {
                Ok(p) => p,
                Err(e) => return Err(e).ctx("device/poll returned malformed JSON"),
            },
            Err(_) => continue,
        };
        match poll.status.as_str() {
            "consumed" => {
                let token = poll
                    .token
                    .filter(|t| !t.is_empty())
                    .ok_or_else(|| Error::Auth("device/poll consumed without a token".into()))?;
                write_secret(&cfg.token_file(), &token)?;
                if let Some(uid) = poll.user_id.filter(|u| !u.is_empty()) {
                    write_secret(&cfg.user_file(), &uid)?;
                }
                return Ok(());
            }
            "expired" => {
                return Err(Error::Auth(
                    "device code expired before authorization".into(),
                ));
            }
            _ => {}
        }
    }
}

fn read_cred(path: &std::path::Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

async fn register(cfg: &Config) -> Result<()> {
    let api = Api::anonymous(&cfg.api_base_url)?;
    eprintln!("registering with {} ...", cfg.api_base_url);
    let body = api.post_text("/v1/register").await?;
    let uuid = body.trim();
    if uuid.is_empty() {
        return Err(Error::Auth("register returned an empty user id".into()));
    }
    write_secret(&cfg.user_file(), uuid)?;
    eprintln!("registered.");
    Ok(())
}

fn write_secret(path: &std::path::Path, value: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ctx(format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, format!("{value}\n")).ctx(format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_auth_required_wins() {
        assert_eq!(
            resolve_mode("api.ts.condense.chat", Some(true)),
            AuthMode::Device
        );
        assert_eq!(
            resolve_mode("api.condense.chat", Some(false)),
            AuthMode::Register
        );
    }

    #[test]
    fn host_heuristic_when_unknown() {
        assert_eq!(resolve_mode("api.condense.chat", None), AuthMode::Device);
        assert_eq!(
            resolve_mode("api.ts.condense.chat", None),
            AuthMode::Register
        );
    }
}
