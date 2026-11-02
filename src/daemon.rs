//! `dense daemon` — the one background service dense runs on a machine. It owns
//! the per-user autostart entry and runs [`Job`]s in turn, each on its own
//! cadence. New background work is a job here, not another service: add a
//! variant to [`Job`] and a line to [`Job::all`]. One daemon per machine, so its
//! lock and state are machine-wide.

pub mod autostart;
pub mod managed;
pub mod queue;
pub mod routed;
pub mod sessions;
pub mod switch;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use crate::config::Config;
use crate::error::{Context, Error};
use crate::{Result, desktop, device};

/// Background work. A tick does one pass and returns how long to wait before the
/// next, so a job sets its own cadence and backoff.
// ponytail: jobs take turns on one thread, so a slow tick delays the others;
// give each job its own task if one must run during another's network call.
enum Job {
    DesktopEngines(desktop::Watcher),
    /// Heartbeat, the org's on/off switch, and offline usage upload.
    Device(managed::Device),
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
    /// The profile the daemon runs for. The switch, windows and queue are that
    /// profile's login, whichever profile the process reading them resolved to.
    pub profile: Option<String>,
    /// When no condense routing was applied: the only spans whose
    /// session-file usage the daemon reports (the proxy reports the rest).
    #[serde(default)]
    pub transparent_windows: sessions::Windows,
    /// Hash of the credentials the windows and queue belong to.
    pub user: Option<String>,
}

impl Job {
    fn all(cfg: &Config, state: &mut State) -> Vec<Self> {
        let mut jobs = vec![Self::DesktopEngines(desktop::Watcher::default())];
        match managed::Device::start(cfg, state) {
            Ok(job) => jobs.push(Self::Device(job)),
            Err(e) => tracing::warn!("device management off: no device id ({e})"),
        }
        jobs
    }

    async fn tick(&mut self, cfg: &Config, state: &mut State) -> Duration {
        match self {
            Self::DesktopEngines(watcher) => desktop::watch(cfg, watcher).await,
            Self::Device(job) => job.tick(cfg, state).await,
        }
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
}

pub fn install(cfg: &Config) -> Result<()> {
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

/// The daemon loop. `once` runs every job a single time (a job that changed
/// something and asks to report it soon gets one more pass) and exits, leaving
/// a running daemon's pid alone.
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
    let exe = std::env::current_exe().ctx("locating the dense binary")?;
    let started_from = modified(&exe);
    let mut state = State::load(cfg);
    state.profile = Some(cfg.profile().to_string());
    if !once {
        state.pid = Some(std::process::id());
    }
    tracing::info!(profile = cfg.profile(), "dense daemon started");
    let mut jobs: Vec<(Job, Instant)> = Job::all(cfg, &mut state)
        .into_iter()
        .map(|job| (job, Instant::now()))
        .collect();
    save(cfg, &state);

    loop {
        for (job, due) in &mut jobs {
            if *due <= Instant::now() {
                let mut wait = job.tick(cfg, &mut state).await;
                if once && wait == managed::REPORT_DELAY {
                    wait = job.tick(cfg, &mut state).await;
                }
                save(cfg, &state);
                *due = Instant::now() + managed::jitter(wait);
            }
        }
        if once {
            return Ok(());
        }
        // A failure exit, because the Windows launcher only restarts on one.
        if modified(&exe) != started_from {
            return Err(Error::msg(
                "dense was updated; restarting on the new binary",
            ));
        }
        let Some(next) = jobs.iter().map(|(_, due)| *due).min() else {
            return Ok(());
        };
        tokio::select! {
            () = tokio::time::sleep_until(next) => {}
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("dense daemon stopping");
                return Ok(());
            }
        }
    }
}

pub fn running(cfg: &Config) -> bool {
    let Ok(file) = File::options().write(true).open(lock_file(cfg)) else {
        return false;
    };
    matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock))
}

/// `dense status` lines for the daemon and device.
pub fn status_lines(cfg: &Config) -> Vec<(&'static str, String)> {
    let state = State::load(cfg);
    let autostart = match autostart::installed(cfg) {
        Some(p) => format!("autostart installed ({})", p.display()),
        None => "autostart not installed".into(),
    };
    let running = match (running(cfg), state.pid) {
        (true, Some(pid)) => format!("running (pid {pid})"),
        (true, None) => "running".into(),
        (false, _) => "not running".into(),
    };
    let mut lines = vec![("daemon", format!("{running}; {autostart}"))];
    match state.profile.as_deref().filter(|_| !state.owned_by(cfg)) {
        Some(owner) => lines.push(("device", format!("managed by the daemon for `{owner}`"))),
        None => lines.extend(managed::status_lines(cfg, &state)),
    }
    lines
}

/// Remove the one autostart entry and tell condense this device is gone.
/// Routing (shims, wrapped Desktop engines) stays as it is.
pub async fn uninstall(cfg: &Config) -> Result<()> {
    remove_autostart(cfg)?;
    managed::report_uninstall(cfg).await;
    Ok(())
}

pub fn remove_autostart(cfg: &Config) -> Result<()> {
    match autostart::uninstall(cfg, live_pid(cfg))? {
        Some(report) => {
            println!("daemon autostart removed ({})", report.path.display());
            for note in &report.notes {
                eprintln!("warning: {note}");
            }
        }
        None => println!("daemon autostart was not installed"),
    }
    Ok(())
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

/// Only a daemon holding the lock has a pid worth signalling: a stale one could
/// reach `taskkill`.
fn live_pid(cfg: &Config) -> Option<u32> {
    running(cfg).then(|| State::load(cfg).pid).flatten()
}

fn lock_file(cfg: &Config) -> PathBuf {
    cfg.daemon_dir().join("daemon.lock")
}

/// How the daemon notices a self-update: the old image keeps running until the
/// service manager restarts it from the new file.
fn modified(exe: &Path) -> Option<SystemTime> {
    std::fs::metadata(exe).and_then(|m| m.modified()).ok()
}

fn save(cfg: &Config, state: &State) {
    if let Err(e) = state.save(cfg) {
        tracing::warn!("could not save daemon state: {e}");
    }
}

fn state_file(cfg: &Config) -> PathBuf {
    cfg.daemon_dir().join("state.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lock_admits_one_daemon_and_shows_it_as_running() {
        let root = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(root.path(), "http://127.0.0.1:9");
        assert!(!running(&cfg));

        let held = lock(&cfg).unwrap();
        assert!(held.is_some());
        assert!(lock(&cfg).unwrap().is_none());
        assert!(running(&cfg));

        drop(held);
        // A parallel test's fork shares the lock until its child execs.
        assert!((0..100).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(10));
            !running(&cfg)
        }));
    }

    #[test]
    fn state_round_trips_the_pid() {
        let root = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(root.path(), "http://127.0.0.1:9");
        State {
            pid: Some(7),
            ..State::default()
        }
        .save(&cfg)
        .unwrap();
        assert_eq!(State::load(&cfg).pid, Some(7));
    }

    #[test]
    fn every_profile_sees_the_pid_but_only_while_the_daemon_runs() {
        let root = tempfile::tempdir().unwrap();
        let installed_from = Config::for_test(root.path(), "http://127.0.0.1:9");
        let other = Config::for_test(root.path(), "http://127.0.0.1:8");
        State {
            pid: Some(7),
            ..State::default()
        }
        .save(&installed_from)
        .unwrap();
        assert_eq!(live_pid(&other), None);

        let _held = lock(&installed_from).unwrap();
        assert_eq!(live_pid(&other), Some(7));
    }

    #[tokio::test]
    async fn once_refuses_to_run_beside_a_daemon() {
        let root = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(root.path(), "http://127.0.0.1:9");
        let _held = lock(&cfg).unwrap();
        assert!(run(&cfg, true).await.is_err());
    }
}
