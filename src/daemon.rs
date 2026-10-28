//! `dense daemon` — the headless device daemon. Each tick heartbeats
//! `POST /v1/devices/heartbeat`, applies the returned switch ([`switch`]),
//! and drains the offline usage [`queue`]. The last known switch is kept in
//! `state.toml`, so a restart while offline keeps enforcing it.

pub mod autostart;
pub mod queue;
pub mod switch;

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::api::Api;
use crate::api::auth;
use crate::api::devices::{self, Heartbeat, HeartbeatReply};
use crate::config::Config;
use crate::error::Context;
use crate::{Result, device};

const BACKOFF_MAX: Duration = Duration::from_secs(300);
const BACKOFF_MIN: Duration = Duration::from_secs(5);
const INTERVAL_MAX_S: u64 = 3_600;
const INTERVAL_MIN_S: u64 = 5;
/// After a switch change, re-heartbeat this soon so the new mode is reported.
const REPORT_DELAY: Duration = Duration::from_secs(1);

/// Doubling retry delay for failed heartbeats, `min..=max`.
pub struct Backoff {
    current: Duration,
    max: Duration,
    min: Duration,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct State {
    pub device_enabled: Option<bool>,
    /// The last switch the server returned — what an offline restart applies.
    pub enabled: Option<bool>,
    pub interval_s: Option<u64>,
    pub last_attempt_at: Option<u64>,
    pub last_error: Option<String>,
    pub last_ok_at: Option<u64>,
    pub member_enabled: Option<bool>,
    pub mode: Option<switch::Mode>,
    pub pid: Option<u32>,
    /// Tools the switch manages (restored when it flips back on).
    #[serde(default)]
    pub tools: Vec<String>,
}

impl Backoff {
    pub fn new(min: Duration, max: Duration) -> Self {
        Self {
            current: min,
            max,
            min,
        }
    }

    pub fn next_delay(&mut self) -> Duration {
        let delay = self.current;
        self.current = (self.current * 2).min(self.max);
        delay
    }

    pub fn reset(&mut self) {
        self.current = self.min;
    }
}

impl State {
    pub fn load(cfg: &Config) -> Self {
        std::fs::read_to_string(state_file(cfg))
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, cfg: &Config) -> Result<()> {
        std::fs::create_dir_all(cfg.daemon_dir()).ctx("creating dense daemon dir")?;
        let body = toml::to_string_pretty(self).ctx("serializing daemon state")?;
        let path = state_file(cfg);
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, body).ctx(format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).ctx(format!("replacing {}", path.display()))
    }

    fn record(&mut self, reply: &HeartbeatReply) {
        self.enabled = Some(reply.enabled);
        self.member_enabled = reply.member_enabled;
        self.device_enabled = reply.device_enabled;
        self.interval_s = Some(reply.heartbeat_interval_s);
        self.last_ok_at = Some(now());
        self.last_error = None;
    }
}

pub async fn install(cfg: &Config) -> Result<()> {
    let device_id = device::id(cfg)?;
    let report = autostart::install(cfg)?;
    println!("daemon autostart installed -> {}", report.path.display());
    println!("device {device_id}");
    for note in &report.notes {
        eprintln!("warning: {note}");
    }
    Ok(())
}

/// The daemon loop. `once` runs a single tick and exits (scripts, QA).
pub async fn run(cfg: &Config, once: bool) -> Result<()> {
    let device_id = device::id(cfg)?;
    let mut state = State::load(cfg);
    state.pid = Some(std::process::id());
    tracing::info!(device = %device_id, profile = cfg.profile(), "dense daemon started");
    if let Some(enabled) = state.enabled {
        tracing::info!(enabled, "applying the last known switch");
        apply(cfg, &mut state, enabled);
    }
    save(cfg, &state);

    let mut backoff = Backoff::new(BACKOFF_MIN, BACKOFF_MAX);
    loop {
        let delay = tick(cfg, &device_id, &mut state, &mut backoff).await;
        save(cfg, &state);
        if once {
            return Ok(());
        }
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("dense daemon stopping");
                return Ok(());
            }
        }
    }
}

/// `dense status` lines for the device + daemon.
pub fn status_lines(cfg: &Config) -> Vec<(&'static str, String)> {
    let state = State::load(cfg);
    let device = device::load(cfg).unwrap_or_else(|| "none yet".into());
    let autostart = match autostart::installed(cfg) {
        Some(p) => format!("autostart installed ({})", p.display()),
        None => "autostart not installed".into(),
    };
    let beat = match (state.last_ok_at, &state.last_error) {
        (_, Some(err)) => format!("last heartbeat failed: {err}"),
        (Some(at), None) => format!("last heartbeat ok {}s ago", now().saturating_sub(at)),
        (None, None) => "never heartbeated".into(),
    };
    let switch = match state.enabled {
        Some(on) => format!(
            "{} (member {}, device {})",
            if on { "enabled" } else { "disabled" },
            flag(state.member_enabled),
            flag(state.device_enabled)
        ),
        None => "unknown (no heartbeat yet)".into(),
    };
    vec![
        ("device", device),
        ("daemon", format!("{autostart}; {beat}")),
        ("switch", switch),
        ("mode", switch::observe(cfg).as_str().to_string()),
        (
            "queue",
            format!("{} usage records pending", queue::Queue::open(cfg).len()),
        ),
    ]
}

pub async fn uninstall(cfg: &Config) -> Result<()> {
    let state = State::load(cfg);
    match autostart::uninstall(cfg, state.pid)? {
        Some(report) => {
            println!("daemon autostart removed ({})", report.path.display());
            for note in &report.notes {
                eprintln!("warning: {note}");
            }
        }
        None => println!("daemon autostart was not installed"),
    }
    let Some(device_id) = device::load(cfg) else {
        return Ok(());
    };
    let creds = auth::load_creds(cfg);
    if !creds.is_authenticated() {
        eprintln!("warning: not logged in; the server still lists this device as installed");
        return Ok(());
    }
    let api = Api::authed(cfg, &creds)?;
    match devices::uninstall(&api, &device_id).await {
        Ok(()) => println!("device {device_id} marked uninstalled"),
        Err(e) => eprintln!(
            "warning: could not report the uninstall ({})",
            error_chain(&e)
        ),
    }
    Ok(())
}

/// Apply `enabled`, logging (never propagating) a failure.
fn apply(cfg: &Config, state: &mut State, enabled: bool) -> bool {
    match switch::apply(cfg, &mut state.tools, enabled, true) {
        Ok(outcome) => {
            if outcome.changed {
                tracing::info!(enabled, mode = outcome.mode.as_str(), "switch applied");
            }
            state.mode = Some(outcome.mode);
            outcome.changed
        }
        Err(e) => {
            tracing::warn!("could not apply switch (enabled={enabled}): {e}");
            state.mode = Some(switch::observe(cfg));
            false
        }
    }
}

fn clamp_interval(secs: u64) -> Duration {
    Duration::from_secs(secs.clamp(INTERVAL_MIN_S, INTERVAL_MAX_S))
}

fn flag(v: Option<bool>) -> &'static str {
    match v {
        Some(true) => "on",
        Some(false) => "off",
        None => "?",
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn save(cfg: &Config, state: &State) {
    if let Err(e) = state.save(cfg) {
        tracing::warn!("could not save daemon state: {e}");
    }
}

fn state_file(cfg: &Config) -> PathBuf {
    cfg.daemon_dir().join("state.toml")
}

/// One heartbeat + apply + flush. Returns how long to sleep before the next.
async fn tick(cfg: &Config, device_id: &str, state: &mut State, backoff: &mut Backoff) -> Duration {
    state.last_attempt_at = Some(now());
    let creds = auth::load_creds(cfg);
    if !creds.is_authenticated() {
        state.last_error = Some("not logged in (run `dense login`)".into());
        return backoff.next_delay();
    }
    let api = match Api::authed(cfg, &creds) {
        Ok(api) => api,
        Err(e) => {
            state.last_error = Some(e.to_string());
            return backoff.next_delay();
        }
    };
    let hostname = device::hostname();
    let body = Heartbeat {
        app_version: env!("CARGO_PKG_VERSION"),
        device_id,
        hostname: &hostname,
        mode: Some(switch::observe(cfg)),
        platform: device::platform(),
    };
    let reply = match devices::heartbeat(&api, &body).await {
        Ok(reply) => reply,
        Err(e) => {
            let delay = backoff.next_delay();
            tracing::warn!("heartbeat failed, retrying in {}s: {e}", delay.as_secs());
            state.last_error = Some(error_chain(&e));
            return delay;
        }
    };
    backoff.reset();
    state.record(&reply);
    let changed = apply(cfg, state, reply.enabled);

    let queue = queue::Queue::open(cfg);
    match queue::flush_to(&api, device_id, &queue).await {
        Ok(0) => {}
        Ok(n) => tracing::info!("uploaded {n} queued usage records"),
        Err(e) => tracing::warn!("usage upload failed, kept queued: {e}"),
    }

    if changed {
        REPORT_DELAY
    } else {
        clamp_interval(reply.heartbeat_interval_s)
    }
}

fn error_chain(e: &crate::error::Error) -> String {
    let mut out = e.to_string();
    let mut src = std::error::Error::source(e);
    while let Some(s) = src {
        out.push_str(": ");
        out.push_str(&s.to_string());
        src = s.source();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_cap_and_resets() {
        let mut b = Backoff::new(Duration::from_secs(5), Duration::from_secs(30));
        let got: Vec<u64> = (0..6).map(|_| b.next_delay().as_secs()).collect();
        assert_eq!(got, vec![5, 10, 20, 30, 30, 30]);
        b.reset();
        assert_eq!(b.next_delay().as_secs(), 5);
    }

    #[test]
    fn interval_is_clamped() {
        assert_eq!(clamp_interval(0).as_secs(), INTERVAL_MIN_S);
        assert_eq!(clamp_interval(60).as_secs(), 60);
        assert_eq!(clamp_interval(1_000_000).as_secs(), INTERVAL_MAX_S);
    }

    #[test]
    fn state_round_trips_last_switch() {
        let root = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(root.path(), "http://127.0.0.1:9");
        let mut s = State::default();
        s.record(&HeartbeatReply {
            device_enabled: Some(false),
            enabled: false,
            heartbeat_interval_s: 30,
            member_enabled: Some(true),
        });
        s.tools = vec!["claude".into()];
        s.save(&cfg).unwrap();
        let back = State::load(&cfg);
        assert_eq!(back.enabled, Some(false));
        assert_eq!(back.device_enabled, Some(false));
        assert_eq!(back.tools, vec!["claude".to_string()]);
    }

    #[tokio::test]
    async fn offline_tick_backs_off_and_keeps_last_switch() {
        let root = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(root.path(), "http://127.0.0.1:9");
        std::fs::create_dir_all(cfg.cred_dir()).unwrap();
        std::fs::write(cfg.user_file(), "u\n").unwrap();
        let mut state = State {
            enabled: Some(false),
            ..State::default()
        };
        let mut b = Backoff::new(Duration::from_secs(5), Duration::from_secs(300));
        let first = tick(&cfg, "d", &mut state, &mut b).await;
        let second = tick(&cfg, "d", &mut state, &mut b).await;
        assert_eq!((first.as_secs(), second.as_secs()), (5, 10));
        assert_eq!(state.enabled, Some(false));
        assert!(state.last_error.is_some());
    }
}
