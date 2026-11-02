//! Spans in which `dense claude` routed a Claude Code process through the
//! proxy. The proxy already reports that usage, so the session reader skips
//! those sessions' lines inside the span. One file per launch under
//! `<data dir>/routed/`, shared by every profile since one daemon serves the
//! machine. The launcher refreshes `seen_ms` while alive, so a crashed
//! launcher's span ends at its last refresh plus [`LEASE_MS`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::Config;

/// A line dated further ahead than this is judged now instead of deferred.
pub const FUTURE_MS: u64 = 10 * 60 * 1_000;
pub const LEASE_MS: u64 = 2 * 60 * 1_000;
const MAX_BINDINGS: usize = 1_024;
const MAX_PROJECTS: usize = 8;
/// Claude Code's project-dir slug keeps this many characters before hashing.
const SLUG_MAX: usize = 200;
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

/// Which Claude Code session a `dense claude` launch runs.
#[derive(Debug, PartialEq, Eq)]
pub enum Pin {
    /// None named: pass `--session-id <uuid>` so the reader can match on it.
    Inject(String),
    /// The user named it (`--session-id`, `--resume <uuid>`).
    Known(String),
    /// `--continue`, the resume picker, teleport: bound by the launch cwd instead.
    Unknown,
}

pub enum Verdict {
    /// Not yet decidable (a span may still be refreshed); re-read next tick.
    Defer,
    Report,
    Skip,
}

/// Sessions bound to a span they were not pinned to (an unpinned launch's
/// session, or a new session a `/clear` started inside a span), and the
/// project dirs each span's sessions were seen in. Persisted with the cursors.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Bindings {
    #[serde(default)]
    projects: BTreeMap<String, BTreeSet<String>>,
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
    span: Span,
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
    /// Timestamp of the first assistant line of the file this line is in.
    pub file_first_ms: u64,
    /// `projects/<dir>` of a top-level session file (not a subagent's).
    pub project: Option<&'a str>,
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
    pub start_ms: u64,
}

impl Bindings {
    fn has_project(&self, span: &str, project: &str) -> bool {
        self.projects.get(span).is_some_and(|s| s.contains(project))
    }

    pub fn len(&self) -> usize {
        self.sessions.len() + self.projects.values().map(BTreeSet::len).sum::<usize>()
    }

    /// Drop bindings to spans that were pruned. Returns whether any went.
    pub fn retain_known(&mut self, routed: &Routed) -> bool {
        if !routed.complete {
            return false;
        }
        let before = self.len();
        let known = |id: &String| routed.spans.iter().any(|s| &s.id == id);
        self.sessions.retain(|_, b| known(&b.span));
        self.projects.retain(|id, _| known(id));
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

    fn note_project(&mut self, span: &str, project: &str) {
        let set = self.projects.entry(span.to_string()).or_default();
        if set.len() < MAX_PROJECTS {
            set.insert(project.to_string());
        }
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
        // Claude accepts `--session-id` with `--resume`/`--continue` only for a fork.
        match resume {
            _ if unknown || (picker && !fork) => Self::Unknown,
            Some(_) if fork => Self::Inject(uuid::Uuid::new_v4().to_string()),
            Some(v) => Self::Known(v),
            None => Self::Inject(uuid::Uuid::new_v4().to_string()),
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
            .filter(|s| s.session_id.as_deref() == Some(session) || bound.as_ref() == Some(&s.id))
            .collect();
        for s in &owners {
            match s.judge(ts_ms, now_ms) {
                Verdict::Skip => {
                    if let Some(p) = line.project {
                        bindings.note_project(&s.id, p);
                    }
                    return Verdict::Skip;
                }
                Verdict::Defer => verdict = Verdict::Defer,
                Verdict::Report => {}
            }
        }
        if bound.is_some() || !owners.is_empty() || !matches!(verdict, Verdict::Report) {
            return verdict;
        }
        // A session born inside a span in that span's project dir: `/clear`
        // (or plan mode's clear) gave the routed process a new session id.
        if let Some(project) = line.project {
            for s in &self.spans {
                let has_session =
                    s.session_id.is_some() || bindings.sessions.values().any(|b| b.span == s.id);
                if !has_session
                    || line.file_first_ms < s.start_ms
                    || !(s.project_matches(project) || bindings.has_project(&s.id, project))
                {
                    continue;
                }
                match s.judge(line.file_first_ms, now_ms) {
                    Verdict::Skip => {
                        bindings.bind(session, &s.id, ts_ms);
                        return s.judge(ts_ms, now_ms);
                    }
                    Verdict::Defer => verdict = Verdict::Defer,
                    Verdict::Report => {}
                }
            }
        }
        let Some(cwd) = line.cwd else {
            return verdict;
        };
        let unpinned = self
            .spans
            .iter()
            .filter(|s| s.session_id.is_none() && same_path(&s.cwd, cwd));
        for s in unpinned {
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

    /// Whether `project` is Claude Code's `projects/<dir>` for the launch cwd.
    fn project_matches(&self, project: &str) -> bool {
        let slug = slug(&self.cwd.to_string_lossy());
        let fold = |s: &str| {
            if cfg!(any(windows, target_os = "macos")) {
                s.to_lowercase()
            } else {
                s.to_string()
            }
        };
        let (slug, project) = (fold(&slug), fold(project));
        if slug.len() <= SLUG_MAX {
            slug == project
        } else {
            project.starts_with(&format!("{}-", &slug[..SLUG_MAX]))
        }
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

/// Claude Code's project-dir name for a cwd: every UTF-16 unit that is not
/// ASCII alphanumeric becomes `-` (longer names are cut and hashed).
fn slug(cwd: &str) -> String {
    cwd.chars()
        .flat_map(|c| {
            let n = if c.is_ascii_alphanumeric() {
                0
            } else {
                c.len_utf16()
            };
            std::iter::repeat_n('-', n).chain((n == 0).then_some(c))
        })
        .collect()
}

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
            complete: true,
            loaded_ms: u64::MAX,
            spans,
        }
    }

    fn seen<'a>(
        sid: &'a str,
        cwd: &'a str,
        project: Option<&'a str>,
        first: u64,
        ts: u64,
    ) -> Seen<'a> {
        Seen {
            cwd: Some(Path::new(cwd)),
            file_first_ms: first,
            project,
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
        name(r.judge(b, &seen(sid, cwd, None, ts, ts), now))
    }

    #[test]
    fn a_resume_hidden_in_a_short_flag_is_never_given_a_session_id() {
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
        assert!(matches!(
            Pin::of(&args(&["-r", U, "--fork-session"])),
            Pin::Inject(_)
        ));
        assert!(matches!(
            Pin::of(&args(&["--continue", "--fork-session"])),
            Pin::Inject(_)
        ));
        assert!(matches!(
            Pin::of(&args(&["--resume", "--fork-session"])),
            Pin::Inject(_)
        ));
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
    fn clear_inside_a_pinned_span_binds_the_new_session_by_project() {
        let r = routed(vec![span("a", Some("S"), 100, 500, Some(1_000))]);
        let mut b = Bindings::default();
        let proj = Some("-w-proj");
        let at = |sid, project, first, ts| seen(sid, "/w/proj", project, first, ts);
        assert_eq!(
            name(r.judge(&mut b, &at("Y", proj, 300, 300), 2_000)),
            "skip"
        );
        assert_eq!(
            name(r.judge(&mut b, &at("Y", proj, 300, 900), 2_000)),
            "skip"
        );
        assert_eq!(
            name(r.judge(&mut b, &at("Y", proj, 300, 1_200), 2_000)),
            "report"
        );
        assert_eq!(
            name(r.judge(&mut b, &at("Z", Some("-w-other"), 300, 300), 2_000)),
            "report"
        );
        assert_eq!(
            name(r.judge(&mut b, &at("Q", proj, 50, 300), 2_000)),
            "report"
        );
        assert_eq!(
            name(r.judge(&mut b, &at("Q", None, 300, 300), 2_000)),
            "report"
        );
    }

    #[test]
    fn clear_after_a_cd_binds_by_a_project_the_span_was_seen_in() {
        let r = routed(vec![span("a", Some("S"), 100, 500, Some(1_000))]);
        let mut b = Bindings::default();
        let elsewhere = |sid, first, ts| seen(sid, "/x", Some("-x"), first, ts);
        assert_eq!(
            name(r.judge(&mut b, &elsewhere("Y", 300, 300), 2_000)),
            "report"
        );
        assert_eq!(
            name(r.judge(&mut b, &elsewhere("S", 150, 150), 2_000)),
            "skip"
        );
        assert_eq!(
            name(r.judge(&mut b, &elsewhere("Y2", 400, 400), 2_000)),
            "skip"
        );
    }

    #[test]
    fn plain_claude_started_in_the_project_before_the_span_is_reported() {
        let r = routed(vec![span("a", Some("S"), 100, 500, Some(1_000))]);
        let mut b = Bindings::default();
        let line = seen("P", "/w/proj", Some("-w-proj"), 90, 300);
        assert_eq!(name(r.judge(&mut b, &line, 2_000)), "report");
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
        assert_eq!(
            name(r.judge(&mut b, &seen("P", "/w", None, far, far), 1_000)),
            "report"
        );
        assert_eq!(
            name(r.judge(&mut b, &seen("S", "/w", None, far, far), 1_000)),
            "report"
        );
        assert_eq!(
            name(r.judge(&mut b, &seen("P", "/w", None, 1_500, 1_500), 1_000)),
            "defer"
        );
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
    fn slug_matches_claude_project_dirs() {
        assert_eq!(slug("/home/u/src/my.repo"), "-home-u-src-my-repo");
        assert_eq!(slug("C:\\Users\\u"), "C--Users-u");
        assert_eq!(slug("/tmp/é"), "-tmp--");
        let long = format!("/{}", "a".repeat(300));
        let s = span("a", Some("S"), 0, 0, None);
        let s = Span {
            cwd: long.clone().into(),
            ..s
        };
        assert!(s.project_matches(&format!("-{}-1abc", "a".repeat(SLUG_MAX - 1))));
        assert!(!s.project_matches("-aaa"));
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
            r.judge(
                &mut b,
                &Seen {
                    cwd: None,
                    file_first_ms: now,
                    project: None,
                    session: Some("S"),
                    ts_ms: now,
                },
                now
            ),
            Verdict::Skip
        ));
        guard.close().await;
        let r = Routed::load_and_prune(&daemon, now_ms());
        assert_eq!(r.spans.len(), 1);
        assert!(r.spans[0].end_ms.is_some());
    }
}
