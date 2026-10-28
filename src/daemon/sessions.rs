//! Offline usage: while no Condense routing is applied nothing reaches the proxy, so the
//! daemon reads token counts from Claude Code's local session files
//! (`<claude home>/projects/**/*.jsonl`) and queues them for upload. Lines
//! deserialize into [`Line`], which has no field that can hold message content.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::queue::{Queue, UsageRecord};
use super::routed::{Bindings, Routed, Verdict};
use super::switch::Mode;
use crate::Result;
use crate::config::Config;
use crate::error::Context;
use crate::harness::commands;

const HEAD_BYTES: u64 = 256;
const MAX_ID: usize = 256;
const MAX_LINE: usize = 16 << 20;
const MAX_PENDING: usize = 4_096;
const MAX_READ_PER_TICK: u64 = 64 << 20;
const MAX_ROOTS: usize = 16;
const MAX_WINDOWS: usize = 256;
const RECENT_IDS: usize = 64;
/// A message with no `stop_reason` line yet (aborted, or still streaming) is
/// reported with its largest counts once its last line is this old.
const SETTLE_MS: u64 = 10 * 60 * 1_000;
const SYNTHETIC_MODEL: &str = "<synthetic>";
const WALK_DEPTH: usize = 4;
const WINDOW_RETENTION_MS: u64 = 35 * 24 * 3_600 * 1_000;

/// Per-file read positions, persisted at `<daemon dir>/sessions.json`.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Cursors {
    #[serde(default)]
    bindings: Bindings,
    files: BTreeMap<String, Cursor>,
}

/// What the reader may report: inside a transparent window and outside every
/// `dense claude` routed span.
pub struct Filter<'a> {
    pub routed: &'a Routed,
    pub windows: &'a Windows,
}

/// A span (unix ms) during which the daemon had the machine `transparent`.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Window {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_ms: Option<u64>,
    pub start_ms: u64,
}

/// Transparent windows, oldest first; only the last can be open.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Windows(Vec<Window>);

struct Ctx<'a> {
    bindings: &'a mut Bindings,
    filter: &'a Filter<'a>,
    now_ms: u64,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct Cursor {
    /// sha256 of the first `head_len` bytes: a mismatch means the file was replaced.
    head: String,
    head_len: u64,
    mtime_ms: u64,
    offset: u64,
    pending: BTreeMap<String, Pending>,
    recent: VecDeque<String>,
    size: u64,
}

#[derive(Deserialize)]
struct Line {
    cwd: Option<PathBuf>,
    message: Option<Message>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    timestamp: Option<String>,
    #[serde(rename = "type")]
    type_: Option<String>,
}

#[derive(Deserialize)]
struct Message {
    id: Option<String>,
    model: Option<String>,
    stop_reason: Option<IgnoredAny>,
    usage: Option<Usage>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct Pending {
    cache_read: u64,
    cache_write: u64,
    first_at: String,
    first_ms: u64,
    input_tokens: u64,
    last_ms: u64,
    model: String,
    output_tokens: u64,
    session_ref: String,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
}

impl Cursors {
    pub fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ctx(format!("creating {}", parent.display()))?;
        }
        let body = serde_json::to_vec(self).ctx("serializing session cursors")?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, body).ctx(format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).ctx(format!("replacing {}", path.display()))
    }
}

impl Windows {
    /// Close the open window (if any) at `at_ms`.
    pub fn close(&mut self, at_ms: u64) {
        if let Some(w) = self.0.last_mut().filter(|w| w.end_ms.is_none()) {
            w.end_ms = Some(at_ms.max(w.start_ms));
        }
    }

    pub fn contains(&self, ts_ms: u64) -> bool {
        self.0
            .iter()
            .any(|w| w.start_ms <= ts_ms && w.end_ms.is_none_or(|end| ts_ms < end))
    }

    pub fn earliest(&self) -> Option<u64> {
        self.0.first().map(|w| w.start_ms)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Record the mode the daemon just applied.
    pub fn note(&mut self, mode: Mode, now_ms: u64) {
        match mode {
            Mode::Condensing => self.close(now_ms),
            Mode::Transparent => {
                if self.0.last().is_none_or(|w| w.end_ms.is_some()) {
                    self.0.push(Window {
                        end_ms: None,
                        start_ms: now_ms,
                    });
                }
            }
        }
    }

    pub fn prune(&mut self, now_ms: u64) {
        self.0
            .retain(|w| w.end_ms.is_none_or(|e| e + WINDOW_RETENTION_MS >= now_ms));
        let excess = self.0.len().saturating_sub(MAX_WINDOWS);
        self.0.drain(..excess);
    }
}

/// Scan the session files and queue every new in-window message. Returns how
/// many records were queued.
pub fn report(cfg: &Config, windows: &Windows, now_ms: u64) -> Result<usize> {
    let routed = Routed::load_and_prune(cfg, now_ms);
    if windows.is_empty() {
        return Ok(0);
    }
    let path = cfg.data_dir().join("daemon").join("sessions.json");
    let mut cursors = Cursors::load(&path);
    let mut roots = roots(cfg.home(), |k| std::env::var(k).ok());
    for dir in remembered_roots(cfg) {
        let p = dir.join("projects");
        if !roots.contains(&p) {
            roots.push(p);
        }
    }
    let filter = Filter {
        routed: &routed,
        windows,
    };
    let (records, dirty) = scan(&roots, &mut cursors, &filter, now_ms);
    Queue::open(cfg).push(&records)?;
    if dirty {
        cursors.save(&path)?;
    }
    Ok(records.len())
}

/// The `projects` dirs Claude Code writes sessions to: `$CLAUDE_CONFIG_DIR`
/// (else `~/.claude`), plus the default and XDG locations, deduplicated.
pub fn roots(home: &Path, env: impl Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    let xdg = env("XDG_CONFIG_HOME")
        .filter(|v| Path::new(v).is_absolute())
        .map_or_else(|| home.join(".config"), PathBuf::from);
    let candidates = [
        commands::claude_home(home, &env),
        home.join(".claude"),
        xdg.join("claude"),
    ];
    let mut seen = BTreeSet::new();
    candidates
        .into_iter()
        .map(|p| p.join("projects"))
        .filter(|p| seen.insert(std::fs::canonicalize(p).unwrap_or_else(|_| p.clone())))
        .collect()
}

/// Remember this process's `CLAUDE_CONFIG_DIR` for the daemon, whose
/// supervisor environment does not carry it.
pub fn remember_root(cfg: &Config) {
    let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    else {
        return;
    };
    let mut known = remembered_roots(cfg);
    if known.contains(&dir) {
        return;
    }
    known.push(dir);
    let excess = known.len().saturating_sub(MAX_ROOTS);
    known.drain(..excess);
    let body: String = known
        .iter()
        .filter_map(|p| p.to_str())
        .map(|p| format!("{p}\n"))
        .collect();
    let path = roots_file(cfg);
    let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
    let written = std::fs::create_dir_all(cfg.data_dir())
        .and_then(|()| std::fs::write(&tmp, body))
        .and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        tracing::debug!("could not remember CLAUDE_CONFIG_DIR: {e}");
    }
}

/// Read new bytes of every session file under `roots`. Returns the settled
/// in-window records and whether any cursor changed.
pub fn scan(
    roots: &[PathBuf],
    cursors: &mut Cursors,
    filter: &Filter,
    now_ms: u64,
) -> (Vec<UsageRecord>, bool) {
    let mut files = Vec::new();
    for root in roots {
        walk(root, WALK_DEPTH, &mut files);
    }
    files.sort();
    files.dedup();
    let mut out = Vec::new();
    let mut dirty = false;
    let mut live = BTreeSet::new();
    dirty |= cursors.bindings.retain_known(filter.routed);
    for path in files {
        let key = path.to_string_lossy().into_owned();
        let cursor = cursors.files.entry(key.clone()).or_default();
        let mut ctx = Ctx {
            bindings: &mut cursors.bindings,
            filter,
            now_ms,
        };
        let bound = ctx.bindings.len();
        match read_file(&path, cursor, &mut ctx, &mut out) {
            Ok(changed) => dirty |= changed || ctx.bindings.len() != bound,
            Err(e) => tracing::debug!("skipping {}: {e}", path.display()),
        }
        live.insert(key);
    }
    let gone: Vec<String> = cursors
        .files
        .keys()
        .filter(|k| !live.contains(*k))
        .cloned()
        .collect();
    for key in gone {
        if let Some(mut cursor) = cursors.files.remove(&key) {
            let ids: Vec<String> = cursor.pending.keys().cloned().collect();
            for id in ids {
                settle(&mut cursor, &id, filter, &mut out);
            }
        }
        dirty = true;
    }
    (out, dirty)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Returns false when the line must wait for a later tick.
fn handle_line(
    line: &[u8],
    cursor: &mut Cursor,
    ctx: &mut Ctx,
    out: &mut Vec<UsageRecord>,
) -> bool {
    if !contains(line, b"\"usage\"") {
        return true;
    }
    let Ok(parsed) = serde_json::from_slice::<Line>(line) else {
        return true;
    };
    if parsed.type_.as_deref() != Some("assistant") {
        return true;
    }
    let (Some(msg), Some(at)) = (parsed.message, parsed.timestamp) else {
        return true;
    };
    let (Some(id), Some(usage), Some(ts)) = (msg.id, msg.usage, parse_ms(&at)) else {
        return true;
    };
    let session = parsed.session_id.unwrap_or_default();
    let model = msg.model.unwrap_or_default();
    if [&id, &session, &model, &at]
        .iter()
        .any(|v| v.len() > MAX_ID)
        || cursor.recent.contains(&id)
    {
        return true;
    }
    if !cursor.pending.contains_key(&id) {
        let sid = Some(session.as_str()).filter(|s| !s.is_empty());
        match ctx
            .filter
            .routed
            .judge(ctx.bindings, sid, parsed.cwd.as_deref(), ts, ctx.now_ms)
        {
            Verdict::Defer => return false,
            Verdict::Skip => {
                remember(cursor, id);
                return true;
            }
            Verdict::Report => {}
        }
    }
    let entry = cursor.pending.entry(id.clone()).or_insert_with(|| Pending {
        cache_read: 0,
        cache_write: 0,
        first_at: at.clone(),
        first_ms: ts,
        input_tokens: 0,
        last_ms: ts,
        model,
        output_tokens: 0,
        session_ref: session,
    });
    let n = |v: Option<u64>| v.unwrap_or(0);
    entry.cache_read = entry.cache_read.max(n(usage.cache_read_input_tokens));
    entry.cache_write = entry.cache_write.max(n(usage.cache_creation_input_tokens));
    entry.input_tokens = entry.input_tokens.max(n(usage.input_tokens));
    entry.output_tokens = entry.output_tokens.max(n(usage.output_tokens));
    entry.last_ms = entry.last_ms.max(ts);
    if ts < entry.first_ms {
        entry.first_ms = ts;
        entry.first_at = at;
    }
    let filter = ctx.filter;
    if msg.stop_reason.is_some() {
        settle(cursor, &id, filter, out);
    } else if cursor.pending.len() > MAX_PENDING
        && let Some(oldest) = cursor
            .pending
            .iter()
            .min_by_key(|(_, p)| p.first_ms)
            .map(|(k, _)| k.clone())
    {
        settle(cursor, &oldest, filter, out);
    }
    true
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn head_hash(file: &mut File, len: u64) -> std::io::Result<String> {
    let mut buf = Vec::new();
    file.seek(SeekFrom::Start(0))?;
    file.by_ref().take(len).read_to_end(&mut buf)?;
    Ok(hex::encode(Sha256::digest(&buf)))
}

/// `2026-10-28T12:34:56.789Z` (or a `±HH:MM` offset) → unix ms.
fn parse_ms(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    let num = |from: usize, to: usize| -> Option<i64> {
        let part = s.get(from..to)?;
        part.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| part.parse().ok())?
    };
    let seps = [(4, b'-'), (7, b'-'), (13, b':'), (16, b':')];
    if seps.iter().any(|&(i, c)| b.get(i) != Some(&c))
        || !matches!(b.get(10), Some(b'T' | b't' | b' '))
    {
        return None;
    }
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let mut i = 19;
    let mut frac_ms = 0;
    if b.get(i) == Some(&b'.') {
        let start = i + 1;
        i = start;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return None;
        }
        let digits = &s[start..i.min(start + 3)];
        frac_ms = digits.parse::<i64>().ok()? * 10_i64.pow(3 - digits.len() as u32);
    }
    let offset_ms = match &s[i..] {
        "Z" | "z" => 0,
        tz if tz.len() == 6 && tz.as_bytes()[3] == b':' => {
            let sign = match tz.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            sign * (num(i + 1, i + 3)? * 60 + num(i + 4, i + 6)?) * 60_000
        }
        _ => return None,
    };
    let ms = (days_from_civil(y, mo, d) * 86_400 + h * 3_600 + mi * 60 + sec) * 1_000 + frac_ms
        - offset_ms;
    u64::try_from(ms).ok()
}

/// Read one `\n`-terminated line into `buf`, keeping at most `max` bytes.
/// Returns (bytes consumed, whether a newline ended it, whether it overflowed).
fn read_line_bounded(
    r: &mut impl BufRead,
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<(u64, bool, bool)> {
    let mut consumed = 0u64;
    let mut overlong = false;
    loop {
        let avail = r.fill_buf()?;
        if avail.is_empty() {
            return Ok((consumed, false, overlong));
        }
        let (chunk, done) = match avail.iter().position(|&c| c == b'\n') {
            Some(i) => (&avail[..=i], true),
            None => (avail, false),
        };
        let n = chunk.len();
        if buf.len() + n > max {
            overlong = true;
            buf.clear();
        } else if !overlong {
            buf.extend_from_slice(chunk);
        }
        r.consume(n);
        consumed += n as u64;
        if done {
            return Ok((consumed, true, overlong));
        }
    }
}

/// Read `path` from `cursor.offset`. Returns whether the cursor changed.
fn read_file(
    path: &Path,
    cursor: &mut Cursor,
    ctx: &mut Ctx,
    out: &mut Vec<UsageRecord>,
) -> std::io::Result<bool> {
    let meta = std::fs::metadata(path)?;
    let size = meta.len();
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    let before = (cursor.offset, cursor.pending.len());
    let mut at_end = cursor.offset == size;
    let unchanged = at_end && size == cursor.size && mtime_ms == cursor.mtime_ms;
    let mut replaced = false;
    if !unchanged {
        let mut file = File::open(path)?;
        replaced = size < cursor.offset
            || (cursor.head_len > 0
                && (size < cursor.head_len
                    || head_hash(&mut file, cursor.head_len)? != cursor.head));
        if replaced {
            cursor.offset = 0;
            cursor.head_len = 0;
        }
        if ctx
            .filter
            .windows
            .earliest()
            .is_some_and(|start| mtime_ms < start)
        {
            cursor.offset = size;
            at_end = true;
        } else {
            at_end = read_from(&mut file, cursor, size, ctx, out)?;
        }
        cursor.head_len = cursor.offset.min(HEAD_BYTES);
        cursor.head = head_hash(&mut file, cursor.head_len)?;
        cursor.mtime_ms = mtime_ms;
        cursor.size = size;
    }

    // A backlog cut off by the read budget or by a deferred line is not stale yet.
    if at_end {
        let stale: Vec<String> = cursor
            .pending
            .iter()
            .filter(|(_, p)| p.last_ms + SETTLE_MS <= ctx.now_ms)
            .map(|(k, _)| k.clone())
            .collect();
        for id in stale {
            settle(cursor, &id, ctx.filter, out);
        }
    }
    Ok(!unchanged || replaced || before != (cursor.offset, cursor.pending.len()))
}

/// Returns whether the read reached `size`.
fn read_from(
    file: &mut File,
    cursor: &mut Cursor,
    size: u64,
    ctx: &mut Ctx,
    out: &mut Vec<UsageRecord>,
) -> std::io::Result<bool> {
    file.seek(SeekFrom::Start(cursor.offset))?;
    let budget = size.saturating_sub(cursor.offset).min(MAX_READ_PER_TICK);
    let mut reader = BufReader::with_capacity(64 << 10, file.by_ref().take(budget));
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let (n, complete, overlong) = read_line_bounded(&mut reader, &mut buf, MAX_LINE)?;
        if n == 0 {
            return Ok(cursor.offset == size);
        }
        if !complete {
            // A partial last line waits for its newline, unless one line ate the whole budget.
            if n >= MAX_READ_PER_TICK {
                cursor.offset += n;
            }
            return Ok(false);
        }
        if !overlong && !handle_line(&buf, cursor, ctx, out) {
            return Ok(false);
        }
        cursor.offset += n;
    }
}

fn remembered_roots(cfg: &Config) -> Vec<PathBuf> {
    std::fs::read_to_string(roots_file(cfg))
        .unwrap_or_default()
        .lines()
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .collect()
}

fn remember(cursor: &mut Cursor, id: String) {
    cursor.recent.push_back(id);
    if cursor.recent.len() > RECENT_IDS {
        cursor.recent.pop_front();
    }
}

fn settle(cursor: &mut Cursor, id: &str, filter: &Filter, out: &mut Vec<UsageRecord>) {
    let Some(p) = cursor.pending.remove(id) else {
        return;
    };
    remember(cursor, id.to_string());
    let empty = p.input_tokens + p.output_tokens + p.cache_read + p.cache_write == 0;
    if p.model == SYNTHETIC_MODEL || empty || !filter.windows.contains(p.first_ms) {
        return;
    }
    out.push(UsageRecord {
        cache_read: p.cache_read,
        cache_write: p.cache_write,
        input_tokens: p.input_tokens,
        message_id: id.to_string(),
        model: p.model,
        occurred_at: p.first_at,
        output_tokens: p.output_tokens,
        session_ref: p.session_ref,
    });
}

fn roots_file(cfg: &Config) -> PathBuf {
    cfg.data_dir().join("claude-roots")
}

fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() && depth > 0 {
            walk(&path, depth - 1, out);
        } else if kind.is_file() && path.extension().is_some_and(|e| e == "jsonl") {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use serde_json::json;

    use super::*;
    use crate::daemon::routed::Span;

    const T0: u64 = 1_793_188_800_000; // 2026-10-28T12:00:00Z
    const SECRET: &str = "TOP-SECRET-PROMPT-TEXT";

    struct Fx {
        _dir: tempfile::TempDir,
        root: PathBuf,
    }

    impl Fx {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join(".claude").join("projects");
            std::fs::create_dir_all(&root).unwrap();
            Self { _dir: dir, root }
        }

        fn append(&self, rel: &str, body: &str) -> PathBuf {
            let path = self.root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(body.as_bytes()).unwrap();
            path
        }

        fn scan(&self, cursors: &mut Cursors, windows: &Windows, now: u64) -> Vec<UsageRecord> {
            let routed = Routed::default();
            let f = Filter {
                routed: &routed,
                windows,
            };
            scan(std::slice::from_ref(&self.root), cursors, &f, now).0
        }
    }

    fn assistant(id: &str, at_ms: u64, out: u64, stop: Option<&str>) -> String {
        let line = json!({
            "type": "assistant",
            "sessionId": "sess-1",
            "timestamp": iso(at_ms),
            "uuid": "u",
            "message": {
                "id": id,
                "model": "claude-test-1",
                "role": "assistant",
                "content": [{"type": "text", "text": SECRET}],
                "stop_reason": stop,
                "usage": {
                    "input_tokens": 3,
                    "output_tokens": out,
                    "cache_read_input_tokens": 100,
                    "cache_creation_input_tokens": 20,
                    "service_tier": "standard"
                }
            }
        });
        format!("{line}\n")
    }

    #[test]
    fn messages_from_a_dense_routed_span_are_not_reported() {
        let fx = Fx::new();
        let mut c = Cursors::default();
        let w = open_from(T0);
        let span = |id: &str, sid: Option<&str>| Span {
            cwd: "/w/routed".into(),
            end_ms: Some(T0 + 100),
            id: id.into(),
            pid: 1,
            seen_ms: T0 + 100,
            session_id: sid.map(str::to_string),
            start_ms: T0 + 10,
        };
        let routed = Routed {
            loaded_ms: T0 + 1_000,
            spans: vec![span("pinned", Some("s-pin")), span("resumed", None)],
        };
        let line = |id: &str, at: u64, sid: &str, cwd: &str| {
            let mut v: serde_json::Value =
                serde_json::from_str(&assistant(id, at, 5, Some("end_turn"))).unwrap();
            v["sessionId"] = json!(sid);
            v["cwd"] = json!(cwd);
            format!("{v}\n")
        };
        fx.append(
            "-w-routed/s.jsonl",
            &[
                line("pin_cd", T0 + 20, "s-pin", "/elsewhere"),
                line("pin_before", T0 + 5, "s-pin", "/w/routed"),
                line("pin_after", T0 + 100, "s-pin", "/w/routed"),
                line("res_first", T0 + 30, "s-res", "/w/routed"),
                line("res_cd", T0 + 40, "s-res", "/w/routed/sub"),
                line("plain_sub", T0 + 50, "s-plain", "/w/routed/sub"),
                line("plain_other", T0 + 50, "s-other", "/w/other"),
                line("later", T0 + 1_500, "s-plain", "/w/other"),
            ]
            .concat(),
        );
        let f = Filter {
            routed: &routed,
            windows: &w,
        };
        let (got, _) = scan(std::slice::from_ref(&fx.root), &mut c, &f, T0 + 2_000);
        let mut got = ids(&got);
        got.sort_unstable();
        assert_eq!(got, ["pin_after", "pin_before", "plain_other", "plain_sub"]);
        assert_eq!(c.bindings.len(), 1);

        let routed = Routed {
            loaded_ms: T0 + 1_000_000,
            ..routed
        };
        let f = Filter {
            routed: &routed,
            windows: &w,
        };
        let (got, _) = scan(std::slice::from_ref(&fx.root), &mut c, &f, T0 + 1_000_000);
        assert_eq!(ids(&got), ["later"]);
    }

    #[test]
    fn null_usage_fields_count_as_zero_and_huge_ids_are_dropped() {
        let fx = Fx::new();
        let mut c = Cursors::default();
        let w = open_from(T0);
        let mut v: serde_json::Value =
            serde_json::from_str(&assistant("nulls", T0 + 1, 7, Some("end_turn"))).unwrap();
        v["message"]["usage"]["cache_read_input_tokens"] = json!(null);
        let huge = assistant(&"x".repeat(MAX_ID + 1), T0 + 2, 7, Some("end_turn"));
        fx.append("p/s.jsonl", &format!("{v}\n{huge}"));
        let got = fx.scan(&mut c, &w, T0 + 10);
        assert_eq!(ids(&got), ["nulls"]);
        assert_eq!((got[0].cache_read, got[0].output_tokens), (0, 7));
    }

    fn ids(records: &[UsageRecord]) -> Vec<&str> {
        records.iter().map(|r| r.message_id.as_str()).collect()
    }

    fn iso(ms: u64) -> String {
        let secs = ms / 1_000;
        let (days, rem) = (secs / 86_400, secs % 86_400);
        // Only used for dates on 2026-10-28 .. 2026-10-29 in these fixtures.
        let day = 28 + (days - T0 / 1_000 / 86_400);
        format!(
            "2026-10-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
            rem / 3_600,
            rem % 3_600 / 60,
            rem % 60,
            ms % 1_000
        )
    }

    fn open_from(start: u64) -> Windows {
        let mut w = Windows::default();
        w.note(Mode::Transparent, start);
        w
    }

    fn user_line() -> String {
        format!(
            "{}\n",
            json!({"type": "user", "timestamp": iso(T0), "message": {"role": "user", "content": SECRET}})
        )
    }

    #[test]
    fn parses_rfc3339() {
        assert_eq!(parse_ms("2026-10-28T12:00:00.000Z"), Some(T0));
        assert_eq!(parse_ms("2026-10-28T12:00:00Z"), Some(T0));
        assert_eq!(parse_ms("2026-10-28T14:00:00.5+02:00"), Some(T0 + 500));
        assert_eq!(parse_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_ms("2026-10-28 12:00:00Z"), Some(T0));
        for bad in [
            "",
            "2026-10-28",
            "2026-13-01T00:00:00Z",
            "2026-10-28T12:00:00",
            "x026-10-28T12:00:00Z",
        ] {
            assert_eq!(parse_ms(bad), None, "{bad}");
        }
        assert_eq!(iso(T0), "2026-10-28T12:00:00.000Z");
    }

    #[test]
    fn streamed_duplicates_report_once_with_final_counts() {
        let fx = Fx::new();
        let mut c = Cursors::default();
        let w = open_from(T0);
        fx.append(
            "-p/s.jsonl",
            &[
                assistant("msg_a", T0 + 1_000, 7, None),
                user_line(),
                assistant("msg_a", T0 + 2_000, 42, Some("tool_use")),
                assistant("msg_a", T0 + 2_500, 42, Some("tool_use")),
            ]
            .concat(),
        );
        let got = fx.scan(&mut c, &w, T0 + 3_000);
        assert_eq!(got.len(), 1);
        let r = &got[0];
        assert_eq!(
            (r.input_tokens, r.output_tokens, r.cache_read, r.cache_write),
            (3, 42, 100, 20)
        );
        assert_eq!(
            (r.model.as_str(), r.session_ref.as_str()),
            ("claude-test-1", "sess-1")
        );
        assert_eq!(r.occurred_at, iso(T0 + 1_000));
        // A later block line for the same message (next tick) is not re-reported.
        fx.append(
            "-p/s.jsonl",
            &assistant("msg_a", T0 + 2_600, 42, Some("tool_use")),
        );
        assert!(fx.scan(&mut c, &w, T0 + 4_000).is_empty());
    }

    #[test]
    fn unfinished_message_waits_then_settles_with_max_counts() {
        let fx = Fx::new();
        let mut c = Cursors::default();
        let w = open_from(T0);
        fx.append("-p/s.jsonl", &assistant("msg_a", T0 + 1_000, 9, None));
        assert!(fx.scan(&mut c, &w, T0 + 2_000).is_empty());
        fx.append("-p/s.jsonl", &assistant("msg_a", T0 + 5_000, 11, None));
        assert!(fx.scan(&mut c, &w, T0 + 6_000).is_empty());
        let got = fx.scan(&mut c, &w, T0 + 5_000 + SETTLE_MS);
        assert_eq!(ids(&got), ["msg_a"]);
        assert_eq!(got[0].output_tokens, 11);
    }

    #[test]
    fn incremental_reads_only_new_lines() {
        let fx = Fx::new();
        let mut c = Cursors::default();
        let w = open_from(T0);
        fx.append(
            "-p/s.jsonl",
            &assistant("msg_1", T0 + 1, 1, Some("end_turn")),
        );
        assert_eq!(ids(&fx.scan(&mut c, &w, T0 + 10)), ["msg_1"]);
        let f = Filter {
            routed: &Routed::default(),
            windows: &w,
        };
        let (none, dirty) = scan(std::slice::from_ref(&fx.root), &mut c, &f, T0 + 20);
        assert!(none.is_empty() && !dirty);
        fx.append(
            "-p/s.jsonl",
            &assistant("msg_2", T0 + 30, 1, Some("end_turn")),
        );
        assert_eq!(ids(&fx.scan(&mut c, &w, T0 + 40)), ["msg_2"]);
    }

    #[test]
    fn cursors_survive_a_restart() {
        let fx = Fx::new();
        let state = fx.root.parent().unwrap().join("sessions.json");
        let w = open_from(T0);
        fx.append("-p/s.jsonl", &assistant("msg_1", T0 + 1, 1, None));
        fx.append(
            "-p/s.jsonl",
            &assistant("msg_2", T0 + 2, 1, Some("end_turn")),
        );
        let mut c = Cursors::default();
        assert_eq!(ids(&fx.scan(&mut c, &w, T0 + 10)), ["msg_2"]);
        c.save(&state).unwrap();
        let mut c = Cursors::load(&state);
        fx.append(
            "-p/s.jsonl",
            &assistant("msg_1", T0 + 3, 5, Some("end_turn")),
        );
        let got = fx.scan(&mut c, &w, T0 + 20);
        assert_eq!(ids(&got), ["msg_1"]);
        assert_eq!(got[0].output_tokens, 5);
    }

    #[test]
    fn partial_last_line_waits_for_its_newline() {
        let fx = Fx::new();
        let mut c = Cursors::default();
        let w = open_from(T0);
        let line = assistant("msg_1", T0 + 1, 1, Some("end_turn"));
        let (head, tail) = line.split_at(line.len() / 2);
        fx.append("-p/s.jsonl", head);
        assert!(fx.scan(&mut c, &w, T0 + 10).is_empty());
        assert_eq!(c.files.values().next().unwrap().offset, 0);
        fx.append("-p/s.jsonl", tail);
        assert_eq!(ids(&fx.scan(&mut c, &w, T0 + 20)), ["msg_1"]);
    }

    #[test]
    fn truncated_or_replaced_file_is_reread_from_the_start() {
        let fx = Fx::new();
        let mut c = Cursors::default();
        let w = open_from(T0);
        let path = fx.append(
            "-p/s.jsonl",
            &[
                assistant("msg_1", T0 + 1, 1, Some("end_turn")),
                assistant("msg_2", T0 + 2, 1, Some("end_turn")),
            ]
            .concat(),
        );
        assert_eq!(fx.scan(&mut c, &w, T0 + 10).len(), 2);
        std::fs::write(&path, assistant("msg_3", T0 + 3, 1, Some("end_turn"))).unwrap();
        assert_eq!(ids(&fx.scan(&mut c, &w, T0 + 20)), ["msg_3"]);
        // Same-or-larger size but a different head: rotated, re-read; known ids stay deduped.
        let rotated = [
            assistant("msg_4", T0 + 4, 1, Some("end_turn")),
            assistant("msg_3", T0 + 3, 1, Some("end_turn")),
        ]
        .concat();
        std::fs::write(&path, rotated).unwrap();
        assert_eq!(ids(&fx.scan(&mut c, &w, T0 + 30)), ["msg_4"]);
    }

    #[test]
    fn only_messages_inside_a_transparent_window_are_reported() {
        let fx = Fx::new();
        let mut c = Cursors::default();
        let mut w = Windows::default();
        w.note(Mode::Transparent, T0 + 100);
        w.note(Mode::Condensing, T0 + 200);
        w.note(Mode::Transparent, T0 + 300);
        fx.append(
            "-p/s.jsonl",
            &[
                assistant("before", T0 + 50, 1, Some("end_turn")),
                assistant("in_first", T0 + 150, 1, Some("end_turn")),
                assistant("condensing", T0 + 250, 1, Some("end_turn")),
                assistant("at_close", T0 + 200, 1, Some("end_turn")),
                assistant("in_open", T0 + 400, 1, Some("end_turn")),
            ]
            .concat(),
        );
        assert_eq!(ids(&fx.scan(&mut c, &w, T0 + 500)), ["in_first", "in_open"]);
    }

    #[test]
    fn files_older_than_every_window_are_skipped_unread() {
        let fx = Fx::new();
        let mut c = Cursors::default();
        let path = fx.append("-p/old.jsonl", &assistant("old", T0, 1, Some("end_turn")));
        let size = std::fs::metadata(&path).unwrap().len();
        let w = open_from(u64::MAX / 2);
        assert!(fx.scan(&mut c, &w, T0).is_empty());
        assert_eq!(c.files.values().next().unwrap().offset, size);
    }

    #[test]
    fn walks_every_project_and_subagent_file() {
        let fx = Fx::new();
        let mut c = Cursors::default();
        let w = open_from(T0);
        fx.append(
            "-home-a/s1.jsonl",
            &assistant("a", T0 + 1, 1, Some("end_turn")),
        );
        fx.append(
            "-home-b/s2.jsonl",
            &assistant("b", T0 + 2, 1, Some("end_turn")),
        );
        fx.append(
            "-home-b/s2/subagents/agent-x.jsonl",
            &assistant("c", T0 + 3, 1, Some("end_turn")),
        );
        fx.append(
            "-home-b/notes.txt",
            &assistant("txt", T0 + 4, 1, Some("end_turn")),
        );
        let mut got: Vec<String> = fx
            .scan(&mut c, &w, T0 + 10)
            .into_iter()
            .map(|r| r.message_id)
            .collect();
        got.sort();
        assert_eq!(got, ["a", "b", "c"]);
    }

    #[test]
    fn synthetic_garbage_and_zero_usage_lines_are_ignored() {
        let fx = Fx::new();
        let mut c = Cursors::default();
        let w = open_from(T0);
        let synthetic =
            assistant("syn", T0 + 1, 1, Some("end_turn")).replace("claude-test-1", SYNTHETIC_MODEL);
        let zero = format!(
            "{}\n",
            json!({"type": "assistant", "timestamp": iso(T0 + 2), "message": {"id": "zero", "model": "m",
                "stop_reason": "end_turn", "usage": {"input_tokens": 0, "output_tokens": 0}}})
        );
        fx.append(
            "-p/s.jsonl",
            &[
                synthetic,
                zero,
                "{\"usage\": not json\n".into(),
                user_line(),
                assistant("ok", T0 + 3, 1, Some("end_turn")),
            ]
            .concat(),
        );
        assert_eq!(ids(&fx.scan(&mut c, &w, T0 + 10)), ["ok"]);
    }

    #[test]
    fn overlong_lines_are_skipped_with_bounded_memory() {
        let mut big = vec![b'x'; 300];
        big.push(b'\n');
        big.extend_from_slice(b"ok\n");
        let mut r = BufReader::with_capacity(16, &big[..]);
        let mut buf = Vec::new();
        assert_eq!(
            read_line_bounded(&mut r, &mut buf, 64).unwrap(),
            (301, true, true)
        );
        assert!(buf.capacity() <= 128);
        buf.clear();
        assert_eq!(
            read_line_bounded(&mut r, &mut buf, 64).unwrap(),
            (3, true, false)
        );
        assert_eq!(buf, b"ok\n");
    }

    #[test]
    fn records_never_carry_content() {
        let fx = Fx::new();
        let mut c = Cursors::default();
        let w = open_from(T0);
        fx.append(
            "-p/s.jsonl",
            &[user_line(), assistant("msg_1", T0 + 1, 1, None)].concat(),
        );
        let got = fx.scan(&mut c, &w, T0 + 10);
        assert!(got.is_empty());
        let queued = serde_json::to_string(&fx.scan(&mut c, &w, T0 + 1 + SETTLE_MS)).unwrap();
        let cursors = serde_json::to_string(&c).unwrap();
        assert!(queued.contains("msg_1"));
        assert!(!queued.contains(SECRET) && !cursors.contains(SECRET));
    }

    #[test]
    fn roots_follow_claude_config_dir_and_dedupe() {
        let home = Path::new("/h");
        let none = roots(home, |_| None);
        assert_eq!(
            none,
            [
                PathBuf::from("/h/.claude/projects"),
                PathBuf::from("/h/.config/claude/projects")
            ]
        );
        let custom = roots(home, |k| (k == "CLAUDE_CONFIG_DIR").then(|| "/c".into()));
        assert_eq!(custom[0], PathBuf::from("/c/projects"));
        assert_eq!(custom.len(), 3);
    }

    #[test]
    fn windows_open_once_close_and_prune() {
        let mut w = Windows::default();
        w.note(Mode::Condensing, 5);
        assert!(w.is_empty());
        w.note(Mode::Transparent, 10);
        w.note(Mode::Transparent, 20);
        assert_eq!(w.0.len(), 1);
        assert!(w.contains(10) && w.contains(1_000_000) && !w.contains(9));
        w.close(30);
        assert!(w.contains(29) && !w.contains(30));
        w.close(40);
        assert_eq!(w.0[0].end_ms, Some(30));
        w.note(Mode::Transparent, 50);
        w.prune(30 + WINDOW_RETENTION_MS + 1);
        assert_eq!(
            w.0,
            [Window {
                end_ms: None,
                start_ms: 50
            }]
        );
    }
}
