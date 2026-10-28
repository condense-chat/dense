//! Spans in which `dense claude` routed a Claude Code process through the
//! proxy. The proxy already reports that usage, so the session reader skips
//! lines from those spans. One file per launch under `<data dir>/routed/`,
//! shared by every profile since one daemon serves the machine;
//! the launcher refreshes `seen_ms` while alive, so a crashed launcher's span
//! ends at its last refresh plus [`LEASE_MS`].

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::Config;

pub const LEASE_MS: u64 = 2 * 60 * 1_000;
const MAX_SPANS: usize = 1_024;
const REFRESH: Duration = Duration::from_secs(30);
const RETENTION_MS: u64 = 35 * 24 * 3_600 * 1_000;

/// A live span owned by the launching `dense claude`. Call [`Guard::close`]
/// when the child exits.
pub struct Guard {
    path: PathBuf,
    refresh: tokio::task::JoinHandle<()>,
    span: Span,
}

#[derive(Debug, Default)]
pub struct Routed(pub Vec<Span>);

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Span {
    pub cwd: PathBuf,
    pub end_ms: Option<u64>,
    pub pid: u32,
    pub seen_ms: u64,
    pub start_ms: u64,
}

impl Guard {
    pub fn open(cfg: &Config) -> Option<Self> {
        let cwd = std::env::current_dir().ok()?;
        let now = now_ms();
        let span = Span {
            cwd,
            end_ms: None,
            pid: std::process::id(),
            seen_ms: now,
            start_ms: now,
        };
        let dir = dir(cfg);
        let path = dir.join(format!("{}.json", uuid::Uuid::new_v4()));
        if let Err(e) = std::fs::create_dir_all(&dir).and_then(|()| write(&path, &span)) {
            tracing::debug!("could not record routed span: {e}");
            return None;
        }
        let refresh = {
            let (path, mut span) = (path.clone(), span.clone());
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(REFRESH).await;
                    span.seen_ms = now_ms();
                    let _ = write(&path, &span);
                }
            })
        };
        Some(Self {
            path,
            refresh,
            span,
        })
    }

    pub fn close(mut self) {
        self.refresh.abort();
        let now = now_ms();
        self.span.seen_ms = now;
        self.span.end_ms = Some(now);
        let _ = write(&self.path, &self.span);
    }
}

impl Routed {
    /// Load every span, deleting expired files and the oldest beyond the cap.
    pub fn load_and_prune(cfg: &Config, now_ms: u64) -> Self {
        let dir = dir(cfg);
        let mut spans: Vec<(PathBuf, Span)> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .filter_map(|p| {
                let span = std::fs::read(&p)
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Span>(&b).ok());
                if span.is_none() {
                    let _ = std::fs::remove_file(&p);
                }
                span.map(|s| (p, s))
            })
            .collect();
        spans.sort_by_key(|(_, s)| std::cmp::Reverse(s.start_ms));
        let mut kept = Vec::new();
        for (i, (path, span)) in spans.into_iter().enumerate() {
            if i >= MAX_SPANS || span.effective_end().saturating_add(RETENTION_MS) < now_ms {
                let _ = std::fs::remove_file(&path);
            } else {
                kept.push(span);
            }
        }
        Self(kept)
    }

    pub fn covers(&self, cwd: &Path, ts_ms: u64) -> bool {
        self.0.iter().any(|s| s.covers(cwd, ts_ms))
    }
}

impl Span {
    pub fn effective_end(&self) -> u64 {
        self.end_ms
            .unwrap_or_else(|| self.seen_ms.saturating_add(LEASE_MS))
    }

    pub fn covers(&self, cwd: &Path, ts_ms: u64) -> bool {
        self.start_ms <= ts_ms && ts_ms < self.effective_end() && cwd.starts_with(&self.cwd)
    }
}

fn dir(cfg: &Config) -> PathBuf {
    cfg.data_dir().join("routed")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn write(path: &Path, span: &Span) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec(span).map_err(std::io::Error::other)?,
    )?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(cwd: &str, start: u64, seen: u64, end: Option<u64>) -> Span {
        Span {
            cwd: cwd.into(),
            end_ms: end,
            pid: 1,
            seen_ms: seen,
            start_ms: start,
        }
    }

    #[test]
    fn covers_cwd_subtree_inside_the_span() {
        let s = span("/w/proj", 100, 150, Some(200));
        assert!(s.covers(Path::new("/w/proj"), 100));
        assert!(s.covers(Path::new("/w/proj/sub"), 199));
        assert!(!s.covers(Path::new("/w/proj"), 200));
        assert!(!s.covers(Path::new("/w/proj"), 99));
        assert!(!s.covers(Path::new("/w/project-2"), 150));
        assert!(!s.covers(Path::new("/w"), 150));
    }

    #[test]
    fn open_span_of_a_crashed_launcher_ends_after_the_lease() {
        let s = span("/w", 100, 1_000, None);
        assert!(s.covers(Path::new("/w"), 1_000 + LEASE_MS - 1));
        assert!(!s.covers(Path::new("/w"), 1_000 + LEASE_MS));
    }

    #[test]
    fn load_prunes_expired_corrupt_and_excess_files() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path(), "http://127.0.0.1:9");
        let d = dir(&cfg);
        std::fs::create_dir_all(&d).unwrap();
        let now = RETENTION_MS * 2;
        write(&d.join("old.json"), &span("/w", 1, 1, Some(2))).unwrap();
        write(&d.join("live.json"), &span("/w", now - 10, now - 5, None)).unwrap();
        std::fs::write(d.join("bad.json"), b"{").unwrap();
        let r = Routed::load_and_prune(&cfg, now);
        assert_eq!(r.0.len(), 1);
        assert_eq!(r.0[0].start_ms, now - 10);
        let mut left: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["live.json"]);
    }

    #[tokio::test]
    async fn span_from_any_profile_reaches_the_machine_daemon() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = Config::for_test(tmp.path(), "http://127.0.0.1:9");
        let other = Config::for_test(tmp.path(), "http://127.0.0.2:9");
        assert_ne!(daemon.profile(), other.profile());
        let guard = Guard::open(&other).unwrap();
        let now = now_ms();
        let cwd = std::env::current_dir().unwrap();
        assert!(Routed::load_and_prune(&daemon, now).covers(&cwd, now));
        guard.close();
        let r = Routed::load_and_prune(&daemon, now_ms());
        assert_eq!(r.0.len(), 1);
        assert!(r.0[0].end_ms.is_some());
    }
}
