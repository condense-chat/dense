//! Spans in which `dense claude` routed a Claude Code process through the
//! proxy. The proxy already reports that usage, so the session reader skips
//! those sessions' lines inside the span. One file per launch under
//! `<data dir>/routed/`, shared by every profile since one daemon serves the
//! machine. The launcher refreshes `seen_ms` while alive, so a crashed
//! launcher's span ends at its last refresh plus [`LEASE_MS`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::Config;

pub const LEASE_MS: u64 = 2 * 60 * 1_000;
const MAX_BINDINGS: usize = 1_024;
const MAX_SPANS: usize = 1_024;
const REFRESH: Duration = Duration::from_secs(30);
const RETENTION_MS: u64 = 35 * 24 * 3_600 * 1_000;
/// Claude Code subcommands that start no conversation.
const SUBCOMMANDS: &[&str] = &[
    "agents",
    "attach",
    "auth",
    "auto-mode",
    "doctor",
    "gateway",
    "import",
    "install",
    "logs",
    "mcp",
    "plugin",
    "plugins",
    "purge",
    "respawn",
    "rm",
    "setup-token",
    "stop",
    "kill",
    "ultrareview",
    "update",
    "upgrade",
];

/// Which Claude Code session a `dense claude` launch runs.
#[derive(Debug, PartialEq, Eq)]
pub enum Pin {
    /// None named: pass `--session-id <uuid>` so the reader can match on it.
    Inject(String),
    /// The user named it (`--session-id`, `--resume <uuid>`).
    Known(String),
    /// `--continue`, the resume picker, a fork: bound by the launch cwd instead.
    Unknown,
}

pub enum Verdict {
    /// Not yet decidable (a span may still be refreshed); re-read next tick.
    Defer,
    Report,
    Skip,
}

/// Sessions without a known id, bound to the span whose launch cwd and time
/// their first line matched. Persisted with the reader's cursors.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Bindings(BTreeMap<String, String>);

/// A live span owned by the launching `dense claude`. Call [`Guard::close`]
/// when the child exits.
pub struct Guard {
    path: PathBuf,
    refresh: tokio::task::JoinHandle<()>,
    span: Span,
}

#[derive(Debug)]
pub struct Routed {
    /// When the spans were read: a span opened after this is not in `spans`.
    pub loaded_ms: u64,
    pub spans: Vec<Span>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Span {
    pub cwd: PathBuf,
    pub end_ms: Option<u64>,
    #[serde(default)]
    pub id: String,
    pub pid: u32,
    pub seen_ms: u64,
    #[serde(default)]
    pub session_id: Option<String>,
    pub start_ms: u64,
}

impl Bindings {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Drop bindings to spans that were pruned. Returns whether any went.
    pub fn retain_known(&mut self, routed: &Routed) -> bool {
        let before = self.0.len();
        self.0
            .retain(|_, span| routed.spans.iter().any(|s| &s.id == span));
        while self.0.len() > MAX_BINDINGS {
            self.0.pop_first();
        }
        self.0.len() != before
    }
}

impl Guard {
    pub fn open(cfg: &Config, session_id: Option<String>) -> Option<Self> {
        let cwd = std::env::current_dir().ok()?;
        let now = now_ms();
        let id = uuid::Uuid::new_v4().to_string();
        let dir = dir(cfg);
        let path = dir.join(format!("{id}.json"));
        let span = Span {
            cwd,
            end_ms: None,
            id,
            pid: std::process::id(),
            seen_ms: now,
            session_id,
            start_ms: now,
        };
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

    pub async fn close(mut self) {
        self.refresh.abort();
        let _ = (&mut self.refresh).await;
        let now = now_ms();
        self.span.seen_ms = now;
        self.span.end_ms = Some(now);
        let _ = write(&self.path, &self.span);
    }
}

impl Pin {
    pub fn of(args: &[String]) -> Self {
        let mut resume = None;
        let mut unknown = false;
        let mut it = args.iter().take_while(|a| *a != "--").peekable();
        if it.peek().is_some_and(|a| SUBCOMMANDS.contains(&a.as_str())) {
            return Self::Unknown;
        }
        while let Some(a) = it.next() {
            let (flag, inline) = match a.split_once('=') {
                Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
                _ => (a.as_str(), None),
            };
            let mut value = || {
                inline
                    .clone()
                    .or_else(|| it.next_if(|v| !v.starts_with('-')).map(String::to_string))
            };
            match flag {
                "--session-id" => {
                    if let Some(v) = value() {
                        return Self::Known(v);
                    }
                }
                "-r" | "--resume" => match value() {
                    Some(v) if uuid::Uuid::parse_str(&v).is_ok() => resume = Some(v),
                    _ => unknown = true,
                },
                "-c" | "--continue" | "--fork-session" | "--teleport" | "--from-pr" => {
                    unknown = true;
                }
                _ => {}
            }
        }
        match resume {
            _ if unknown => Self::Unknown,
            Some(v) => Self::Known(v),
            None => Self::Inject(uuid::Uuid::new_v4().to_string()),
        }
    }
}

impl Default for Routed {
    fn default() -> Self {
        Self {
            loaded_ms: u64::MAX,
            spans: Vec::new(),
        }
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
                let bytes = std::fs::read(&p).ok()?;
                match serde_json::from_slice::<Span>(&bytes) {
                    Ok(mut s) => {
                        if s.id.is_empty() {
                            s.id = p.file_stem()?.to_string_lossy().into_owned();
                        }
                        Some((p, s))
                    }
                    Err(_) => {
                        let _ = std::fs::remove_file(&p);
                        None
                    }
                }
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
        Self {
            loaded_ms: now_ms,
            spans: kept,
        }
    }

    /// Decide one assistant line: skip it when its session ran under a
    /// `dense claude` span at `ts_ms`.
    pub fn judge(
        &self,
        bindings: &mut Bindings,
        session: Option<&str>,
        cwd: Option<&Path>,
        ts_ms: u64,
        now_ms: u64,
    ) -> Verdict {
        if ts_ms >= self.loaded_ms {
            return Verdict::Defer;
        }
        let Some(session) = session else {
            return Verdict::Report;
        };
        let bound = bindings.0.get(session);
        let mut verdict = Verdict::Report;
        let candidates = self
            .spans
            .iter()
            .filter(|s| s.session_id.as_deref() == Some(session) || bound == Some(&s.id));
        for s in candidates {
            match s.judge(ts_ms, now_ms) {
                Verdict::Skip => return Verdict::Skip,
                Verdict::Defer => verdict = Verdict::Defer,
                Verdict::Report => {}
            }
        }
        if bound.is_some() || !matches!(verdict, Verdict::Report) {
            return verdict;
        }
        let Some(cwd) = cwd else {
            return verdict;
        };
        let unpinned = self
            .spans
            .iter()
            .filter(|s| s.session_id.is_none() && same_path(&s.cwd, cwd));
        for s in unpinned {
            match s.judge(ts_ms, now_ms) {
                Verdict::Skip => {
                    bindings.0.insert(session.to_string(), s.id.clone());
                    return Verdict::Skip;
                }
                Verdict::Defer => verdict = Verdict::Defer,
                Verdict::Report => {}
            }
        }
        verdict
    }
}

impl Span {
    pub fn effective_end(&self) -> u64 {
        self.end_ms
            .unwrap_or_else(|| self.seen_ms.saturating_add(LEASE_MS))
    }

    /// Open spans past their lease wait until a refresh could have landed
    /// (a suspended launcher refreshes again soon after wake).
    fn judge(&self, ts_ms: u64, now_ms: u64) -> Verdict {
        if ts_ms < self.start_ms {
            Verdict::Report
        } else if ts_ms < self.effective_end() {
            Verdict::Skip
        } else if self.end_ms.is_none()
            && now_ms < ts_ms.saturating_add(LEASE_MS + REFRESH.as_millis() as u64)
        {
            Verdict::Defer
        } else {
            Verdict::Report
        }
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

fn same_path(a: &Path, b: &Path) -> bool {
    if cfg!(any(windows, target_os = "macos")) {
        a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
    } else {
        a == b
    }
}

fn write(path: &Path, span: &Span) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
    std::fs::write(
        &tmp,
        serde_json::to_vec(span).map_err(std::io::Error::other)?,
    )?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const U: &str = "0b9d6f7e-4f1c-4d5e-9a7b-3c2d1e0f9a8b";

    fn span(id: &str, sid: Option<&str>, start: u64, seen: u64, end: Option<u64>) -> Span {
        Span {
            cwd: "/w/proj".into(),
            end_ms: end,
            id: id.into(),
            pid: 1,
            seen_ms: seen,
            session_id: sid.map(str::to_string),
            start_ms: start,
        }
    }

    fn routed(spans: Vec<Span>) -> Routed {
        Routed {
            loaded_ms: u64::MAX,
            spans,
        }
    }

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn v(r: &Routed, b: &mut Bindings, sid: &str, cwd: &str, ts: u64, now: u64) -> &'static str {
        match r.judge(b, Some(sid), Some(Path::new(cwd)), ts, now) {
            Verdict::Defer => "defer",
            Verdict::Report => "report",
            Verdict::Skip => "skip",
        }
    }

    #[test]
    fn pin_reads_session_flags() {
        assert!(matches!(Pin::of(&args(&[])), Pin::Inject(_)));
        assert!(matches!(Pin::of(&args(&["-p", "hi"])), Pin::Inject(_)));
        assert_eq!(Pin::of(&args(&["--session-id", U])), Pin::Known(U.into()));
        assert_eq!(
            Pin::of(&args(&[&format!("--session-id={U}")])),
            Pin::Known(U.into())
        );
        assert_eq!(Pin::of(&args(&["-r", U, "-p", "x"])), Pin::Known(U.into()));
        assert_eq!(Pin::of(&args(&["--resume"])), Pin::Unknown);
        assert_eq!(Pin::of(&args(&["--resume", "search"])), Pin::Unknown);
        assert_eq!(Pin::of(&args(&["-c"])), Pin::Unknown);
        assert_eq!(Pin::of(&args(&["-r", U, "--fork-session"])), Pin::Unknown);
        assert_eq!(Pin::of(&args(&["mcp", "list"])), Pin::Unknown);
        assert!(matches!(Pin::of(&args(&["--", "-c"])), Pin::Inject(_)));
    }

    #[test]
    fn pinned_session_is_skipped_inside_its_span_anywhere() {
        let r = routed(vec![span("a", Some("S"), 100, 150, Some(200))]);
        let mut b = Bindings::default();
        assert_eq!(
            v(&r, &mut b, "S", "/elsewhere/after/cd", 150, 1_000),
            "skip"
        );
        assert_eq!(v(&r, &mut b, "S", "/w/proj", 99, 1_000), "report");
        assert_eq!(v(&r, &mut b, "S", "/w/proj", 200, 1_000), "report");
        assert_eq!(v(&r, &mut b, "OTHER", "/w/proj/sub", 150, 1_000), "report");
    }

    #[test]
    fn unpinned_span_binds_its_session_by_exact_cwd() {
        let r = routed(vec![span("a", None, 100, 150, Some(200))]);
        let mut b = Bindings::default();
        assert_eq!(v(&r, &mut b, "P", "/w/proj/sub", 120, 1_000), "report");
        assert_eq!(v(&r, &mut b, "R", "/w/proj", 120, 1_000), "skip");
        assert_eq!(v(&r, &mut b, "R", "/somewhere/else", 130, 1_000), "skip");
        assert_eq!(v(&r, &mut b, "R", "/w/proj", 250, 1_000), "report");
        b.retain_known(&routed(vec![]));
        assert!(b.0.is_empty());
    }

    #[test]
    fn open_span_past_its_lease_defers_then_reports() {
        let r = routed(vec![span("a", Some("S"), 100, 1_000, None)]);
        let mut b = Bindings::default();
        let ts = 1_000 + LEASE_MS + 5;
        assert_eq!(v(&r, &mut b, "S", "/w", 1_000 + LEASE_MS - 1, ts), "skip");
        assert_eq!(v(&r, &mut b, "S", "/w", ts, ts + 1), "defer");
        assert_eq!(
            v(&r, &mut b, "S", "/w", ts, ts + LEASE_MS + 30_000),
            "report"
        );
    }

    #[test]
    fn lines_newer_than_the_snapshot_defer() {
        let r = Routed {
            loaded_ms: 5_000_000,
            spans: vec![],
        };
        let mut b = Bindings::default();
        assert_eq!(v(&r, &mut b, "S", "/w", 4_999_999, 5_000_000), "report");
        assert_eq!(v(&r, &mut b, "S", "/w", 5_000_000, 5_000_000), "defer");
        assert_eq!(
            v(&r, &mut b, "S", "/w", 5_000_000 + LEASE_MS, 5_000_000),
            "defer"
        );
    }

    #[test]
    fn load_prunes_expired_corrupt_and_excess_files() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path(), "http://127.0.0.1:9");
        let d = dir(&cfg);
        std::fs::create_dir_all(&d).unwrap();
        let now = RETENTION_MS * 2;
        write(&d.join("old.json"), &span("", None, 1, 1, Some(2))).unwrap();
        write(
            &d.join("live.json"),
            &span("", None, now - 10, now - 5, None),
        )
        .unwrap();
        std::fs::write(d.join("bad.json"), b"{").unwrap();
        let r = Routed::load_and_prune(&cfg, now);
        assert_eq!(r.spans.len(), 1);
        assert_eq!(
            (r.spans[0].start_ms, r.spans[0].id.as_str()),
            (now - 10, "live")
        );
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
        let guard = Guard::open(&other, Some("S".into())).unwrap();
        let now = now_ms();
        let mut r = Routed::load_and_prune(&daemon, now);
        r.loaded_ms = u64::MAX;
        let mut b = Bindings::default();
        assert!(matches!(
            r.judge(&mut b, Some("S"), None, now, now),
            Verdict::Skip
        ));
        guard.close().await;
        let r = Routed::load_and_prune(&daemon, now_ms());
        assert_eq!(r.spans.len(), 1);
        assert!(r.spans[0].end_ms.is_some());
    }
}
