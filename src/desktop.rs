//! `dense claude-desktop` — route Claude Desktop's local engine through condense.
//! Desktop spawns its engine by absolute path from a user-writable folder, so
//! a copy of dense takes the engine's filename and the real binary moves to a
//! sibling. Wrapping is a rename, so unwrapping is a rename back.

mod bundle;
pub mod engine;

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::api::auth;
use crate::config::Config;
use crate::error::Context;
use crate::{Result, daemon, ui};

use bundle::Bundles;

pub const COMMAND: &str = "claude-desktop";

const BIN: &str = if cfg!(windows) {
    "claude.exe"
} else {
    "claude"
};
const BUNDLE: &str = "claude.app";
const DISPLACED_PREFIX: &str = "claude.dense-old-";
const FORCED_FILE: &str = "desktop-forced";
/// How long a folder is left alone once its wrapper keeps disappearing.
const HOLD_SECS: u64 = 3_600;
const LOCK_FILE: &str = "desktop.lock";
const POLL: Duration = Duration::from_secs(15);
const REAL: &str = if cfg!(windows) {
    "claude.real.exe"
} else {
    "claude.real"
};
const REAL_BUNDLE: &str = "claude.real.app";
/// Wraps in a row, with no pass finding the last one still there, before a hold.
const REWRAP_LIMIT: u32 = 3;
/// How long an engine left unrouted waits, unchanged, before another full check.
const RETRY: Duration = Duration::from_secs(300);
const ROUTING_FILE: &str = "desktop.toml";
const TMP: &str = "claude.dense-tmp";

pub type Rewraps = BTreeMap<String, Rewrap>;
/// Size and mtime of an engine's files: a stat, where a full check costs a hash
/// and a signature.
type Seen = Vec<Option<(u64, SystemTime)>>;

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Current,
    Skipped(String),
    Wrapped,
}

/// A folder dense wrapped again before any pass found its last wrapper in place,
/// as when security software removes it or Desktop downloads the engine again.
#[derive(Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct Rewrap {
    count: u32,
    held_until: u64,
}

/// One published engine: `<root>/<version>/[<build>/]` holding `.verified`.
#[derive(Debug, PartialEq, Eq)]
struct Install {
    bin: PathBuf,
    dir: PathBuf,
    real: PathBuf,
    version: String,
}

#[derive(Deserialize)]
struct RemoteSpaces {
    entries: Vec<serde::de::IgnoredAny>,
}

#[derive(Deserialize)]
struct Payload {
    sha256: String,
}

/// What [`route`] did, for the caller to show.
#[derive(Debug, Default)]
pub struct Report {
    pub fields: Vec<(&'static str, String)>,
    pub ready: bool,
    pub summary: String,
    pub warnings: Vec<String>,
}

pub struct Removal {
    pub changed: bool,
    pub restored: Vec<String>,
}

/// What the daemon's Desktop job carries from one pass to the next.
#[derive(Debug, Default)]
pub struct Watcher {
    /// Whether a pass already replaced wrappers from an older dense.
    refreshed: bool,
    /// Engines a pass left unrouted, as their files were then.
    unrouted: BTreeMap<PathBuf, (Seen, Instant)>,
}

/// Turns the daemon's Desktop job on. Wrapped engines can't stand in for this:
/// Desktop removes them all when it replaces a version.
#[derive(Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
struct Routing {
    /// `install` added the daemon for this, so `uninstall` removes it again.
    added_daemon: bool,
    data_dir: Option<PathBuf>,
}

impl Install {
    fn bundled(&self) -> bool {
        bundle::present(&self.dir)
    }

    // ponytail: size alone, as the watcher asks every poll. A genuine engine of
    // another size beside a stale `real` still reads as wrapped; a folder holds
    // one build, so a re-download comes back the same size as `real`.
    fn is_wrapped(&self) -> bool {
        if self.bundled() {
            return bundle::wrapped(&self.dir);
        }
        let len = |path: &Path| std::fs::metadata(path).map(|m| m.len());
        matches!((len(&self.bin), len(&self.real)), (Ok(bin), Ok(real)) if bin != real)
    }

    fn label(&self) -> String {
        let build = self
            .dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| *n != self.version);
        match build {
            Some(build) => format!("{}/{build}", self.version),
            None => self.version.clone(),
        }
    }

    /// The bytes about to move must be the ones Desktop verified at download,
    /// so an unknown file never ends up behind the shim.
    fn matches_payload(&self) -> Result<bool> {
        let Ok(raw) = std::fs::read(self.dir.join(".payload")) else {
            return Ok(false);
        };
        let Ok(payload) = serde_json::from_slice::<Payload>(&raw) else {
            return Ok(false);
        };
        Ok(file_sha256(&self.bin)? == payload.sha256)
    }

    fn seen(&self) -> Seen {
        let markers = [self.dir.join(".payload"), self.dir.join(".verified")];
        [&self.bin, &self.real]
            .into_iter()
            .chain(&markers)
            .map(|path| {
                let meta = std::fs::metadata(path).ok()?;
                Some((meta.len(), meta.modified().ok()?))
            })
            .collect()
    }

    fn unwrap_engine(&self) -> Result<bool> {
        if self.bundled() {
            return bundle::unwrap(&self.dir);
        }
        sweep_displaced(&self.bin);
        if !self.real.exists() {
            return Ok(false);
        }
        replace(&self.real, &self.bin).map(|()| true)
    }

    /// `refresh` also replaces an older dense copy, which costs a hash per
    /// install, so the watcher leaves it off.
    fn wrap(&self, exe: &Path, refresh: bool, bundles: Bundles) -> Result<Outcome> {
        if self.bundled() {
            return bundle::wrap(&self.dir, exe, refresh, bundles);
        }
        sweep_displaced(&self.bin);
        if self.is_wrapped() {
            if !refresh || file_sha256(&self.bin)? == file_sha256(exe)? {
                return Ok(Outcome::Current);
            }
            return place(exe, &self.bin).map(|()| Outcome::Wrapped);
        }
        match (self.bin.exists(), self.real.exists()) {
            (false, false) => Ok(Outcome::Skipped("engine binary is missing".to_string())),
            (false, true) => place(exe, &self.bin).map(|()| Outcome::Wrapped),
            (true, _) if !self.matches_payload()? => Ok(Outcome::Skipped(
                "engine doesn't match Desktop's download record".to_string(),
            )),
            (true, _) => {
                let tmp = stage(exe, &self.bin)?;
                swap(&self.bin, &tmp, &self.real)
                    .inspect_err(|_| {
                        let _ = std::fs::remove_file(&tmp);
                    })
                    .map(|()| Outcome::Wrapped)
            }
        }
    }
}

impl Routing {
    fn load(cfg: &Config) -> Option<Self> {
        let raw = std::fs::read_to_string(routing_file(cfg)).ok()?;
        toml::from_str(&raw).ok()
    }

    fn save(&self, cfg: &Config) -> Result<()> {
        let path = routing_file(cfg);
        std::fs::create_dir_all(cfg.data_dir()).ctx("creating dense data dir")?;
        let body = toml::to_string_pretty(self).ctx("serializing Desktop routing")?;
        std::fs::write(&path, body).ctx(format!("writing {}", path.display()))
    }

    fn take(cfg: &Config) -> Option<Self> {
        let routing = Self::load(cfg);
        let _ = std::fs::remove_file(routing_file(cfg));
        routing
    }
}

impl Watcher {
    /// An engine left unrouted is checked again once Desktop changes its files,
    /// or after [`RETRY`]. Until then a pass only stats it.
    fn due(&self, dir: &Path, seen: &Seen, now: Instant) -> bool {
        self.unrouted
            .get(dir)
            .is_none_or(|(was, at)| was != seen || now.duration_since(*at) >= RETRY)
    }
}

/// Published engines still running outside condense, each with its fix.
pub fn unwrapped_versions(cfg: &Config) -> Vec<String> {
    installs(&roots_or_default(recorded_or(cfg, None)))
        .iter()
        .filter(|i| !i.is_wrapped())
        .map(|i| format!("{} — run `dense claude-desktop install`", i.label()))
        .collect()
}

pub fn has_wrapped_engines(cfg: &Config) -> bool {
    installs(&roots_or_default(recorded_or(cfg, None)))
        .iter()
        .any(Install::is_wrapped)
}

/// Whether Desktop has run on this computer, even with no engine downloaded yet.
pub fn detected() -> bool {
    candidate_roots()
        .iter()
        .any(|root| root.parent().is_some_and(Path::is_dir))
}

/// Routing outlives the wrapped engines, which Desktop can replace all at once.
pub fn in_use(cfg: &Config) -> bool {
    Routing::load(cfg).is_some()
        || has_wrapped_engines(cfg)
        || (daemon::managed::forced_on(cfg) && detected())
}

/// False with no engines: nothing is routed yet, whatever happens later.
pub fn routes_every_engine(cfg: &Config) -> bool {
    let all = installs(&roots_or_default(recorded_or(cfg, None)));
    !all.is_empty() && all.iter().all(Install::is_wrapped)
}

pub fn summary(cfg: &Config, rewraps: &Rewraps, now: u64) -> String {
    let all = installs(&roots_or_default(recorded_or(cfg, None)));
    let routed = all.iter().filter(|i| i.is_wrapped()).count();
    let held = rewraps.values().filter(|r| r.held_until > now).count();
    match all.len() {
        _ if held > 0 => format!(
            "{routed} of {} engines routed; {held} left alone for now: its wrapper keeps \
             disappearing, possibly removed by security software",
            all.len()
        ),
        0 => "no engine yet; the next one Desktop downloads is routed".to_string(),
        n if n == routed => format!("{n} of {n} engines routed"),
        n => format!("{routed} of {n} engines routed; the daemon keeps trying"),
    }
}

pub fn install(cfg: &Config, data_dir: Option<PathBuf>, watch: bool) -> Result<()> {
    let report = route(cfg, data_dir, watch)?;
    ui::title("Claude Desktop");
    if report.ready {
        ui::success(&report.summary);
    } else {
        ui::note(&report.summary);
    }
    ui::section("Setup details");
    for (label, value) in &report.fields {
        ui::field(label, value);
    }
    for warning in &report.warnings {
        ui::warning(warning);
    }
    coverage(0);
    ui::hint("Check status", "dense claude-desktop status");
    ui::hint("Undo", "dense claude-desktop uninstall");
    Ok(())
}

/// `dense daemon install`: the daemon is the user's own now, so a later
/// `uninstall` leaves it running.
pub fn keep_daemon(cfg: &Config) -> Result<()> {
    let _lock = lock(cfg)?;
    match Routing::load(cfg) {
        Some(routing) if routing.added_daemon => Routing {
            added_daemon: false,
            ..routing
        }
        .save(cfg),
        _ => Ok(()),
    }
}

pub fn status(cfg: &Config, data_dir: Option<PathBuf>) -> Result<()> {
    let roots = roots_or_default(recorded_or(cfg, data_dir));
    let engine = Config::resolve(None, None)?;
    let logged_in = auth::load_creds(&engine).is_authenticated();
    let found = installs(&roots);
    let routed = found.iter().filter(|i| i.is_wrapped()).count();
    let off = cfg!(feature = "desktop-switch") && daemon::managed::switched_off(cfg);
    ui::title("Claude Desktop");
    if off {
        ui::note("Routing is paused by your organization");
    } else if routed > 0 && routed < found.len() && logged_in {
        ui::note(format!(
            "{routed} of {} local engines connected; some need attention",
            found.len()
        ));
        ui::hint("Connect remaining engines", "dense claude-desktop install");
    } else if routed > 0 && logged_in {
        ui::success(format!(
            "{routed} of {} local engines connected to Condense",
            found.len()
        ));
    } else if routed > 0 {
        ui::note("Setup is saved. Log in to start routing sessions");
        ui::hint("Log in", "dense login");
    } else {
        ui::note("Local Code sessions are not connected to Condense");
        ui::hint("Connect", "dense claude-desktop install");
    }
    ui::section("Local engines");
    if found.is_empty() {
        ui::field(
            "Engine",
            "No engine yet. Start a Code session in Claude Desktop to download one.",
        );
    }
    for found in found {
        let state = match (found.is_wrapped(), logged_in, off) {
            (false, _, _) => "Not connected",
            (true, _, true) => "Ready; routing paused by your organization",
            (true, false, _) => "Ready; login required (sessions run directly)",
            (true, true, false) => "Connected",
        };
        ui::field("Version", format!("{}: {state}", found.label()));
    }
    ui::field("Server", &engine.api_base_url);
    for root in &roots {
        ui::field("Folder", root.display());
    }
    ui::section("Automatic updates");
    let wanted = Routing::load(cfg).is_some() || daemon::managed::forced_on(cfg);
    let running = daemon::running(cfg);
    ui::field(
        "Watcher",
        match (wanted, running) {
            (false, _) => "Disabled; run `dense claude-desktop install` after Desktop updates",
            (true, false) => "Not running; new engines won't connect automatically",
            (true, true) => "Running; new engines connect automatically",
        },
    );
    if wanted && !running {
        ui::hint("Restart watcher", "dense claude-desktop install");
    }
    coverage(roots.iter().map(|r| cloud_sessions(r)).sum());
    Ok(())
}

pub fn restore(cfg: &Config, data_dir: Option<PathBuf>) -> Result<Removal> {
    let progress = ui::progress("Restoring Claude Desktop...");
    let _lock = lock(cfg)?;
    // Inside the lock, so a pass waiting on it finds routing off and wraps nothing.
    let routing = Routing::take(cfg);
    let configured = routing.is_some() || forced_file(cfg).exists();
    let routing = routing.unwrap_or_default();
    let _ = std::fs::remove_file(forced_file(cfg));
    let mut restored = Vec::new();
    let mut result = Ok(());
    for found in installs(&roots_or_default(data_dir.or(routing.data_dir))) {
        match found.unwrap_engine() {
            Ok(true) => restored.push(found.label()),
            Ok(false) => {}
            Err(e) => result = result.and(Err(e)),
        }
    }
    drop(progress);
    if routing.added_daemon {
        result = result.and(daemon::mark_added(cfg, true));
    }
    result.and(daemon::release(cfg))?;
    Ok(Removal {
        changed: configured || !restored.is_empty(),
        restored,
    })
}

pub fn uninstall(cfg: &Config, data_dir: Option<PathBuf>) -> Result<()> {
    let report = restore(cfg, data_dir)?;
    ui::title("Claude Desktop");
    if report.changed {
        ui::success("Condense disconnected from Claude Desktop");
        for version in report.restored {
            ui::field("Restored", version);
        }
        ui::field("Updates", "Automatic routing disabled");
    } else {
        ui::note("Already disconnected. Nothing to remove");
    }
    ui::hint("Reconnect", "dense claude-desktop install");
    Ok(())
}

/// The daemon's Desktop job: follow Desktop through installs, engine downloads
/// and updates, and uninstalls, wrapping each engine it publishes. The first
/// pass after a start also replaces wrappers from an older dense, since the
/// daemon restarts whenever its binary changes.
pub async fn watch(cfg: &Config, watcher: &mut Watcher, rewraps: &mut Rewraps) -> Duration {
    let Ok(exe) = std::env::current_exe() else {
        return POLL;
    };
    let cfg = cfg.clone();
    let mut state = std::mem::take(watcher);
    let mut held = std::mem::take(rewraps);
    // Off the daemon's thread: the lock can wait on an `install`, and hashing
    // and `codesign` take seconds.
    let pass = tokio::task::spawn_blocking(move || {
        wrap_new(&cfg, &exe, &mut state, &mut held);
        (state, held)
    });
    match pass.await {
        Ok((state, held)) => {
            *watcher = state;
            *rewraps = held;
        }
        Err(e) => tracing::warn!("Desktop engine pass failed: {e}"),
    }
    POLL
}

/// Whether the daemon's Desktop job has work.
pub fn watching(cfg: &Config) -> bool {
    Routing::load(cfg).is_some()
}

/// Cloud sessions that were granted a local folder. They run on Anthropic's side,
/// so the shim never sees them.
// ponytail: cloud sessions with no folder leave no local record, so this is a
// floor; local Cowork's session schema wasn't observed, so only cloud is counted.
fn cloud_sessions(root: &Path) -> usize {
    let Some(store) = root.parent().map(|p| p.join("local-agent-mode-sessions")) else {
        return 0;
    };
    subdirs(&store)
        .iter()
        .flat_map(|account| subdirs(account))
        .filter_map(|org| std::fs::read(org.join("remote-session-spaces.json")).ok())
        .filter_map(|raw| serde_json::from_slice::<RemoteSpaces>(&raw).ok())
        .map(|spaces| spaces.entries.len())
        .sum()
}

pub fn coverage(cloud: usize) {
    ui::section("What Condense can route");
    ui::field("Code", "Sessions using this computer's local engine");
    ui::field("Cowork", cowork_status(cloud));
    ui::field("Chat", "Not routed");
}

fn cowork_status(cloud: usize) -> String {
    let rule = "Only sessions using this computer's local engine. Anthropic's cloud and Desktop's VM are not routed.";
    match cloud {
        0 => rule.to_string(),
        n => format!(
            "{rule}\n{n} {} with folder access ran in Anthropic's cloud.",
            if n == 1 { "session" } else { "sessions" }
        ),
    }
}

/// Where Desktop keeps engines, whether or not it has made the folder yet.
fn candidate_roots() -> Vec<PathBuf> {
    let Some(base) = directories::BaseDirs::new() else {
        return Vec::new();
    };
    let mut roots = vec![base.config_dir().join("Claude").join("claude-code")];
    if cfg!(windows) {
        roots.extend(packaged_roots(base.data_local_dir()));
    }
    roots
}

fn default_roots() -> Vec<PathBuf> {
    let mut roots = candidate_roots();
    roots.retain(|r| r.is_dir());
    roots
}

// Packaged Windows builds keep data in `Claude-Data\<family>`, or in the package's
// redirected roaming folder on older installs. `Claude-3p` is left out: gateway-mode
// users picked their own endpoint.
fn packaged_roots(local: &Path) -> Vec<PathBuf> {
    let children = |dir: PathBuf| -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .map(|d| d.flatten().map(|e| e.path()).collect())
            .unwrap_or_default()
    };
    let is_claude_family = |p: &PathBuf| {
        p.file_name()
            .is_some_and(|n| n.to_string_lossy().to_lowercase().contains("claude_"))
    };
    let current = children(local.join("Claude-Data"))
        .into_iter()
        .filter(|p| is_claude_family(p))
        .map(|p| p.join("claude-code"));
    let legacy = children(local.join("Packages"))
        .into_iter()
        .filter(|p| is_claude_family(p))
        .map(|p| {
            p.join("LocalCache")
                .join("Roaming")
                .join("Claude")
                .join("claude-code")
        });
    current.chain(legacy).collect()
}

fn forced_file(cfg: &Config) -> PathBuf {
    cfg.data_dir().join(FORCED_FILE)
}

fn file_sha256(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path).ctx(format!("reading {}", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).ctx(format!("hashing {}", path.display()))?;
    Ok(hex::encode(hasher.finalize()))
}

fn install_at(dir: &Path, version: &str) -> Install {
    let (bin, real) = if bundle::present(dir) {
        (
            bundle::executable(&dir.join(BUNDLE)),
            bundle::executable(&dir.join(REAL_BUNDLE)),
        )
    } else {
        (dir.join(BIN), dir.join(REAL))
    };
    Install {
        bin,
        dir: dir.to_path_buf(),
        real,
        version: version.to_string(),
    }
}

fn installs(roots: &[PathBuf]) -> Vec<Install> {
    let mut out = Vec::new();
    for root in roots {
        for version_dir in subdirs(root) {
            let version = version_dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let mut dirs = subdirs(&version_dir);
            dirs.push(version_dir);
            out.extend(
                dirs.iter()
                    .filter(|d| d.join(".verified").exists())
                    .map(|d| install_at(d, &version)),
            );
        }
    }
    out
}

/// One wrap or restore at a time, across processes and a whole pass, so a hash
/// and the renames it justifies can't interleave with another dense's. The OS
/// drops it if this process dies.
fn lock(cfg: &Config) -> Result<File> {
    std::fs::create_dir_all(cfg.data_dir()).ctx("creating dense data dir")?;
    let path = cfg.data_dir().join(LOCK_FILE);
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .ctx(format!("opening {}", path.display()))?;
    file.lock().ctx(format!("locking {}", path.display()))?;
    Ok(file)
}

fn nothing_routed(found: &[Install], watch: bool, data_dir: Option<&Path>) -> &'static str {
    let present = data_dir.map_or_else(detected, Path::is_dir);
    match (found.is_empty(), watch, present) {
        (false, _, _) => "No local engines are ready. Review the warnings below.",
        (true, true, true) => "No engine yet. Dense will prepare the next one Desktop downloads.",
        (true, true, false) => {
            "Claude Desktop hasn't run on this computer yet. Dense will prepare its local engines when it does."
        }
        (true, false, _) => {
            "No engine yet. Start a Code session in Claude Desktop, then run this again."
        }
    }
}

fn place(exe: &Path, bin: &Path) -> Result<()> {
    let tmp = stage(exe, bin)?;
    replace(&tmp, bin).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn recorded_or(cfg: &Config, data_dir: Option<PathBuf>) -> Option<PathBuf> {
    data_dir.or_else(|| Routing::load(cfg).and_then(|routing| routing.data_dir))
}

/// Atomic on unix. On Windows a running exe can be renamed but not replaced, so
/// a wrapped engine in use is moved aside first.
fn replace(new: &Path, bin: &Path) -> Result<()> {
    if cfg!(windows) && bin.exists() {
        return replace_aside(new, bin);
    }
    std::fs::rename(new, bin).ctx(format!("replacing {}", bin.display()))
}

fn replace_aside(new: &Path, bin: &Path) -> Result<()> {
    let aside = sibling(
        bin,
        &format!("{DISPLACED_PREFIX}{}", uuid::Uuid::new_v4().simple()),
    );
    swap(bin, new, &aside)?;
    let _ = std::fs::remove_file(aside);
    Ok(())
}

fn roots_or_default(data_dir: Option<PathBuf>) -> Vec<PathBuf> {
    match data_dir {
        Some(dir) => vec![dir],
        None => default_roots(),
    }
}

/// [`install`] without printing, for callers with their own output.
pub fn route(cfg: &Config, data_dir: Option<PathBuf>, watch: bool) -> Result<Report> {
    let _progress = ui::progress("Preparing Claude Desktop...");
    let _lock = lock(cfg)?;
    let data_dir = recorded_or(cfg, data_dir);
    let exe = std::env::current_exe().ctx("locating the dense binary")?;
    if watch {
        // The user's own choice now, so leaving an org won't undo it.
        let _ = std::fs::remove_file(forced_file(cfg));
    }
    let found = installs(&roots_or_default(data_dir.clone()));
    let mut report = Report::default();
    let mut failed = None;
    let mut routed = 0;
    for found in &found {
        match found.wrap(&exe, true, bundle::CODESIGNED) {
            Ok(Outcome::Wrapped) => {
                routed += 1;
                report
                    .fields
                    .push(("Engine", format!("{}: ready", found.label())));
            }
            Ok(Outcome::Current) => {
                routed += 1;
                report
                    .fields
                    .push(("Engine", format!("{}: already ready", found.label())));
            }
            Ok(Outcome::Skipped(why)) => {
                report
                    .warnings
                    .push(format!("Engine {} skipped: {why}", found.label()));
            }
            // With the watcher on, a later pass tries again.
            Err(e) => {
                report
                    .warnings
                    .push(format!("Could not connect engine {}: {e}", found.label()));
                failed = Some(e);
            }
        }
    }
    if let Some(e) = failed.filter(|_| !watch) {
        return Err(e);
    }
    let mut watching = false;
    if watch {
        let added_daemon = daemon::autostart::installed(cfg).is_none()
            || Routing::load(cfg).is_some_and(|routing| routing.added_daemon);
        Routing {
            added_daemon,
            data_dir: data_dir.clone(),
        }
        .save(cfg)?;
        match daemon::autostart::install(cfg) {
            Ok(autostart) => {
                watching = true;
                report
                    .fields
                    .push(("Service file", autostart.path.display().to_string()));
                report.warnings.extend(autostart.notes);
                report.fields.push((
                    "Updates",
                    "Enabled; new engines connect automatically".into(),
                ));
            }
            // Not fatal: wrapped engines keep working without the daemon.
            Err(e) => report.warnings.push(e.to_string()),
        }
    }
    if !watch {
        report
            .fields
            .push(("Updates", "Unchanged (--no-watch)".into()));
    }
    if routed == 0 {
        report.summary = nothing_routed(&found, watching, data_dir.as_deref()).into();
        return Ok(report);
    }
    // Desktop passes no flags, so the engine only ever sees the default profile.
    let engine = Config::resolve(None, None)?;
    if engine.api_host != cfg.api_host {
        report.warnings.push(format!(
            "this command targeted {}, but Desktop's engine uses your default profile, {}. \
             `dense profile` changes it.",
            cfg.api_host, engine.api_host
        ));
    }
    let logged_in = auth::load_creds(&engine).is_authenticated();
    let off = cfg!(feature = "desktop-switch") && daemon::managed::switched_off(cfg);
    report.ready = logged_in && !off && routed == found.len();
    report.summary = if off {
        "Setup saved. Routing is paused by your organization.".into()
    } else if !logged_in {
        "Setup saved. Log in to connect your local Code sessions: `dense login`.".into()
    } else if routed == found.len() {
        format!("Local Code sessions now route through {}", engine.api_host)
    } else {
        format!(
            "{routed} of {} local engines connected; some engines need attention",
            found.len()
        )
    };
    Ok(report)
}

fn routing_file(cfg: &Config) -> PathBuf {
    cfg.data_dir().join(ROUTING_FILE)
}

fn sibling(path: &Path, name: &str) -> PathBuf {
    path.with_file_name(name)
}

/// A copy of dense beside `bin`, ready for the rename that puts it in place.
fn stage(exe: &Path, bin: &Path) -> Result<PathBuf> {
    let tmp = sibling(bin, TMP);
    std::fs::copy(exe, &tmp).ctx(format!("copying dense to {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .ctx(format!("marking {} executable", tmp.display()))?;
    }
    Ok(tmp)
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    out.sort();
    out
}

/// Put `new` at `path`, moving any current `path` to `aside`. A failure puts
/// `path` back.
fn swap(path: &Path, new: &Path, aside: &Path) -> Result<()> {
    if path.exists() {
        std::fs::rename(path, aside).ctx(format!("moving {}", path.display()))?;
    }
    let placed = std::fs::rename(new, path).ctx(format!("installing {}", path.display()));
    if placed.is_err() {
        let _ = std::fs::rename(aside, path);
    }
    placed
}

fn sweep_displaced(bin: &Path) {
    let Some(entries) = bin.parent().and_then(|d| std::fs::read_dir(d).ok()) else {
        return;
    };
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(DISPLACED_PREFIX)
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn wrap_new(cfg: &Config, exe: &Path, watcher: &mut Watcher, rewraps: &mut Rewraps) {
    let Ok(_lock) = lock(cfg).inspect_err(|e| tracing::warn!("no Desktop engine pass: {e}")) else {
        return;
    };
    let routing = match Routing::load(cfg) {
        Some(routing) => routing,
        None if daemon::managed::forced_on(cfg) => {
            // Written first: what can't be undone later must not be wrapped now.
            if let Err(e) = std::fs::write(forced_file(cfg), "") {
                tracing::warn!("not routing Desktop: {e}");
                return;
            }
            Routing::default()
        }
        None if daemon::managed::no_longer_managed(cfg) => return restore_forced(cfg),
        None => return,
    };
    let refresh = !watcher.refreshed;
    let found = installs(&roots_or_default(routing.data_dir));
    // Gone with Desktop, or replaced by a newer version.
    watcher
        .unrouted
        .retain(|dir, _| found.iter().any(|f| &f.dir == dir));
    rewraps.retain(|dir, _| found.iter().any(|f| f.dir.display().to_string() == *dir));
    let now = daemon::managed::now();
    for found in &found {
        let key = found.dir.display().to_string();
        if rewraps.get(&key).is_some_and(|r| r.held_until > now)
            || !watcher.due(&found.dir, &found.seen(), Instant::now())
        {
            continue;
        }
        let why = match found.wrap(exe, refresh, bundle::CODESIGNED) {
            Ok(Outcome::Current) => {
                watcher.unrouted.remove(&found.dir);
                rewraps.remove(&key);
                continue;
            }
            Ok(Outcome::Wrapped) => {
                watcher.unrouted.remove(&found.dir);
                let rewrap = rewraps.entry(key).or_default();
                rewrap.count += 1;
                if rewrap.count >= REWRAP_LIMIT {
                    rewrap.held_until = now + HOLD_SECS;
                    tracing::warn!(
                        "{}: its wrapper keeps disappearing; leaving it alone for an hour",
                        found.label()
                    );
                }
                continue;
            }
            Ok(Outcome::Skipped(why)) => why,
            Err(e) => e.to_string(),
        };
        tracing::warn!("{} not routed: {why}", found.label());
        watcher
            .unrouted
            .insert(found.dir.clone(), (found.seen(), Instant::now()));
    }
    watcher.refreshed = true;
}

/// The org's forcing ended, so the engines go back. Routing the user chose
/// themselves never gets here: that has a [`Routing`] file.
fn restore_forced(cfg: &Config) {
    if !forced_file(cfg).exists() {
        return;
    }
    let mut clean = true;
    for found in installs(&default_roots()) {
        if let Err(e) = found.unwrap_engine() {
            tracing::warn!("could not restore {}: {e}", found.label());
            clean = false;
        }
    }
    if clean {
        tracing::info!("this device is no longer managed: Desktop's engines are restored");
        let _ = std::fs::remove_file(forced_file(cfg));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENGINE: &[u8] = b"real engine";
    const DENSE: &[u8] = b"dense copy";

    fn default_bundles() -> Bundles {
        bundle::CODESIGNED
    }

    fn published(root: &Path, payload: &[u8]) -> Install {
        let dir = root.join("2.1.309").join("abcdef012345");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(BIN), ENGINE).unwrap();
        std::fs::write(dir.join(".verified"), "manifest-checksum").unwrap();
        let sha = hex::encode(Sha256::digest(payload));
        std::fs::write(
            dir.join(".payload"),
            format!(r#"{{"sha256":"{sha}","size":1}}"#),
        )
        .unwrap();
        install_at(&dir, "2.1.309")
    }

    fn exe(root: &Path) -> PathBuf {
        let path = root.join("dense");
        std::fs::write(&path, DENSE).unwrap();
        path
    }

    #[test]
    fn wrap_then_unwrap_round_trips_the_engine() {
        let tmp = tempfile::tempdir().unwrap();
        let found = published(tmp.path(), ENGINE);
        let exe = exe(tmp.path());

        assert_eq!(
            found.wrap(&exe, true, default_bundles()).unwrap(),
            Outcome::Wrapped
        );
        assert_eq!(std::fs::read(&found.bin).unwrap(), DENSE);
        assert_eq!(std::fs::read(&found.real).unwrap(), ENGINE);
        assert!(found.is_wrapped());

        assert!(found.unwrap_engine().unwrap());
        assert_eq!(std::fs::read(&found.bin).unwrap(), ENGINE);
        assert!(!found.real.exists());
        assert!(!found.unwrap_engine().unwrap());
    }

    #[test]
    fn wrap_is_idempotent_and_keeps_the_real_engine() {
        let tmp = tempfile::tempdir().unwrap();
        let found = published(tmp.path(), ENGINE);
        let exe = exe(tmp.path());

        found.wrap(&exe, true, default_bundles()).unwrap();
        assert_eq!(
            found.wrap(&exe, true, default_bundles()).unwrap(),
            Outcome::Current
        );
        assert_eq!(
            found.wrap(&exe, false, default_bundles()).unwrap(),
            Outcome::Current
        );
        assert_eq!(std::fs::read(&found.real).unwrap(), ENGINE);
    }

    #[test]
    fn refresh_replaces_an_older_shim_only_when_asked() {
        let tmp = tempfile::tempdir().unwrap();
        let found = published(tmp.path(), ENGINE);
        let exe = exe(tmp.path());
        found.wrap(&exe, true, default_bundles()).unwrap();
        std::fs::write(&exe, b"newer dense").unwrap();

        assert_eq!(
            found.wrap(&exe, false, default_bundles()).unwrap(),
            Outcome::Current
        );
        assert_eq!(std::fs::read(&found.bin).unwrap(), DENSE);
        assert_eq!(
            found.wrap(&exe, true, default_bundles()).unwrap(),
            Outcome::Wrapped
        );
        assert_eq!(std::fs::read(&found.bin).unwrap(), b"newer dense");
        assert_eq!(std::fs::read(&found.real).unwrap(), ENGINE);
    }

    #[test]
    fn unknown_binary_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let found = published(tmp.path(), b"something else");
        let exe = exe(tmp.path());

        assert!(matches!(
            found.wrap(&exe, true, default_bundles()).unwrap(),
            Outcome::Skipped(_)
        ));
        assert_eq!(std::fs::read(&found.bin).unwrap(), ENGINE);
        assert!(!found.real.exists());
    }

    #[test]
    fn missing_payload_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let found = published(tmp.path(), ENGINE);
        std::fs::remove_file(found.dir.join(".payload")).unwrap();

        let outcome = found
            .wrap(&exe(tmp.path()), true, default_bundles())
            .unwrap();
        assert!(matches!(outcome, Outcome::Skipped(_)));
    }

    #[test]
    fn interrupted_wrap_is_repaired() {
        let tmp = tempfile::tempdir().unwrap();
        let found = published(tmp.path(), ENGINE);
        std::fs::rename(&found.bin, &found.real).unwrap();

        let outcome = found
            .wrap(&exe(tmp.path()), false, default_bundles())
            .unwrap();
        assert_eq!(outcome, Outcome::Wrapped);
        assert_eq!(std::fs::read(&found.bin).unwrap(), DENSE);
        assert_eq!(std::fs::read(&found.real).unwrap(), ENGINE);
    }

    #[test]
    fn a_genuine_engine_beside_a_stale_real_counts_as_unwrapped() {
        let tmp = tempfile::tempdir().unwrap();
        let found = published(tmp.path(), ENGINE);
        // A re-download: same size as the engine it replaced.
        std::fs::write(&found.real, b"stale build").unwrap();
        assert!(!found.is_wrapped());

        let outcome = found
            .wrap(&exe(tmp.path()), false, default_bundles())
            .unwrap();
        assert_eq!(outcome, Outcome::Wrapped);
        assert_eq!(std::fs::read(&found.bin).unwrap(), DENSE);
        assert_eq!(std::fs::read(&found.real).unwrap(), ENGINE);
    }

    #[test]
    fn a_stale_real_never_replaces_an_engine_desktop_did_not_record() {
        let tmp = tempfile::tempdir().unwrap();
        let found = published(tmp.path(), b"something else");
        std::fs::write(&found.real, b"stale build").unwrap();

        let outcome = found
            .wrap(&exe(tmp.path()), false, default_bundles())
            .unwrap();
        assert!(matches!(outcome, Outcome::Skipped(_)));
        assert_eq!(std::fs::read(&found.bin).unwrap(), ENGINE);
        assert_eq!(std::fs::read(&found.real).unwrap(), b"stale build");
    }

    #[test]
    fn a_failed_stage_moves_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let found = published(tmp.path(), ENGINE);
        let touched = || std::fs::metadata(&found.dir).unwrap().modified().unwrap();
        let before = touched();
        // Past the filesystem's timestamp granularity, so a rename would show.
        std::thread::sleep(Duration::from_millis(30));

        let gone = tmp.path().join("no-such-dense");
        assert!(found.wrap(&gone, true, default_bundles()).is_err());
        assert_eq!(touched(), before);
    }

    #[test]
    fn a_replacement_that_cannot_land_puts_the_old_file_back() {
        let tmp = tempfile::tempdir().unwrap();
        let found = published(tmp.path(), ENGINE);

        let gone = tmp.path().join("no-such-file");
        assert!(replace_aside(&gone, &found.bin).is_err());
        assert_eq!(std::fs::read(&found.bin).unwrap(), ENGINE);
    }

    #[test]
    fn a_replacement_that_lands_leaves_nothing_aside() {
        let tmp = tempfile::tempdir().unwrap();
        let found = published(tmp.path(), ENGINE);

        replace_aside(&exe(tmp.path()), &found.bin).unwrap();
        assert_eq!(std::fs::read(&found.bin).unwrap(), DENSE);
        let names: Vec<_> = std::fs::read_dir(&found.dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(!names.iter().any(|n| n.starts_with(DISPLACED_PREFIX)));
    }

    #[test]
    fn cloud_sessions_are_counted_across_accounts_and_bad_files_are_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("claude-code");
        assert_eq!(cloud_sessions(&root), 0);
        let store = tmp.path().join("local-agent-mode-sessions");
        for (org, body) in [
            ("a/1", r#"{"entries":[{"sessionId":"x","folders":[]},{}]}"#),
            ("b/2", r#"{"entries":[{}]}"#),
            ("c/3", "not json"),
        ] {
            std::fs::create_dir_all(store.join(org)).unwrap();
            std::fs::write(store.join(org).join("remote-session-spaces.json"), body).unwrap();
        }
        assert_eq!(cloud_sessions(&root), 3);
        assert!(cowork_status(3).contains("3 sessions"));
        assert!(!cowork_status(0).contains("with folder access"));
    }

    #[test]
    fn routing_survives_a_reload_and_is_gone_once_taken() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path(), "http://127.0.0.1:9");
        assert_eq!(Routing::load(&cfg), None);

        let routing = Routing {
            added_daemon: true,
            data_dir: Some(tmp.path().join("engines")),
        };
        routing.save(&cfg).unwrap();
        assert_eq!(Routing::load(&cfg).as_ref(), Some(&routing));

        assert_eq!(Routing::take(&cfg), Some(routing));
        assert_eq!(Routing::load(&cfg), None);
    }

    #[tokio::test]
    async fn the_daemon_job_wraps_new_engines_only_while_routing_is_on() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path(), "http://127.0.0.1:9");
        let root = tmp.path().join("engines");
        let found = published(&root, ENGINE);
        // The job wraps with this test binary, so the payload check is the real one.
        assert_eq!(
            watch(&cfg, &mut Watcher::default(), &mut Rewraps::new()).await,
            POLL
        );
        assert!(!found.is_wrapped());

        Routing {
            added_daemon: false,
            data_dir: Some(root),
        }
        .save(&cfg)
        .unwrap();
        watch(&cfg, &mut Watcher::default(), &mut Rewraps::new()).await;
        assert!(found.is_wrapped());
        assert_eq!(std::fs::read(&found.real).unwrap(), ENGINE);
    }

    #[tokio::test]
    async fn a_pass_queued_behind_uninstall_finds_routing_gone() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path(), "http://127.0.0.1:9");
        let root = tmp.path().join("engines");
        let found = published(&root, ENGINE);
        Routing {
            added_daemon: false,
            data_dir: Some(root),
        }
        .save(&cfg)
        .unwrap();

        let held = lock(&cfg).unwrap();
        let release = async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(!found.is_wrapped());
            Routing::take(&cfg);
            drop(held);
        };
        let mut watcher = Watcher::default();
        let mut rewraps = Rewraps::new();
        tokio::join!(watch(&cfg, &mut watcher, &mut rewraps), release);
        assert!(!found.is_wrapped());
    }

    #[tokio::test]
    async fn once_runs_the_jobs_and_leaves_the_pid_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path(), "http://127.0.0.1:9");
        let root = tmp.path().join("engines");
        let found = published(&root, ENGINE);
        Routing {
            added_daemon: false,
            data_dir: Some(root),
        }
        .save(&cfg)
        .unwrap();
        daemon::State {
            pid: Some(7),
            ..daemon::State::default()
        }
        .save(&cfg)
        .unwrap();

        daemon::run(&cfg, true).await.unwrap();
        assert!(found.is_wrapped());
        assert_eq!(daemon::State::load(&cfg).pid, Some(7));
        assert!(!daemon::running(&cfg));
    }

    #[test]
    fn a_recorded_data_dir_counts_as_where_the_engines_are() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path(), "http://127.0.0.1:9");
        let root = tmp.path().join("engines");
        let found = published(&root, ENGINE);
        found
            .wrap(&exe(tmp.path()), false, default_bundles())
            .unwrap();
        Routing {
            added_daemon: false,
            data_dir: Some(root),
        }
        .save(&cfg)
        .unwrap();

        assert!(in_use(&cfg));
        assert!(has_wrapped_engines(&cfg));
    }

    fn routed_at(cfg: &Config, root: &Path) {
        Routing {
            added_daemon: false,
            data_dir: Some(root.to_path_buf()),
        }
        .save(cfg)
        .unwrap();
    }

    #[test]
    fn an_engine_left_unrouted_is_checked_again_once_desktop_changes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path(), "http://127.0.0.1:9");
        let root = tmp.path().join("engines");
        // Desktop has written the engine but not yet its download record.
        let found = published(&root, b"not finished");
        routed_at(&cfg, &root);
        let exe = exe(tmp.path());
        let mut watcher = Watcher::default();

        wrap_new(&cfg, &exe, &mut watcher, &mut Rewraps::new());
        assert!(!found.is_wrapped());
        assert!(!watcher.due(&found.dir, &found.seen(), Instant::now()));

        let sha = hex::encode(Sha256::digest(ENGINE));
        std::fs::write(
            found.dir.join(".payload"),
            format!(r#"{{"sha256":"{sha}"}}"#),
        )
        .unwrap();
        assert!(watcher.due(&found.dir, &found.seen(), Instant::now()));
        wrap_new(&cfg, &exe, &mut watcher, &mut Rewraps::new());
        assert!(found.is_wrapped());
        assert!(watcher.unrouted.is_empty());
    }

    #[test]
    fn an_unchanged_unrouted_engine_waits_out_the_retry() {
        let tmp = tempfile::tempdir().unwrap();
        let found = published(tmp.path(), b"something else");
        let start = Instant::now();
        let mut watcher = Watcher::default();
        watcher
            .unrouted
            .insert(found.dir.clone(), (found.seen(), start));
        assert!(!watcher.due(&found.dir, &found.seen(), start + RETRY / 2));
        assert!(watcher.due(&found.dir, &found.seen(), start + RETRY));
    }

    #[test]
    fn the_watcher_forgets_engines_desktop_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path(), "http://127.0.0.1:9");
        let root = tmp.path().join("engines");
        let found = published(&root, b"something else");
        routed_at(&cfg, &root);
        let mut watcher = Watcher::default();
        wrap_new(&cfg, &exe(tmp.path()), &mut watcher, &mut Rewraps::new());
        assert_eq!(watcher.unrouted.len(), 1);

        std::fs::remove_dir_all(&found.dir).unwrap();
        wrap_new(&cfg, &exe(tmp.path()), &mut watcher, &mut Rewraps::new());
        assert!(watcher.unrouted.is_empty());
    }

    #[test]
    fn only_the_first_pass_replaces_older_wrappers() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path(), "http://127.0.0.1:9");
        let root = tmp.path().join("engines");
        let found = published(&root, ENGINE);
        routed_at(&cfg, &root);
        let exe = exe(tmp.path());
        found.wrap(&exe, false, default_bundles()).unwrap();
        std::fs::write(&exe, b"newer dense").unwrap();

        let mut watcher = Watcher::default();
        wrap_new(&cfg, &exe, &mut watcher, &mut Rewraps::new());
        assert_eq!(std::fs::read(&found.bin).unwrap(), b"newer dense");

        std::fs::write(&exe, b"newest dense").unwrap();
        wrap_new(&cfg, &exe, &mut watcher, &mut Rewraps::new());
        assert_eq!(std::fs::read(&found.bin).unwrap(), b"newer dense");
        assert_eq!(std::fs::read(&found.real).unwrap(), ENGINE);
    }

    #[test]
    fn a_wrapper_that_keeps_disappearing_is_left_alone_for_an_hour() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path(), "http://127.0.0.1:9");
        let root = tmp.path().join("engines");
        let found = published(&root, ENGINE);
        routed_at(&cfg, &root);
        let exe = exe(tmp.path());
        let mut watcher = Watcher::default();
        let mut rewraps = Rewraps::new();
        for _ in 0..REWRAP_LIMIT {
            wrap_new(&cfg, &exe, &mut watcher, &mut rewraps);
            std::fs::remove_file(&found.bin).unwrap();
        }
        wrap_new(&cfg, &exe, &mut watcher, &mut rewraps);
        assert!(!found.bin.exists());
        let until = rewraps.values().next().unwrap().held_until;
        let now = daemon::managed::now();
        assert!(until > now + HOLD_SECS - 60);
        assert!(summary(&cfg, &rewraps, now).contains("left alone for now"));
        assert!(!summary(&cfg, &rewraps, until).contains("left alone for now"));

        rewraps.values_mut().next().unwrap().held_until = 0;
        wrap_new(&cfg, &exe, &mut watcher, &mut rewraps);
        assert!(found.bin.exists());
    }

    #[test]
    fn an_engine_that_stays_wrapped_leaves_no_count_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path(), "http://127.0.0.1:9");
        let root = tmp.path().join("engines");
        published(&root, ENGINE);
        routed_at(&cfg, &root);
        let exe = exe(tmp.path());
        let mut watcher = Watcher::default();
        let mut rewraps = Rewraps::new();
        wrap_new(&cfg, &exe, &mut watcher, &mut rewraps);
        assert_eq!(rewraps.len(), 1);
        wrap_new(&cfg, &exe, &mut watcher, &mut rewraps);
        assert!(rewraps.is_empty());
    }

    #[test]
    fn one_unwrapped_engine_means_desktop_is_not_routed() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path(), "http://127.0.0.1:9");
        let root = tmp.path().join("engines");
        Routing {
            added_daemon: false,
            data_dir: Some(root.clone()),
        }
        .save(&cfg)
        .unwrap();
        assert!(!routes_every_engine(&cfg));
        assert!(summary(&cfg, &Rewraps::new(), 0).starts_with("no engine yet"));

        published(&root, ENGINE)
            .wrap(&exe(tmp.path()), false, default_bundles())
            .unwrap();
        assert!(routes_every_engine(&cfg));
        assert_eq!(summary(&cfg, &Rewraps::new(), 0), "1 of 1 engines routed");

        let newer = root.join("2.1.310");
        std::fs::create_dir_all(&newer).unwrap();
        std::fs::write(newer.join(BIN), ENGINE).unwrap();
        std::fs::write(newer.join(".verified"), "manifest-checksum").unwrap();
        assert!(!routes_every_engine(&cfg));
        assert!(summary(&cfg, &Rewraps::new(), 0).starts_with("1 of 2"));
    }

    #[test]
    fn an_explicit_daemon_install_clears_only_the_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path(), "http://127.0.0.1:9");
        keep_daemon(&cfg).unwrap();
        assert_eq!(Routing::load(&cfg), None);

        let data_dir = Some(tmp.path().join("engines"));
        let routing = |added_daemon| Routing {
            added_daemon,
            data_dir: data_dir.clone(),
        };
        routing(true).save(&cfg).unwrap();
        keep_daemon(&cfg).unwrap();
        assert_eq!(Routing::load(&cfg), Some(routing(false)));
    }

    #[test]
    fn installs_finds_build_folders_and_ignores_unpublished_ones() {
        let tmp = tempfile::tempdir().unwrap();
        published(tmp.path(), ENGINE);
        std::fs::create_dir_all(tmp.path().join("2.1.310").join("downloading")).unwrap();

        let found = installs(&[tmp.path().to_path_buf()]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].label(), "2.1.309/abcdef012345");
    }

    #[test]
    fn bundle_builds_point_at_both_bundles() {
        let tmp = tempfile::tempdir().unwrap();
        let macos = tmp.path().join("claude.app").join("Contents").join("MacOS");
        std::fs::create_dir_all(&macos).unwrap();

        let found = install_at(tmp.path(), "2.1.309");
        assert_eq!(found.bin, macos.join(BIN));
        assert_eq!(
            found.real,
            bundle::executable(&tmp.path().join(REAL_BUNDLE))
        );
    }
}

#[cfg(test)]
mod packaged_root_tests {
    use super::packaged_roots;

    #[test]
    fn finds_both_data_folders_and_skips_gateway_mode() -> std::io::Result<()> {
        let local = tempfile::tempdir()?;
        let path = |rel: &str| local.path().join(rel);
        for dir in [
            "Claude-Data/claude_pzs8sxrjxfjjc/claude-code",
            "Claude-Data/anthropicpbc.claude_fnn82j28hfe8t/claude-code",
            "Claude-Data/unrelated/claude-code",
            "Claude-3p/claude-code",
            "Packages/Claude_pzs8sxrjxfjjc/LocalCache/Roaming/Claude/claude-code",
            "Packages/Microsoft.Edge/LocalCache/Roaming/Claude/claude-code",
        ] {
            std::fs::create_dir_all(path(dir))?;
        }
        let mut found = packaged_roots(local.path());
        found.sort();
        let mut want = vec![
            path("Claude-Data/claude_pzs8sxrjxfjjc/claude-code"),
            path("Claude-Data/anthropicpbc.claude_fnn82j28hfe8t/claude-code"),
            path("Packages/Claude_pzs8sxrjxfjjc/LocalCache/Roaming/Claude/claude-code"),
        ];
        want.sort();
        assert_eq!(found, want);
        Ok(())
    }
}
