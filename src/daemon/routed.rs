use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::Config;

/// A line dated further ahead than this is judged now instead of deferred.
pub const FUTURE_MS: u64 = 10 * 60 * 1_000;
pub const LEASE_MS: u64 = 2 * 60 * 1_000;
/// How often a span re-reads which session its Claude Code process is on.
const ID_POLL: Duration = Duration::from_secs(2);
const MAX_BINDINGS: usize = 1_024;
const MAX_ID_LEN: usize = 256;
const MAX_SESSIONS: usize = 64;
const MAX_SPANS: usize = 1_024;
const REFRESH: Duration = Duration::from_secs(30);
const RETENTION_MS: u64 = 35 * 24 * 3_600 * 1_000;
const STALE_TMP: Duration = Duration::from_secs(3_600);
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

/// Which Claude Code session a `dense claude` launch runs, when its argv says.
#[derive(Debug, PartialEq, Eq)]
pub enum Pin {
    /// The user named it (`--session-id`, `--resume <uuid>`).
    Known(String),
    /// New, `--continue`, the resume picker, teleport: learned from the process.
    Unknown,
}

pub enum Verdict {
    /// Not yet decidable (a span may still be refreshed); re-read next tick.
    Defer,
    Report,
    Skip,
}

/// Sessions bound by launch folder to a span that never learned its session
/// ids (a Claude Code too old to publish them). Persisted with the cursors.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Bindings {
    #[serde(default)]
    sessions: BTreeMap<String, Bound>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Bound {
    at_ms: u64,
    span: String,
}

/// A live span owned by the launching `dense claude`. Call [`Guard::close`]
/// when the child exits.
pub struct Guard {
    path: PathBuf,
    refresh: tokio::task::JoinHandle<()>,
    session_file: Option<PathBuf>,
    span: Arc<Mutex<Span>>,
}

#[derive(Debug)]
pub struct Routed {
    /// False when the span dir could not be fully read: bindings to missing
    /// spans must then be kept.
    pub complete: bool,
    /// When the spans were read: a span opened after this is not in `spans`.
    pub loaded_ms: u64,
    pub spans: Vec<Span>,
}

/// One assistant line as the span judge sees it.
pub struct Seen<'a> {
    pub cwd: Option<&'a Path>,
    pub session: Option<&'a str>,
    pub ts_ms: u64,
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
    /// Session ids the launched process was seen on (`/clear` starts another).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<String>,
    pub start_ms: u64,
}

/// Claude Code's `<claude home>/sessions/<pid>.json`, as far as a span reads it.
#[derive(Deserialize)]
struct LiveSession {
    #[serde(rename = "sessionId")]
    session_id: String,
}

impl Bindings {
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// Drop bindings to spans that were pruned. Returns whether any went.
    pub fn retain_known(&mut self, routed: &Routed) -> bool {
        if !routed.complete {
            return false;
        }
        let before = self.len();
        let known = |id: &String| routed.spans.iter().any(|s| &s.id == id);
        self.sessions.retain(|_, b| known(&b.span));
        self.len() != before
    }

    fn bind(&mut self, session: &str, span: &str, at_ms: u64) {
        self.sessions.insert(
            session.to_string(),
            Bound {
                at_ms,
                span: span.to_string(),
            },
        );
        while self.sessions.len() > MAX_BINDINGS {
            let Some(oldest) = self
                .sessions
                .iter()
                .min_by_key(|(_, b)| b.at_ms)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            self.sessions.remove(&oldest);
        }
    }
}

impl Guard {
    /// Open a span for `child`, the Claude Code process just launched, after
    /// the spawn so its pid can be followed.
    pub fn open(cfg: &Config, session_id: Option<String>, child: Option<u32>) -> Option<Self> {
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
            sessions: Vec::new(),
            start_ms: now,
        };
        if let Err(e) = std::fs::create_dir_all(&dir).and_then(|()| write(&path, &span)) {
            tracing::debug!("could not record routed span: {e}");
            return None;
        }
        let session_file = child.map(|pid| {
            crate::harness::commands::claude_home(cfg.home(), |k| std::env::var(k).ok())
                .join("sessions")
                .join(format!("{pid}.json"))
        });
        let span = Arc::new(Mutex::new(span));
        let refresh = {
            let (path, span, file) = (path.clone(), Arc::clone(&span), session_file.clone());
            tokio::spawn(async move {
                let mut written = Instant::now();
                loop {
                    tokio::time::sleep(ID_POLL).await;
                    let mut span = lock(&span);
                    let learned = file.as_deref().is_some_and(|f| learn(&mut span, f));
                    if learned || written.elapsed() >= REFRESH {
                        span.seen_ms = now_ms();
                        let _ = write(&path, &span);
                        written = Instant::now();
                    }
                }
            })
        };
        Some(Self {
            path,
            refresh,
            session_file,
            span,
        })
    }

    pub async fn close(self) {
        let Self {
            path,
            refresh,
            session_file,
            span,
        } = self;
        refresh.abort();
        let _ = refresh.await;
        let mut span = lock(&span);
        if let Some(f) = &session_file {
            learn(&mut span, f);
        }
        let now = now_ms();
        span.seen_ms = now;
        span.end_ms = Some(now);
        let _ = write(&path, &span);
    }
}

impl Pin {
    pub fn of(args: &[String]) -> Self {
        let mut fork = false;
        let mut resume = None;
        let mut unknown = false;
        let mut picker = false;
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
                    _ => picker = true,
                },
                "-c" | "--continue" => picker = true,
                "--fork-session" => fork = true,
                "--teleport" | "--from-pr" => unknown = true,
                // A cluster or attached value (`-cp`, `-r<id>`) may hide a resume.
                short if !short.starts_with("--") && short.starts_with('-') => {
                    unknown |= short.contains(['c', 'r']);
                }
                _ => {}
            }
        }
        // A fork runs under a new id, learned from the process like any other.
        match resume {
            Some(v) if !(unknown || picker || fork) => Self::Known(v),
            _ => Self::Unknown,
        }
    }
}

impl Default for Routed {
    fn default() -> Self {
        Self {
            complete: true,
            loaded_ms: u64::MAX,
            spans: Vec::new(),
        }
    }
}

impl Routed {
    /// Load every span, deleting expired files and the oldest beyond the cap.
    /// Any read error leaves every file in place for this tick.
    pub fn load_and_prune(cfg: &Config, now_ms: u64) -> Self {
        let dir = dir(cfg);
        let mut complete = true;
        let entries: Vec<PathBuf> = match std::fs::read_dir(&dir) {
            Ok(rd) => rd
                .filter_map(|e| e.inspect_err(|_| complete = false).ok())
                .map(|e| e.path())
                .collect(),
            Err(e) => {
                complete = e.kind() == std::io::ErrorKind::NotFound;
                Vec::new()
            }
        };
        let mut spans: Vec<(PathBuf, Span)> = Vec::new();
        for p in entries {
            if p.extension().is_some_and(|x| x == "tmp") {
                sweep_stale_tmp(&p);
                continue;
            }
            if p.extension().is_none_or(|x| x != "json") {
                continue;
            }
            let Ok(bytes) = std::fs::read(&p) else {
                complete = false;
                continue;
            };
            match serde_json::from_slice::<Span>(&bytes) {
                Ok(mut s) => {
                    if s.id.is_empty()
                        && let Some(stem) = p.file_stem()
                    {
                        s.id = stem.to_string_lossy().into_owned();
                    }
                    spans.push((p, s));
                }
                Err(_) => {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
        spans.sort_by_key(|(_, s)| std::cmp::Reverse(s.start_ms));
        let mut kept = Vec::new();
        for (i, (path, span)) in spans.into_iter().enumerate() {
            let expired =
                i >= MAX_SPANS || span.effective_end().saturating_add(RETENTION_MS) < now_ms;
            if expired && complete {
                let _ = std::fs::remove_file(&path);
            } else {
                kept.push(span);
            }
        }
        Self {
            complete,
            loaded_ms: now_ms,
            spans: kept,
        }
    }

    /// Decide one assistant line: skip it when its session ran under a
    /// `dense claude` span at its timestamp.
    pub fn judge(&self, bindings: &mut Bindings, line: &Seen, now_ms: u64) -> Verdict {
        let ts_ms = line.ts_ms;
        let future = ts_ms > now_ms.saturating_add(FUTURE_MS);
        let verdict = self.judge_spans(bindings, line, now_ms);
        match verdict {
            Verdict::Defer if future => Verdict::Report,
            Verdict::Report if ts_ms >= self.loaded_ms && !future => Verdict::Defer,
            v => v,
        }
    }

    fn judge_spans(&self, bindings: &mut Bindings, line: &Seen, now_ms: u64) -> Verdict {
        let ts_ms = line.ts_ms;
        let Some(session) = line.session else {
            return Verdict::Report;
        };
        let bound = bindings.sessions.get(session).map(|b| b.span.clone());
        let mut verdict = Verdict::Report;
        let owners: Vec<&Span> = self
            .spans
            .iter()
            .filter(|s| s.names(session) || bound.as_ref() == Some(&s.id))
            .collect();
        for s in &owners {
            match s.judge(ts_ms, now_ms) {
                Verdict::Skip => return Verdict::Skip,
                Verdict::Defer => verdict = Verdict::Defer,
                Verdict::Report => {}
            }
        }
        if bound.is_some() || !owners.is_empty() || !matches!(verdict, Verdict::Report) {
            return verdict;
        }
        if self.spans.iter().any(|s| s.may_name(ts_ms, now_ms)) {
            return Verdict::Defer;
        }
        // A Claude Code that publishes no session ids: bound by launch folder.
        let Some(cwd) = line.cwd else {
            return verdict;
        };
        let blind = self
            .spans
            .iter()
            .filter(|s| s.knows_no_session() && same_path(&s.cwd, cwd));
        for s in blind {
            match s.judge(ts_ms, now_ms) {
                Verdict::Skip => {
                    bindings.bind(session, &s.id, ts_ms);
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

    /// Open, and its last refresh is older than the line: the process may
    /// yet report it was on that line's session.
    fn may_name(&self, ts_ms: u64, now_ms: u64) -> bool {
        self.end_ms.is_none()
            && self.start_ms <= ts_ms
            && self.seen_ms <= ts_ms
            && now_ms < self.seen_ms.saturating_add(LEASE_MS)
    }

    fn knows_no_session(&self) -> bool {
        self.session_id.is_none() && self.sessions.is_empty()
    }

    fn names(&self, session: &str) -> bool {
        self.session_id.as_deref() == Some(session) || self.sessions.iter().any(|s| s == session)
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

/// Machine-wide, not per profile: one daemon reads every profile's spans.
fn dir(cfg: &Config) -> PathBuf {
    cfg.data_dir().join("routed")
}

/// Note the session `file` names now. Returns whether it is a new one.
fn learn(span: &mut Span, file: &Path) -> bool {
    let id = std::fs::read(file)
        .ok()
        .and_then(|b| serde_json::from_slice::<LiveSession>(&b).ok())
        .map(|l| l.session_id);
    match id {
        Some(id)
            if id.len() <= MAX_ID_LEN && !span.names(&id) && span.sessions.len() < MAX_SESSIONS =>
        {
            span.sessions.push(id);
            true
        }
        _ => false,
    }
}

fn lock(span: &Mutex<Span>) -> MutexGuard<'_, Span> {
    span.lock().unwrap_or_else(PoisonError::into_inner)
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

/// Claude Code's project-dir name for a cwd: every UTF-16 unit that is not
/// ASCII alphanumeric becomes `-` (longer names are cut and hashed).
fn sweep_stale_tmp(path: &Path) {
    let old = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age > STALE_TMP);
    if old {
        let _ = std::fs::remove_file(path);
    }
}

fn write(path: &Path, span: &Span) -> std::io::Result<()> {
    crate::atomic::replace(
        path,
        &serde_json::to_vec(span).map_err(std::io::Error::other)?,
    )
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
            sessions: Vec::new(),
            start_ms: start,
        }
    }

    fn learned(id: &str, sessions: &[&str], start: u64, seen: u64, end: Option<u64>) -> Span {
        Span {
            sessions: sessions.iter().map(|s| s.to_string()).collect(),
            ..span(id, None, start, seen, end)
        }
    }

    fn routed(spans: Vec<Span>) -> Routed {
        Routed {
            complete: true,
            loaded_ms: u64::MAX,
            spans,
        }
    }

    fn seen<'a>(sid: &'a str, cwd: &'a str, ts: u64) -> Seen<'a> {
        Seen {
            cwd: Some(Path::new(cwd)),
            session: Some(sid),
            ts_ms: ts,
        }
    }

    fn name(v: Verdict) -> &'static str {
        match v {
            Verdict::Defer => "defer",
            Verdict::Report => "report",
            Verdict::Skip => "skip",
        }
    }

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn v(r: &Routed, b: &mut Bindings, sid: &str, cwd: &str, ts: u64, now: u64) -> &'static str {
        name(r.judge(b, &seen(sid, cwd, ts), now))
    }

    #[test]
    fn a_resume_hidden_in_a_short_flag_is_not_taken_as_a_session_id() {
        for flags in [
            &["-cp", "hi"][..],
            &["-pc", "hi"],
            &["-rp", "hi"],
            &["-pr", "hi"],
        ] {
            assert_eq!(Pin::of(&args(flags)), Pin::Unknown, "{flags:?}");
        }
        assert_eq!(Pin::of(&args(&[&format!("-r{U}")])), Pin::Unknown);
        assert_eq!(Pin::of(&args(&[&format!("-r={U}")])), Pin::Unknown);
    }

    #[test]
    fn pin_reads_session_flags() {
        assert_eq!(Pin::of(&args(&[])), Pin::Unknown);
        assert_eq!(Pin::of(&args(&["-p", "hi"])), Pin::Unknown);
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
        assert_eq!(Pin::of(&args(&["--", "-c"])), Pin::Unknown);
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
    fn a_clear_is_skipped_by_the_session_id_the_process_reported() {
        let r = routed(vec![learned("a", &["S", "Y"], 100, 500, Some(1_000))]);
        let mut b = Bindings::default();
        assert_eq!(
            v(&r, &mut b, "Y", "/somewhere/after/cd", 300, 2_000),
            "skip"
        );
        assert_eq!(v(&r, &mut b, "Y", "/w/proj", 1_200, 2_000), "report");
    }

    #[test]
    fn plain_claude_in_the_same_folder_during_a_span_is_reported() {
        let r = routed(vec![learned("a", &["S"], 100, 500, Some(1_000))]);
        let mut b = Bindings::default();
        assert_eq!(v(&r, &mut b, "P", "/w/proj", 300, 2_000), "report");
        assert_eq!(b.len(), 0);
        let pinned = routed(vec![span("a", Some("S"), 100, 500, Some(1_000))]);
        assert_eq!(v(&pinned, &mut b, "P", "/w/proj", 300, 2_000), "report");
    }

    #[test]
    fn an_unknown_session_waits_until_an_open_span_has_reported_past_it() {
        let r = routed(vec![learned("a", &["S"], 100, 500, None)]);
        let mut b = Bindings::default();
        assert_eq!(v(&r, &mut b, "P", "/w/proj", 600, 700), "defer");
        assert_eq!(v(&r, &mut b, "P", "/w/proj", 400, 700), "report");
        assert_eq!(v(&r, &mut b, "P", "/w/proj", 600, 500 + LEASE_MS), "report");
    }

    #[test]
    fn a_span_that_never_learned_a_session_binds_by_exact_cwd() {
        let r = routed(vec![span("a", None, 100, 150, Some(200))]);
        let mut b = Bindings::default();
        assert_eq!(v(&r, &mut b, "P", "/w/proj/sub", 120, 1_000), "report");
        assert_eq!(v(&r, &mut b, "R", "/w/proj", 120, 1_000), "skip");
        assert_eq!(v(&r, &mut b, "R", "/somewhere/else", 130, 1_000), "skip");
        assert_eq!(v(&r, &mut b, "R", "/w/proj", 250, 1_000), "report");
        b.retain_known(&routed(vec![]));
        assert_eq!(b.len(), 0);
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
    fn far_future_lines_are_judged_now() {
        let r = Routed {
            complete: true,
            loaded_ms: 1_000,
            spans: vec![span("a", Some("S"), 100, 500, None)],
        };
        let mut b = Bindings::default();
        let far = 1_000 + FUTURE_MS + 1;
        assert_eq!(v(&r, &mut b, "P", "/w", far, 1_000), "report");
        assert_eq!(v(&r, &mut b, "S", "/w", far, 1_000), "report");
        assert_eq!(v(&r, &mut b, "P", "/w", 1_500, 1_000), "defer");
    }

    #[test]
    fn bindings_survive_an_incomplete_span_read_and_evict_oldest_first() {
        let mut b = Bindings::default();
        for i in 0..MAX_BINDINGS as u64 + 1 {
            b.bind(&format!("s{}", MAX_BINDINGS as u64 - i), "a", i);
        }
        assert_eq!(b.sessions.len(), MAX_BINDINGS);
        assert!(!b.sessions.contains_key(&format!("s{MAX_BINDINGS}")));
        assert!(b.sessions.contains_key("s0"));
        let mut broken = routed(vec![]);
        broken.complete = false;
        assert!(!b.retain_known(&broken));
        assert_eq!(b.sessions.len(), MAX_BINDINGS);
    }

    #[test]
    fn lines_newer_than_the_snapshot_defer() {
        let r = Routed {
            complete: true,
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
    fn learn_records_each_new_session_once_and_ignores_junk() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("42.json");
        let mut s = span("a", Some("S"), 0, 0, None);
        assert!(!learn(&mut s, &f));
        std::fs::write(&f, r#"{"pid":42,"sessionId":"S"}"#).unwrap();
        assert!(!learn(&mut s, &f));
        std::fs::write(&f, r#"{"pid":42,"sessionId":"Y","peerToken":"x"}"#).unwrap();
        assert!(learn(&mut s, &f));
        assert!(!learn(&mut s, &f));
        std::fs::write(
            &f,
            format!(r#"{{"sessionId":"{}"}}"#, "x".repeat(MAX_ID_LEN + 1)),
        )
        .unwrap();
        assert!(!learn(&mut s, &f));
        std::fs::write(&f, b"{").unwrap();
        assert!(!learn(&mut s, &f));
        assert_eq!(s.sessions, ["Y"]);
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
    async fn a_span_follows_its_process_through_a_clear() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = Config::for_test(tmp.path(), "http://127.0.0.1:9");
        let other = Config::for_test(tmp.path(), "http://127.0.0.2:9");
        assert_ne!(daemon.profile(), other.profile());
        let sessions = tmp.path().join(".claude").join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let live = sessions.join("4242.json");
        std::fs::write(&live, r#"{"pid":4242,"sessionId":"S"}"#).unwrap();
        let guard = Guard::open(&other, None, Some(4242)).unwrap();
        tokio::time::sleep(ID_POLL + Duration::from_millis(500)).await;
        std::fs::write(&live, r#"{"pid":4242,"sessionId":"Y"}"#).unwrap();
        guard.close().await;
        let r = Routed::load_and_prune(&daemon, now_ms());
        assert_eq!(r.spans.len(), 1);
        assert!(r.spans[0].end_ms.is_some());
        assert_eq!(r.spans[0].sessions, ["S", "Y"]);
        let at = r.spans[0].start_ms;
        let mut b = Bindings::default();
        assert_eq!(v(&r, &mut b, "Y", "/x", at, at + LEASE_MS * 2), "skip");
        assert_eq!(v(&r, &mut b, "P", "/x", at, at + LEASE_MS * 2), "report");
    }
}
