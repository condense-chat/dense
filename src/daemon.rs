//! `dense daemon` — the headless device daemon. Each tick heartbeats
//! `POST /v1/devices/heartbeat`, applies the returned switch ([`switch`]),
//! and drains the offline usage [`queue`]. The last known switch is kept in
//! `state.toml`, so a restart while offline keeps enforcing it. The switch is
//! only ever applied for a `managed` (Enterprise) device. One daemon per
//! machine: shims are shared by every profile, so the lock is too.

pub mod autostart;
pub mod queue;
pub mod routed;
pub mod sessions;
pub mod switch;

use std::fs::File;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::api::Api;
use crate::api::auth;
use crate::api::devices::{self, Heartbeat, HeartbeatReply, Reply};
use crate::config::Config;
use crate::error::{Context, Error};
use crate::{Result, device};

const DEVICE_LIMIT_MSG: &str =
    "device limit reached: remove old devices in the dashboard (this device stays unmanaged)";
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
    /// The server refused this device for the user's device cap.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub limited: bool,
    /// Whether the switch is ours to apply; unknown is treated as not.
    pub managed: Option<bool>,
    pub member_enabled: Option<bool>,
    pub mode: Option<switch::Mode>,
    pub pid: Option<u32>,
    /// When no condense routing was applied: the only spans whose
    /// session-file usage the daemon reports (the proxy reports the rest).
    #[serde(default)]
    pub transparent_windows: sessions::Windows,
    /// Hash of the credentials the windows and queue belong to.
    pub user: Option<String>,
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
        let Ok(raw) = std::fs::read_to_string(state_file(cfg)) else {
            return Self::default();
        };
        toml::from_str(&raw).unwrap_or_else(|e| {
            tracing::warn!("ignoring unreadable daemon state: {e}");
            Self::default()
        })
    }

    pub fn save(&self, cfg: &Config) -> Result<()> {
        std::fs::create_dir_all(cfg.daemon_dir()).ctx("creating dense daemon dir")?;
        let body = toml::to_string_pretty(self).ctx("serializing daemon state")?;
        let path = state_file(cfg);
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, body).ctx(format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).ctx(format!("replacing {}", path.display()))
    }

    /// The switch to enforce, if this device is managed and one is known.
    fn enforced(&self) -> Option<bool> {
        self.enabled.filter(|_| self.managed == Some(true))
    }

    fn forget_switch(&mut self) {
        self.enabled = None;
        self.managed = None;
        self.member_enabled = None;
        self.device_enabled = None;
    }

    fn observed(&mut self, mode: switch::Mode) {
        self.mode = Some(mode);
        self.transparent_windows.note(mode, now_ms());
    }

    fn record(&mut self, reply: &HeartbeatReply) {
        self.limited = false;
        self.managed = Some(reply.managed);
        self.enabled = Some(reply.enabled);
        self.member_enabled = reply.member_enabled;
        self.device_enabled = reply.device_enabled;
        self.interval_s = Some(reply.heartbeat_interval_s);
        self.last_ok_at = Some(now());
        self.last_error = None;
    }
}

pub async fn install(cfg: &Config) -> Result<()> {
    sessions::remember_root(cfg);
    let device_id = device::id(cfg)?;
    let report = autostart::install(cfg)?;
    println!("daemon autostart installed -> {}", report.path.display());
    println!("device {device_id}");
    for note in &report.notes {
        eprintln!("warning: {note}");
    }
    Ok(())
}

/// After `dense login`: if the account's org manages devices, install the
/// daemon so the switch applies without anyone running `dense daemon install`.
pub async fn after_login(cfg: &Config) {
    sessions::remember_root(cfg);
    if let Some(user) = auth::load_creds(cfg).user_id {
        match device::bind_user(cfg, &user) {
            Ok(true) => drop_usage(cfg),
            Ok(false) => {}
            Err(e) => tracing::warn!("could not bind the device id to this user: {e}"),
        }
    }
    if autostart::installed(cfg).is_some() {
        return;
    }
    let Ok(mut device_id) = device::id(cfg) else {
        return;
    };
    let mut state = State::load(cfg);
    match heartbeat(cfg, &mut device_id, &mut state).await {
        Ok(Some((reply, _))) if reply.managed => {}
        Ok(_) => {
            if state.limited {
                eprintln!("warning: {DEVICE_LIMIT_MSG}");
            }
            return;
        }
        Err(e) => {
            tracing::debug!("post-login heartbeat failed: {e}");
            return;
        }
    }
    save(cfg, &state);
    match autostart::install(cfg) {
        Ok(_) => {
            println!("this device is managed by your organization; the dense daemon is installed")
        }
        Err(e) => eprintln!(
            "warning: this device is managed by your organization, but installing the dense daemon failed ({}); run `dense daemon install`",
            error_chain(&e)
        ),
    }
}

/// Clear the cached switch (on logout): a later account must not inherit it,
/// nor this account's unsent usage.
pub fn forget_switch(cfg: &Config) {
    let mut state = State::load(cfg);
    state.forget_switch();
    state.transparent_windows = sessions::Windows::default();
    state.user = None;
    save(cfg, &state);
    drop_usage(cfg);
}

fn drop_usage(cfg: &Config) {
    if let Err(e) = queue::Queue::open(cfg).clear() {
        tracing::warn!("could not drop queued usage: {e}");
    }
}

/// The daemon loop. `once` runs a single tick (plus the follow-up report when
/// it changed the mode) and exits, leaving a running daemon's pid alone.
pub async fn run(cfg: &Config, once: bool) -> Result<()> {
    let Some(_lock) = lock(cfg)? else {
        let msg = "another dense daemon is already running";
        if once {
            return Err(Error::msg(msg));
        }
        // Exit cleanly so a supervisor's restart loop treats this as done.
        eprintln!("{msg}; exiting");
        return Ok(());
    };
    let mut device_id = device::id(cfg)?;
    sessions::remember_root(cfg);
    let mut state = State::load(cfg);
    if !once {
        state.pid = Some(std::process::id());
    }
    tracing::info!(device = %device_id, profile = cfg.profile(), "dense daemon started");
    if switch::observe(cfg) == switch::Mode::Condensing {
        state.transparent_windows.close(last_seen_ms(&state));
    }
    if let Some(enabled) = state.enforced() {
        tracing::info!(enabled, "applying the last known switch");
        apply(cfg, &mut state, enabled);
    }
    save(cfg, &state);

    let mut backoff = Backoff::new(BACKOFF_MIN, BACKOFF_MAX);
    let mut reported = false;
    loop {
        let delay = tick(cfg, &mut device_id, &mut state, &mut backoff).await;
        save(cfg, &state);
        if once && (reported || delay != REPORT_DELAY) {
            return Ok(());
        }
        reported = true;
        tokio::select! {
            () = tokio::time::sleep(jitter(delay)) => {}
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
    let running = match (running(cfg), state.pid) {
        (true, Some(pid)) => format!("running (pid {pid})"),
        (true, None) => "running".into(),
        (false, _) => "not running".into(),
    };
    let beat = match (state.last_ok_at, &state.last_error) {
        (_, Some(err)) => format!("last heartbeat failed: {err}"),
        (Some(at), None) => format!("last heartbeat ok {}s ago", now().saturating_sub(at)),
        (None, None) => "never heartbeated".into(),
    };
    let switch = match (state.managed, state.enabled) {
        _ if state.limited => DEVICE_LIMIT_MSG.into(),
        (Some(false), _) => "not managed (no org device management)".into(),
        (_, Some(on)) => format!(
            "{} (member {}, device {})",
            if on { "enabled" } else { "disabled" },
            flag(state.member_enabled),
            flag(state.device_enabled)
        ),
        (_, None) => "unknown (no heartbeat yet)".into(),
    };
    vec![
        ("device", device),
        ("daemon", format!("{running}; {autostart}; {beat}")),
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
    report_uninstall(cfg).await;
    Ok(())
}

/// Tell condense this device is gone (`POST /v1/devices/uninstall`); warns,
/// never fails, since the local uninstall must go through regardless.
pub async fn report_uninstall(cfg: &Config) {
    let Some(device_id) = device::load(cfg) else {
        return;
    };
    let creds = auth::load_creds(cfg);
    if !creds.is_authenticated() {
        eprintln!("warning: not logged in; the server still lists this device as installed");
        return;
    }
    let result = match Api::authed(cfg, &creds) {
        Ok(api) => devices::uninstall(&api, &device_id).await,
        Err(e) => Err(e),
    };
    match result {
        Ok(true) => println!("device {device_id} marked uninstalled"),
        Ok(false) => println!("device {device_id} was never registered; nothing to mark"),
        Err(e) => eprintln!(
            "warning: could not report the uninstall ({})",
            error_chain(&e)
        ),
    }
}

/// Apply `enabled`, logging (never propagating) a failure.
fn apply(cfg: &Config, state: &mut State, enabled: bool) -> bool {
    match switch::apply(cfg, enabled, &switch::installed(cfg), true) {
        Ok(outcome) => {
            if outcome.changed {
                tracing::info!(enabled, mode = outcome.mode.as_str(), "switch applied");
            }
            state.observed(outcome.mode);
            outcome.changed
        }
        Err(e) => {
            tracing::warn!("could not apply switch (enabled={enabled}): {e}");
            state.observed(switch::observe(cfg));
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

/// Up to +20% so a fleet that went offline together does not retry in lockstep.
fn jitter(d: Duration) -> Duration {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|t| t.subsec_nanos())
        .unwrap_or(0);
    d + d.mul_f64(f64::from(nanos % 1_000) / 5_000.0)
}

/// The machine-wide single-instance lock, held for the daemon's lifetime;
/// `None` when another daemon holds it.
fn lock(cfg: &Config) -> Result<Option<File>> {
    let path = lock_file(cfg);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ctx(format!("creating {}", parent.display()))?;
    }
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .ctx(format!("opening {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e).ctx(format!("locking {}", path.display())),
    }
}

/// When the daemon last ran a tick (ms) — the latest a mode change it missed can be dated.
fn last_seen_ms(state: &State) -> u64 {
    state.last_attempt_at.map_or_else(now_ms, |s| s * 1_000)
}

fn lock_file(cfg: &Config) -> PathBuf {
    cfg.data_dir().join("daemon").join("daemon.lock")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Queue session-file usage for the transparent windows. Runs before the
/// heartbeat so it works offline; the queue carries it to the next upload.
/// The window follows the shims actually on disk: unmanaged devices without
/// shims are transparent throughout (`dense claude` runs are excluded by
/// their routed spans instead).
fn report_offline_usage(cfg: &Config, state: &mut State) {
    let now = now_ms();
    let user = user_ref(&auth::load_creds(cfg));
    if state.user != user {
        // Nothing from before the change may be reported or uploaded as the new user.
        if state.user.is_some() {
            state.transparent_windows = sessions::Windows::default();
            drop_usage(cfg);
        }
        if user.is_none() {
            state.forget_switch();
        }
        state.user.clone_from(&user);
    }
    if user.is_none() {
        state.transparent_windows = sessions::Windows::default();
        return;
    }
    match switch::observe(cfg) {
        switch::Mode::Condensing => state.transparent_windows.close(last_seen_ms(state)),
        switch::Mode::Transparent => state
            .transparent_windows
            .note(switch::Mode::Transparent, now),
    }
    state.transparent_windows.prune(now);
    match sessions::report(cfg, &state.transparent_windows, now) {
        Ok(0) => {}
        Ok(n) => tracing::info!("queued {n} offline usage records from session files"),
        Err(e) => tracing::warn!("could not read session usage: {e}"),
    }
}

fn user_ref(creds: &auth::Creds) -> Option<String> {
    use sha2::{Digest, Sha256};
    let who = creds.user_id.as_deref().or(creds.token.as_deref())?;
    Some(hex::encode(&Sha256::digest(who.as_bytes())[..8]))
}

fn running(cfg: &Config) -> bool {
    let Ok(file) = File::options().write(true).open(lock_file(cfg)) else {
        return false;
    };
    matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock))
}

fn save(cfg: &Config, state: &State) {
    if let Err(e) = state.save(cfg) {
        tracing::warn!("could not save daemon state: {e}");
    }
}

fn state_file(cfg: &Config) -> PathBuf {
    cfg.daemon_dir().join("state.toml")
}

/// One heartbeat. `Ok(None)`: the server refused this credential/device for
/// good (4xx) — the cached switch is dropped so nothing stale is enforced.
/// Heartbeat once. A `device_owned_elsewhere` refusal swaps in a fresh device
/// id (or one another process already wrote) and retries once.
async fn heartbeat(
    cfg: &Config,
    device_id: &mut String,
    state: &mut State,
) -> Result<Option<(HeartbeatReply, Api)>> {
    state.last_attempt_at = Some(now());
    let creds = auth::load_creds(cfg);
    if !creds.is_authenticated() {
        return Err(Error::msg("not logged in (run `dense login`)"));
    }
    let mut retried = false;
    let rejection = loop {
        let api = Api::authed(cfg, &creds)?;
        match send_heartbeat(cfg, &api, device_id).await? {
            Reply::Ok(reply) => {
                state.record(&reply);
                return Ok(Some((reply, api)));
            }
            Reply::Rejected(r) if r.is(devices::DEVICE_OWNED_ELSEWHERE) && !retried => {
                retried = true;
                let fresh = match device::load(cfg).filter(|id| id != device_id) {
                    Some(id) => id,
                    None => device::regenerate(cfg)?,
                };
                tracing::warn!(old = %device_id, new = %fresh, "device id belongs to another user; switched to a new one");
                *device_id = fresh;
            }
            Reply::Rejected(r) => break r,
        }
    };
    if state.enforced().is_some() {
        tracing::warn!("heartbeat rejected ({rejection}); no longer enforcing the cached switch");
    }
    state.forget_switch();
    state.limited = rejection.is(devices::DEVICE_LIMIT);
    state.last_error = Some(if state.limited {
        DEVICE_LIMIT_MSG.to_string()
    } else if matches!(rejection.status, 401 | 403) {
        format!(
            "heartbeat rejected ({rejection}): credential not accepted for this device (run `dense login`)"
        )
    } else {
        format!("heartbeat rejected ({rejection})")
    });
    Ok(None)
}

async fn send_heartbeat(cfg: &Config, api: &Api, device_id: &str) -> Result<Reply<HeartbeatReply>> {
    let hostname = device::hostname();
    let body = Heartbeat {
        app_version: env!("CARGO_PKG_VERSION"),
        device_id,
        hostname: &hostname,
        mode: Some(switch::observe(cfg)),
        platform: device::platform(),
    };
    devices::heartbeat(api, &body).await
}

/// One heartbeat + apply + flush. Returns how long to sleep before the next.
async fn tick(
    cfg: &Config,
    device_id: &mut String,
    state: &mut State,
    backoff: &mut Backoff,
) -> Duration {
    report_offline_usage(cfg, state);
    let (reply, api) = match heartbeat(cfg, device_id, state).await {
        Ok(Some(ok)) => ok,
        Ok(None) => {
            if state.limited {
                tracing::warn!("{DEVICE_LIMIT_MSG}");
            }
            return backoff.next_delay();
        }
        Err(e) => {
            let delay = backoff.next_delay();
            tracing::warn!("heartbeat failed, retrying in {}s: {e}", delay.as_secs());
            state.last_error = Some(error_chain(&e));
            return delay;
        }
    };
    backoff.reset();
    let changed = match state.enforced() {
        Some(enabled) => apply(cfg, state, enabled),
        None => {
            state.observed(switch::observe(cfg));
            false
        }
    };

    let queue = queue::Queue::open(cfg);
    match queue::flush_to(&api, device_id, &queue).await {
        Ok(f) if f.stalled.is_some() => tracing::warn!(
            sent = f.sent,
            "usage upload refused for this device ({}); kept queued until the next heartbeat",
            f.stalled
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default()
        ),
        Ok(f) if f.dropped > 0 => tracing::warn!(
            sent = f.sent,
            dropped = f.dropped,
            "dropped usage records the server rejected"
        ),
        Ok(f) if f.sent > 0 => tracing::info!("uploaded {} queued usage records", f.sent),
        Ok(_) => {}
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

    const OK_OFF: &str = r#"{"managed":true,"enabled":false,"heartbeat_interval_s":60}"#;

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
            managed: true,
            member_enabled: Some(true),
        });
        s.transparent_windows.note(switch::Mode::Transparent, 1_000);
        s.transparent_windows.close(2_000);
        s.transparent_windows.note(switch::Mode::Transparent, 3_000);
        s.save(&cfg).unwrap();
        let back = State::load(&cfg);
        assert_eq!(back.enabled, Some(false));
        assert_eq!(back.device_enabled, Some(false));
        assert_eq!(back.enforced(), Some(false));
        assert_eq!(back.transparent_windows, s.transparent_windows);
        assert!(
            back.transparent_windows.contains(1_500) && !back.transparent_windows.contains(2_500)
        );
    }

    /// A one-route HTTP server: every request gets `status` + `body`; returns
    /// the base URL and the request lines it saw.
    fn serve(status: u16, body: &'static str) -> (String, std::sync::mpsc::Receiver<String>) {
        serve_seq(vec![(status, body)])
    }

    /// Like [`serve`], answering with `replies` in order (the last repeats);
    /// each seen request is reported as `<request line>\n<body>`.
    fn serve_seq(replies: Vec<(u16, &'static str)>) -> (String, std::sync::mpsc::Receiver<String>) {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for (n, stream) in listener.incoming().flatten().enumerate() {
                let (status, body) = replies[n.min(replies.len() - 1)];
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let mut len = 0;
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                    if h == "\r\n" {
                        break;
                    }
                }
                let mut buf = vec![0; len];
                reader.read_exact(&mut buf).unwrap();
                let _ = tx.send(format!(
                    "{}\n{}",
                    line.trim(),
                    String::from_utf8_lossy(&buf)
                ));
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = reader.get_mut().write_all(reply.as_bytes());
            }
        });
        (url, rx)
    }

    fn logged_in(root: &std::path::Path, url: &str) -> Config {
        let cfg = Config::for_test(root, url);
        std::fs::create_dir_all(cfg.cred_dir()).unwrap();
        std::fs::write(cfg.user_file(), "u\n").unwrap();
        cfg
    }

    fn shim(cfg: &Config) -> PathBuf {
        let path = crate::persist::shim_path(cfg, "claude");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        path
    }

    #[tokio::test]
    async fn managed_off_switch_unpersists() {
        let (url, rx) = serve(
            200,
            r#"{"managed":true,"enabled":false,"member_enabled":false,"device_enabled":true,"heartbeat_interval_s":42}"#,
        );
        let root = tempfile::tempdir().unwrap();
        let cfg = logged_in(root.path(), &url);
        let shim = shim(&cfg);
        let mut state = State::default();
        let mut b = Backoff::new(BACKOFF_MIN, BACKOFF_MAX);
        assert_eq!(
            tick(&cfg, &mut "d".to_string(), &mut state, &mut b).await,
            REPORT_DELAY
        );
        assert!(!shim.exists());
        assert_eq!(state.mode, Some(switch::Mode::Transparent));
        assert_eq!(state.enforced(), Some(false));
        assert!(
            rx.recv()
                .unwrap()
                .starts_with("POST /v1/devices/heartbeat HTTP/1.1\n")
        );
        assert_eq!(
            tick(&cfg, &mut "d".to_string(), &mut state, &mut b)
                .await
                .as_secs(),
            42
        );
    }

    #[tokio::test]
    async fn unmanaged_device_never_touches_shims() {
        let (url, _rx) = serve(
            200,
            r#"{"managed":false,"enabled":false,"heartbeat_interval_s":60}"#,
        );
        let root = tempfile::tempdir().unwrap();
        let cfg = logged_in(root.path(), &url);
        let shim = shim(&cfg);
        let mut state = State::default();
        let mut b = Backoff::new(BACKOFF_MIN, BACKOFF_MAX);
        assert_eq!(
            tick(&cfg, &mut "d".to_string(), &mut state, &mut b)
                .await
                .as_secs(),
            60
        );
        assert!(shim.exists());
        assert_eq!(state.enforced(), None);
        assert_eq!(state.mode, Some(switch::Mode::Condensing));
    }

    #[tokio::test]
    async fn missing_managed_field_is_unmanaged() {
        let (url, _rx) = serve(200, r#"{"enabled":false,"heartbeat_interval_s":60}"#);
        let root = tempfile::tempdir().unwrap();
        let cfg = logged_in(root.path(), &url);
        let shim = shim(&cfg);
        let mut state = State::default();
        let mut b = Backoff::new(BACKOFF_MIN, BACKOFF_MAX);
        tick(&cfg, &mut "d".to_string(), &mut state, &mut b).await;
        assert!(shim.exists());
    }

    #[tokio::test]
    async fn rejected_heartbeat_forgets_the_cached_switch() {
        let (url, _rx) = serve(403, r#"{"detail":"forbidden"}"#);
        let root = tempfile::tempdir().unwrap();
        let cfg = logged_in(root.path(), &url);
        let shim = shim(&cfg);
        let mut state = State {
            enabled: Some(false),
            managed: Some(true),
            ..State::default()
        };
        let mut b = Backoff::new(BACKOFF_MIN, BACKOFF_MAX);
        assert_eq!(
            tick(&cfg, &mut "d".to_string(), &mut state, &mut b).await,
            BACKOFF_MIN
        );
        assert_eq!(state.enforced(), None);
        assert!(shim.exists());
        assert!(state.last_error.unwrap().contains("403"));
    }

    #[tokio::test]
    async fn owned_elsewhere_regenerates_the_id_and_retries_once() {
        let (url, rx) = serve_seq(vec![
            (403, r#"{"detail":{"code":"device_owned_elsewhere"}}"#),
            (200, OK_OFF),
        ]);
        let root = tempfile::tempdir().unwrap();
        let cfg = logged_in(root.path(), &url);
        let mut id = device::id(&cfg).unwrap();
        let old = id.clone();
        let mut state = State::default();
        let mut b = Backoff::new(BACKOFF_MIN, BACKOFF_MAX);
        assert_eq!(tick(&cfg, &mut id, &mut state, &mut b).await.as_secs(), 60);
        assert_ne!(id, old);
        assert_eq!(device::load(&cfg).as_deref(), Some(id.as_str()));
        assert!(rx.recv().unwrap().contains(&old));
        assert!(rx.recv().unwrap().contains(&id));
        assert_eq!(state.enforced(), Some(false));
    }

    #[tokio::test]
    async fn owned_elsewhere_twice_is_a_plain_rejection() {
        let (url, rx) = serve(403, r#"{"detail":{"code":"device_owned_elsewhere"}}"#);
        let root = tempfile::tempdir().unwrap();
        let cfg = logged_in(root.path(), &url);
        let mut id = device::id(&cfg).unwrap();
        let mut state = State {
            enabled: Some(false),
            managed: Some(true),
            ..State::default()
        };
        let mut b = Backoff::new(BACKOFF_MIN, BACKOFF_MAX);
        assert_eq!(tick(&cfg, &mut id, &mut state, &mut b).await, BACKOFF_MIN);
        assert_eq!(rx.try_iter().count(), 2);
        assert_eq!(state.enforced(), None);
        assert!(state.last_error.unwrap().contains("device_owned_elsewhere"));
    }

    #[tokio::test]
    async fn device_limit_stays_unmanaged_and_says_why() {
        let (url, _rx) = serve(409, r#"{"detail":{"code":"device_limit"}}"#);
        let root = tempfile::tempdir().unwrap();
        let cfg = logged_in(root.path(), &url);
        let shim = shim(&cfg);
        let mut state = State {
            enabled: Some(false),
            managed: Some(true),
            ..State::default()
        };
        let mut b = Backoff::new(BACKOFF_MIN, BACKOFF_MAX);
        assert_eq!(
            tick(&cfg, &mut "d".to_string(), &mut state, &mut b).await,
            BACKOFF_MIN
        );
        assert!(shim.exists());
        assert!(state.limited && state.enforced().is_none());
        save(&cfg, &state);
        let lines = status_lines(&cfg);
        assert!(
            lines
                .iter()
                .any(|(k, v)| *k == "switch" && v.starts_with("device limit reached"))
        );
    }

    #[tokio::test]
    async fn unknown_device_on_usage_keeps_the_batch() {
        let (url, _rx) = serve_seq(vec![
            (200, OK_OFF),
            (404, r#"{"detail":{"code":"device_unknown"}}"#),
        ]);
        let root = tempfile::tempdir().unwrap();
        let cfg = logged_in(root.path(), &url);
        let q = queue::Queue::open(&cfg);
        let rec = queue::UsageRecord {
            cache_read: 0,
            cache_write: 0,
            input_tokens: 1,
            message_id: "m".into(),
            model: "x".into(),
            occurred_at: "2026-10-28T00:00:00Z".into(),
            output_tokens: 1,
            session_ref: "s".into(),
        };
        q.push(&[rec]).unwrap();
        let mut state = State::default();
        let mut b = Backoff::new(BACKOFF_MIN, BACKOFF_MAX);
        tick(&cfg, &mut "d".to_string(), &mut state, &mut b).await;
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn logout_forgets_switch_but_keeps_device() {
        let root = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(root.path(), "http://127.0.0.1:9");
        let id = device::id(&cfg).unwrap();
        State {
            enabled: Some(false),
            managed: Some(true),
            ..State::default()
        }
        .save(&cfg)
        .unwrap();
        let q = queue::Queue::open(&cfg);
        q.push(&[usage_record("m1")]).unwrap();
        forget_switch(&cfg);
        assert_eq!(State::load(&cfg).enforced(), None);
        assert_eq!(device::load(&cfg), Some(id));
        assert_eq!(q.len(), 0);
    }

    #[test]
    fn logged_out_daemon_reads_no_sessions_and_closes_its_window() {
        let root = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(root.path(), "http://127.0.0.1:9");
        let mut state = State::default();
        state
            .transparent_windows
            .note(switch::Mode::Transparent, 1_000);
        report_offline_usage(&cfg, &mut state);
        assert!(!state.transparent_windows.contains(now_ms() + 1));
        assert!(!cfg.data_dir().join("daemon/sessions.json").exists());
    }

    #[test]
    fn user_change_never_carries_windows_queue_or_switch_over() {
        let root = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(root.path(), "http://127.0.0.1:9");
        let login = |who: &str| {
            std::fs::create_dir_all(cfg.cred_dir()).unwrap();
            std::fs::write(cfg.user_file(), who).unwrap();
        };
        login("user-a");
        let mut state = State {
            enabled: Some(false),
            managed: Some(true),
            ..State::default()
        };
        report_offline_usage(&cfg, &mut state);
        let a = state.user.clone();
        assert!(a.is_some() && state.transparent_windows.contains(now_ms()));
        state.transparent_windows = sessions::Windows::default();
        state
            .transparent_windows
            .note(switch::Mode::Transparent, 1_000);
        let q = queue::Queue::open(&cfg);
        q.push(&[usage_record("m1")]).unwrap();

        login("user-b");
        report_offline_usage(&cfg, &mut state);
        assert!(state.user.is_some() && state.user != a);
        assert!(!state.transparent_windows.contains(1_500));
        assert_eq!(q.len(), 0);
        assert_eq!(state.enforced(), Some(false));

        std::fs::remove_file(cfg.user_file()).unwrap();
        q.push(&[usage_record("m2")]).unwrap();
        report_offline_usage(&cfg, &mut state);
        assert_eq!((state.user.as_deref(), state.enforced()), (None, None));
        assert!(!state.transparent_windows.contains(now_ms()));
        assert_eq!(q.len(), 0);
    }

    fn usage_record(id: &str) -> queue::UsageRecord {
        queue::UsageRecord {
            cache_read: 0,
            cache_write: 0,
            input_tokens: 1,
            message_id: id.into(),
            model: "m".into(),
            occurred_at: "2026-10-28T12:00:00.000Z".into(),
            output_tokens: 1,
            session_ref: "s".into(),
        }
    }

    #[test]
    fn second_daemon_does_not_get_the_lock() {
        let root = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(root.path(), "http://127.0.0.1:9");
        assert!(!running(&cfg));
        let held = lock(&cfg).unwrap();
        assert!(held.is_some());
        assert!(lock(&cfg).unwrap().is_none());
        assert!(running(&cfg));
        drop(held);
        assert!(lock(&cfg).unwrap().is_some());
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
        let first = tick(&cfg, &mut "d".to_string(), &mut state, &mut b).await;
        let second = tick(&cfg, &mut "d".to_string(), &mut state, &mut b).await;
        assert_eq!((first.as_secs(), second.as_secs()), (5, 10));
        assert_eq!(state.enabled, Some(false));
        assert!(state.last_error.is_some());
    }
}
