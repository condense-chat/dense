//! `dense daemon` — the one background service dense runs on a machine. It owns
//! the per-user autostart entry and runs [`Job`]s in turn, each on its own
//! cadence. New background work is a job here, not another service: add a
//! variant to [`Job`] and a line to [`Job::all`]. One daemon per machine, so its
//! lock and state are machine-wide.

pub mod autostart;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use crate::config::Config;
use crate::error::{Context, Error};
use crate::{Result, desktop};

/// Background work. A tick does one pass and returns how long to wait before the
/// next, so a job sets its own cadence and backoff.
// ponytail: jobs take turns on one thread, so a slow tick delays the others;
// give each job its own task if one must run during another's network call.
enum Job {
    DesktopEngines,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct State {
    pub pid: Option<u32>,
}

impl Job {
    fn all() -> Vec<Self> {
        vec![Self::DesktopEngines]
    }

    async fn tick(&mut self, cfg: &Config) -> Duration {
        match self {
            Self::DesktopEngines => desktop::watch(cfg).await,
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
    let report = autostart::install(cfg)?;
    println!("daemon autostart installed -> {}", report.path.display());
    for note in &report.notes {
        eprintln!("warning: {note}");
    }
    Ok(())
}

/// The daemon loop. `once` runs every job a single time and exits, leaving a
/// running daemon's pid alone.
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
    if !once {
        state.pid = Some(std::process::id());
    }
    if let Err(e) = state.save(cfg) {
        tracing::warn!("could not save daemon state: {e}");
    }
    tracing::info!(profile = cfg.profile(), "dense daemon started");

    let mut jobs: Vec<(Job, Instant)> = Job::all()
        .into_iter()
        .map(|job| (job, Instant::now()))
        .collect();
    loop {
        for (job, due) in &mut jobs {
            if *due <= Instant::now() {
                *due = Instant::now() + job.tick(cfg).await;
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

/// `dense status` lines for the daemon.
pub fn status_lines(cfg: &Config) -> Vec<(&'static str, String)> {
    let autostart = match autostart::installed(cfg) {
        Some(p) => format!("autostart installed ({})", p.display()),
        None => "autostart not installed".into(),
    };
    let running = match (running(cfg), State::load(cfg).pid) {
        (true, Some(pid)) => format!("running (pid {pid})"),
        (true, None) => "running".into(),
        (false, _) => "not running".into(),
    };
    vec![("daemon", format!("{running}; {autostart}"))]
}

pub fn uninstall(cfg: &Config) -> Result<()> {
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
        assert!(!running(&cfg));
    }

    #[test]
    fn state_round_trips_the_pid() {
        let root = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(root.path(), "http://127.0.0.1:9");
        State { pid: Some(7) }.save(&cfg).unwrap();
        assert_eq!(State::load(&cfg).pid, Some(7));
    }

    #[test]
    fn every_profile_sees_the_pid_but_only_while_the_daemon_runs() {
        let root = tempfile::tempdir().unwrap();
        let installed_from = Config::for_test(root.path(), "http://127.0.0.1:9");
        let other = Config::for_test(root.path(), "http://127.0.0.1:8");
        State { pid: Some(7) }.save(&installed_from).unwrap();
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
