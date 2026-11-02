//! Device management, the daemon's [`Device`](super::Job::Device) job: heartbeat,
//! the org's on/off switch, and offline usage upload.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::{State, autostart, queue, save, sessions, switch};
use crate::api::Api;
use crate::api::auth;
use crate::api::devices::{self, Heartbeat, HeartbeatReply, Reply};
use crate::config::Config;
use crate::error::Error;
use crate::{Result, desktop, device};

const DEVICE_LIMIT_MSG: &str =
    "device limit reached: remove old devices in the dashboard (this device stays unmanaged)";
const BACKOFF_MAX: Duration = Duration::from_secs(300);
const BACKOFF_MIN: Duration = Duration::from_secs(5);
const INTERVAL_MAX_S: u64 = 3_600;
const INTERVAL_MIN_S: u64 = 5;
/// After a switch change, re-heartbeat this soon so the new mode is reported.
pub const REPORT_DELAY: Duration = Duration::from_secs(1);

/// The job's own state; what survives a restart lives in [`State`].
pub struct Device {
    backoff: Backoff,
    device_id: String,
}

/// Doubling retry delay for failed heartbeats, `min..=max`.
pub struct Backoff {
    current: Duration,
    max: Duration,
    min: Duration,
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
    /// The switch to enforce, if this device is managed and one is known.
    pub fn enforced(&self) -> Option<bool> {
        self.enabled
            .filter(|_| cfg!(feature = "remote-control") && self.is_managed())
    }

    /// Session files are read and uploaded on managed devices only.
    pub fn collects_usage(&self) -> bool {
        cfg!(feature = "offline-usage") && self.is_managed()
    }

    pub fn is_managed(&self) -> bool {
        self.managed == Some(true)
    }

    /// False when the daemon runs for another profile than `cfg`.
    pub fn owned_by(&self, cfg: &Config) -> bool {
        self.profile.as_deref().is_none_or(|p| p == cfg.profile())
    }

    pub fn forget_switch(&mut self) {
        self.enabled = None;
        self.managed = None;
        self.member_enabled = None;
        self.device_enabled = None;
    }

    fn observed(&mut self, mode: switch::Mode) {
        self.mode = Some(mode);
        if self.collects_usage() {
            self.transparent_windows.note(mode, now_ms());
        }
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

impl Device {
    /// Before the first heartbeat: close a transparent window the daemon was
    /// not alive to close, and re-apply the last known switch.
    pub fn start(cfg: &Config, state: &mut State) -> Result<Self> {
        let device_id = device::id(cfg)?;
        sessions::remember_root(cfg);
        tracing::info!(device = %device_id, "device management job started");
        track_user(cfg, state);
        if switch::observe(cfg) == switch::Mode::Condensing {
            state.transparent_windows.close(last_seen_ms(state));
        }
        if let Some(enabled) = state.enforced() {
            tracing::info!(enabled, "applying the last known switch");
            apply(cfg, state, enabled);
        }
        Ok(Self {
            backoff: Backoff::new(BACKOFF_MIN, BACKOFF_MAX),
            device_id,
        })
    }

    pub async fn tick(&mut self, cfg: &Config, state: &mut State) -> Duration {
        tick(cfg, &mut self.device_id, state, &mut self.backoff).await
    }
}

/// Whether the org turned dense off on this device for the daemon's login.
/// Routing entry points (`dense <tool>`, wrapped Desktop engines) honour it on
/// every profile: the switch is the device's, and the device has one daemon.
/// Fails open: an unreadable state file routes until the next heartbeat.
pub fn switched_off(cfg: &Config) -> bool {
    let state = State::load(cfg);
    let owner = state.profile.as_deref().map(|p| cfg.on_profile(p));
    state.enforced() == Some(false) && state.user == user_ref(owner.as_ref().unwrap_or(cfg))
}

/// Whether routed Claude Code runs need a span for the session-file reader.
pub fn collects_usage(cfg: &Config) -> bool {
    State::load(cfg).collects_usage()
}

/// After `dense login`: if the account's org manages devices, install the
/// daemon so the switch applies without anyone running `dense daemon install`.
pub async fn after_login(cfg: &Config) {
    sessions::remember_root(cfg);
    if let Some(user) = auth::load_creds(cfg).user_id {
        match device::bind_user(cfg, &user) {
            Ok(true) if State::load(cfg).owned_by(cfg) => drop_usage(cfg),
            Ok(_) => {}
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
        Ok(Some((reply, _))) if reply.managed && cfg!(feature = "login-autostart") => {}
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
    if !state.owned_by(cfg) {
        return;
    }
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

/// `dense status` lines for the device.
pub fn status_lines(cfg: &Config, state: &State) -> Vec<(&'static str, String)> {
    let device = device::load(cfg).unwrap_or_else(|| "none yet".into());
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
    let mut lines = vec![
        ("device", format!("{device}; {beat}")),
        ("switch", switch),
        ("mode", reported_mode(cfg, state).as_str().to_string()),
    ];
    if state.collects_usage() {
        lines.push((
            "queue",
            format!("{} usage records pending", queue::Queue::open(cfg).len()),
        ));
    }
    lines
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
pub fn jitter(d: Duration) -> Duration {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|t| t.subsec_nanos())
        .unwrap_or(0);
    d + d.mul_f64(f64::from(nanos % 1_000) / 5_000.0)
}

/// When the daemon last ran a tick (ms) — the latest a mode change it missed can be dated.
fn last_seen_ms(state: &State) -> u64 {
    state.last_attempt_at.map_or_else(now_ms, |s| s * 1_000)
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

/// Follow the logged-in user. On any change nothing of the previous user's
/// carries over: not their windows, queued usage, or org switch.
fn track_user(cfg: &Config, state: &mut State) -> Option<String> {
    let user = user_ref(cfg);
    if state.user != user {
        if state.user.is_some() {
            state.transparent_windows = sessions::Windows::default();
            drop_usage(cfg);
            state.forget_switch();
        }
        if user.is_none() {
            state.forget_switch();
        }
        state.user.clone_from(&user);
    }
    user
}

/// Queue session-file usage for the transparent windows. Runs before the
/// heartbeat so it works offline; the queue carries it to the next upload.
/// The window follows the shims actually on disk (`dense claude` runs are
/// excluded by their routed spans instead).
fn report_offline_usage(cfg: &Config, state: &mut State) {
    let now = now_ms();
    if track_user(cfg, state).is_none() {
        state.transparent_windows = sessions::Windows::default();
        return;
    }
    if !state.collects_usage() {
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

/// One login on one profile: the same id on another profile is a user change.
fn user_ref(cfg: &Config) -> Option<String> {
    use sha2::{Digest, Sha256};
    let creds = auth::load_creds(cfg);
    let who = creds.user_id.as_deref().or(creds.token.as_deref())?;
    let key = format!("{}\n{who}", cfg.profile());
    Some(hex::encode(&Sha256::digest(key.as_bytes())[..8]))
}

/// One heartbeat. `Ok(None)`: the server refused this credential/device for
/// good (4xx) — the cached switch is dropped so nothing stale is enforced. A
/// `device_owned_elsewhere` refusal swaps in a fresh device id (or one another
/// process already wrote) and retries once.
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
        match send_heartbeat(cfg, &api, device_id, state).await? {
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

async fn send_heartbeat(
    cfg: &Config,
    api: &Api,
    device_id: &str,
    state: &State,
) -> Result<Reply<HeartbeatReply>> {
    let hostname = device::hostname();
    let body = Heartbeat {
        app_version: env!("CARGO_PKG_VERSION"),
        device_id,
        hostname: &hostname,
        mode: Some(reported_mode(cfg, state)),
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

    let more = if state.collects_usage() {
        upload_usage(cfg, &api, device_id).await
    } else {
        state.transparent_windows = sessions::Windows::default();
        drop_usage(cfg);
        false
    };

    if changed {
        REPORT_DELAY
    } else if more {
        BACKOFF_MIN
    } else {
        clamp_interval(reply.heartbeat_interval_s)
    }
}

/// Flush the queue, logging the outcome. True when a backlog remains to send.
async fn upload_usage(cfg: &Config, api: &Api, device_id: &str) -> bool {
    let queue = queue::Queue::open(cfg);
    let flushed = queue::flush_to(api, device_id, &queue).await;
    let more = flushed
        .as_ref()
        .is_ok_and(|f| f.more && f.stalled.is_none());
    match flushed {
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
    more
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

/// The mode the server is told: shims on disk, or wrapped Desktop engines that
/// the switch has not turned off. Offline windows follow the shims alone; a
/// routed Desktop session is excluded by its routed span instead.
fn reported_mode(cfg: &Config, state: &State) -> switch::Mode {
    let desktop_off = cfg!(feature = "desktop-switch") && state.enforced() == Some(false);
    let desktop_routed = || !desktop_off && desktop::has_wrapped_engines(cfg);
    match switch::observe(cfg) {
        switch::Mode::Transparent if desktop_routed() => switch::Mode::Condensing,
        mode => mode,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::PathBuf;

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

    #[cfg(feature = "remote-control")]
    #[test]
    fn only_a_managed_off_switch_turns_desktop_routing_off() {
        let root = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(root.path(), "http://127.0.0.1:9");
        assert!(!switched_off(&cfg));
        for (managed, enabled, off) in [
            (Some(true), Some(false), true),
            (Some(true), Some(true), false),
            (Some(false), Some(false), false),
            (None, Some(false), false),
        ] {
            let state = State {
                managed,
                enabled,
                ..State::default()
            };
            state.save(&cfg).unwrap();
            assert_eq!(switched_off(&cfg), off, "{managed:?} {enabled:?}");
        }
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
        assert!(back.is_managed());
        assert_eq!(back.transparent_windows, s.transparent_windows);
        assert!(
            back.transparent_windows.contains(1_500) && !back.transparent_windows.contains(2_500)
        );
    }

    /// A one-route HTTP server: every request gets `status` + `body`; returns
    /// the base URL and the request lines it saw.
    pub(crate) fn serve(
        status: u16,
        body: &'static str,
    ) -> (String, std::sync::mpsc::Receiver<String>) {
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

    #[cfg(feature = "remote-control")]
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
        assert_eq!(state.enabled, Some(false));
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
        let lines = status_lines(&cfg, &State::load(&cfg));
        assert!(
            lines
                .iter()
                .any(|(k, v)| *k == "switch" && v.starts_with("device limit reached"))
        );
    }

    #[cfg(feature = "offline-usage")]
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

    #[tokio::test]
    async fn a_device_that_is_not_managed_collects_nothing() {
        collects_nothing_on(r#"{"managed":false,"enabled":true,"heartbeat_interval_s":60}"#).await;
    }

    #[cfg(not(feature = "offline-usage"))]
    #[tokio::test]
    async fn a_build_without_offline_usage_collects_nothing_on_a_managed_device() {
        collects_nothing_on(OK_OFF).await;
    }

    #[cfg(not(feature = "remote-control"))]
    #[tokio::test]
    async fn a_build_without_remote_control_leaves_routing_to_the_user() {
        let (url, _rx) = serve(200, OK_OFF);
        let root = tempfile::tempdir().unwrap();
        let cfg = logged_in(root.path(), &url);
        let shim = shim(&cfg);
        let mut state = State::default();
        let mut b = Backoff::new(BACKOFF_MIN, BACKOFF_MAX);
        tick(&cfg, &mut "d".to_string(), &mut state, &mut b).await;
        state.save(&cfg).unwrap();
        assert!(shim.exists());
        assert!(!switched_off(&cfg));
    }

    #[tokio::test]
    async fn a_v0_11_daemon_updated_in_place_changes_nothing_for_a_regular_user() {
        let replies = [
            (
                200,
                r#"{"managed":false,"enabled":false,"heartbeat_interval_s":60}"#,
            ),
            (409, r#"{"detail":{"code":"device_limit"}}"#),
        ];
        for ((status, body), shimmed) in replies.into_iter().flat_map(|r| [(r, true), (r, false)]) {
            let (url, seen) = serve(status, body);
            let root = tempfile::tempdir().unwrap();
            let cfg = logged_in(root.path(), &url);
            let shim = shimmed.then(|| shim(&cfg));
            std::fs::create_dir_all(cfg.daemon_dir()).unwrap();
            std::fs::write(cfg.daemon_dir().join("state.toml"), "pid = 7\n").unwrap();

            let mut state = State::load(&cfg);
            assert_eq!(state.pid, Some(7));
            let mut job = Device::start(&cfg, &mut state).unwrap();
            job.tick(&cfg, &mut state).await;

            let session = root.path().join(".claude/projects/-work/s.jsonl");
            std::fs::create_dir_all(session.parent().unwrap()).unwrap();
            let line = sessions::tests::assistant("m", now_ms(), 5, Some("end_turn"));
            std::fs::write(&session, line).unwrap();
            job.tick(&cfg, &mut state).await;
            job.tick(&cfg, &mut state).await;
            state.save(&cfg).unwrap();

            assert!(shim.is_none_or(|s| s.exists()));
            assert_eq!(State::load(&cfg).pid, Some(7));
            assert_eq!(state.enforced(), None);
            assert!(!switched_off(&cfg));
            assert!(state.transparent_windows.is_empty());
            assert_eq!(queue::Queue::open(&cfg).len(), 0);
            assert!(!cfg.data_dir().join("daemon/sessions.json").exists());
            let seen: Vec<_> = seen.try_iter().collect();
            assert_eq!(seen.len(), 3, "{seen:?}");
            assert!(seen.iter().all(|r| r.contains("/v1/devices/heartbeat")));
        }
    }

    async fn collects_nothing_on(reply: &'static str) {
        let (url, seen) = serve(200, reply);
        let root = tempfile::tempdir().unwrap();
        let cfg = logged_in(root.path(), &url);
        queue::Queue::open(&cfg)
            .push(&[usage_record("m1")])
            .unwrap();
        let mut state = State::default();
        state
            .transparent_windows
            .note(switch::Mode::Transparent, 1_000);
        let mut b = Backoff::new(BACKOFF_MIN, BACKOFF_MAX);
        tick(&cfg, &mut "d".to_string(), &mut state, &mut b).await;
        tick(&cfg, &mut "d".to_string(), &mut state, &mut b).await;

        assert!(state.transparent_windows.is_empty());
        assert_eq!(queue::Queue::open(&cfg).len(), 0);
        assert!(!cfg.data_dir().join("daemon/sessions.json").exists());
        let seen: Vec<_> = seen.try_iter().collect();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(seen.iter().all(|r| r.contains("/v1/devices/heartbeat")));
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

    #[cfg(feature = "offline-usage")]
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
        assert_eq!(state.enforced(), None);

        std::fs::remove_file(cfg.user_file()).unwrap();
        q.push(&[usage_record("m2")]).unwrap();
        report_offline_usage(&cfg, &mut state);
        assert_eq!((state.user.as_deref(), state.enforced()), (None, None));
        assert!(!state.transparent_windows.contains(now_ms()));
        assert_eq!(q.len(), 0);
    }

    #[cfg(feature = "remote-control")]
    #[test]
    fn the_daemons_switch_binds_every_profile() {
        let root = tempfile::tempdir().unwrap();
        let daemon = logged_in(root.path(), "http://127.0.0.1:9");
        let active = logged_in(root.path(), "http://127.0.0.1:8");
        let mut state = State {
            enabled: Some(false),
            managed: Some(true),
            profile: Some(daemon.profile().to_string()),
            ..State::default()
        };
        track_user(&daemon, &mut state);
        state.save(&daemon).unwrap();
        assert!(switched_off(&active));

        std::fs::remove_file(daemon.user_file()).unwrap();
        assert!(!switched_off(&active), "a logged-out daemon gates nobody");
    }

    #[test]
    fn another_profiles_logout_and_login_leave_the_daemons_state_alone() {
        let root = tempfile::tempdir().unwrap();
        let daemon = logged_in(root.path(), "http://127.0.0.1:9");
        let other = logged_in(root.path(), "http://127.0.0.1:8");
        let mut state = State {
            enabled: Some(false),
            managed: Some(true),
            profile: Some(daemon.profile().to_string()),
            ..State::default()
        };
        track_user(&daemon, &mut state);
        state.save(&daemon).unwrap();
        let queue = queue::Queue::open(&daemon);
        queue.push(&[usage_record("kept")]).unwrap();

        forget_switch(&other);
        assert_eq!(State::load(&daemon).enabled, Some(false));
        assert_eq!(queue.len(), 1);

        forget_switch(&daemon);
        assert_eq!(State::load(&daemon).enabled, None);
        assert_eq!(queue.len(), 0);
    }

    #[test]
    fn a_daemon_moved_to_another_profile_starts_clean() {
        let root = tempfile::tempdir().unwrap();
        let before = logged_in(root.path(), "http://127.0.0.1:9");
        let after = logged_in(root.path(), "http://127.0.0.1:8");
        let mut state = State {
            enabled: Some(false),
            managed: Some(true),
            ..State::default()
        };
        track_user(&before, &mut state);
        track_user(&after, &mut state);
        assert_eq!(state.enforced(), None);
    }

    #[test]
    fn unreadable_state_fails_open() {
        let root = tempfile::tempdir().unwrap();
        let cfg = logged_in(root.path(), "http://127.0.0.1:9");
        std::fs::create_dir_all(cfg.daemon_dir()).unwrap();
        std::fs::write(
            cfg.daemon_dir().join("state.toml"),
            "managed = true\nenabled = fal",
        )
        .unwrap();
        assert!(!switched_off(&cfg));
    }

    #[cfg(feature = "remote-control")]
    #[tokio::test]
    async fn previous_users_off_switch_is_not_applied_after_a_restart() {
        let root = tempfile::tempdir().unwrap();
        let cfg = logged_in(root.path(), "http://127.0.0.1:9");
        let mut state = State {
            enabled: Some(false),
            managed: Some(true),
            ..State::default()
        };
        track_user(&cfg, &mut state);
        state.save(&cfg).unwrap();
        assert!(switched_off(&cfg));

        std::fs::write(cfg.user_file(), "someone-else\n").unwrap();
        let shim = shim(&cfg);
        let mut state = State::load(&cfg);
        let mut job = Device::start(&cfg, &mut state).unwrap();
        state.save(&cfg).unwrap();
        assert!(shim.exists());
        assert!(!switched_off(&cfg));
        job.tick(&cfg, &mut state).await;
        assert_eq!(state.enforced(), None);
        assert!(shim.exists());
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
