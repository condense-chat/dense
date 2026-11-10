//! Persist/unpersist non-destructive shims so the bare tool name routes
//! through dense. A shim is a thin wrapper (`exec dense <tool> "$@"`) in
//! dense's override dir, which the env file puts ahead of the real tool on
//! PATH. The real binary is never moved, so unpersist is instant and safe.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::Result;
use crate::config::Config;
use crate::error::{Context, Error};
use crate::{atomic, daemon, env_file, tool, ui};

/// VS Code-family editors, by their dir under the OS config dir.
const EDITORS: &[&str] = &["Code", "Code - Insiders", "Cursor", "VSCodium", "Windsurf"];
const EDITOR_POLL: Duration = Duration::from_secs(30);
const TOOLS: &[ToolSpec] = &[
    ToolSpec {
        name: "claude",
        available: true,
        install_hint: "https://docs.claude.com/en/docs/claude-code",
    },
    ToolSpec {
        name: "opencode",
        available: true,
        install_hint: "https://opencode.ai",
    },
    ToolSpec {
        name: "codex",
        available: true,
        install_hint: "https://github.com/openai/codex",
    },
];
/// The Claude Code extension spawns `<wrapper> <its bundled claude> <args…>`;
/// pointed at dense, that runs as the routed engine (`desktop::engine`).
const WRAPPER_KEY: &str = "claudeCode.claudeProcessWrapper";

#[derive(Default, Serialize, Deserialize)]
pub struct Record {
    /// The user refused PATH wiring, which the daemon has to respect too.
    /// `None` on a record from before dense remembered the choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_modify_path: Option<bool>,
    pub tools: BTreeMap<String, String>,
}

/// What [`install_shims`] did — the caller renders it in whatever UI it is
/// running (cliclack frame in setup, plain lines for `dense persist`).
#[derive(Default)]
pub struct ShimReport {
    /// The daemon autostart entry that keeps the editors wired.
    pub daemon: Option<PathBuf>,
    /// Editor settings files whose Claude Code extension now routes via dense.
    pub editors: Vec<PathBuf>,
    /// `(tool, shim path)` for every shim written.
    pub persisted: Vec<(String, PathBuf)>,
    /// Tools selected but not yet supported.
    pub skipped: Vec<String>,
    /// Shims written for tools that aren't installed yet.
    pub warnings: Vec<String>,
}

struct ToolSpec {
    available: bool,
    install_hint: &'static str,
    name: &'static str,
}

/// What to do with each editor's `claudeCode.claudeProcessWrapper`.
#[derive(Clone, Copy)]
enum Wire<'a> {
    /// `unpersist`: drop ours.
    Clear,
    /// The daemon: point it at this dense only while unset or its dense is
    /// gone, so a daemon still on an older binary can't undo a newer `persist`.
    Repair(&'a str),
    /// `persist`: point it at this dense.
    Set(&'a str),
}

/// Whether a VS Code-family editor has run here, i.e. there's an extension
/// for dense to route.
pub fn has_editors(cfg: &Config) -> bool {
    !editor_settings(cfg).is_empty()
}

/// Install shims for the targets and record them. Does not touch PATH and
/// prints nothing — outcomes come back in the [`ShimReport`].
pub fn install_shims(cfg: &Config, targets: &[String]) -> Result<ShimReport> {
    let selected = select(targets)?;
    let mut rec = load_record(cfg);
    let mut report = ShimReport::default();
    for spec in selected {
        if !spec.available {
            report.skipped.push(spec.name.to_string());
            continue;
        }
        match tool::resolve_real(cfg, spec.name) {
            Ok(real) => {
                rec.tools
                    .insert(spec.name.to_string(), real.display().to_string());
            }
            Err(_) => {
                rec.tools.insert(spec.name.to_string(), String::new());
                report.warnings.push(format!(
                    "`{}` is not on PATH yet — shim installed; install it: {}",
                    spec.name, spec.install_hint
                ));
            }
        }
        write_shim(cfg, spec.name)?;
        report
            .persisted
            .push((spec.name.to_string(), shim_path(cfg, spec.name)));
    }
    save_record(cfg, &rec)?;
    Ok(report)
}

/// Whether the user allows dense to wire PATH.
pub fn may_modify_path(cfg: &Config) -> bool {
    let rec = load_record(cfg);
    match rec.no_modify_path {
        Some(refused) => !refused,
        // An older install that shimmed tools but left every profile alone
        // was set up with `--no-modify-path`.
        None => rec.tools.is_empty() || env_file::is_wired(cfg),
    }
}

pub fn load_record(cfg: &Config) -> Record {
    std::fs::read_to_string(cfg.persist_file())
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default()
}

/// Names of the tools dense can shim today.
pub fn names() -> impl Iterator<Item = &'static str> {
    TOOLS.iter().filter(|s| s.available).map(|s| s.name)
}

pub fn persist(cfg: &Config, targets: &[String], modify_path: bool) -> Result<()> {
    let progress = ui::progress("Setting up automatic routing...");
    let wiring = env_file::ensure_env(cfg, modify_path)?;
    let mut report = install_shims(cfg, targets)?;
    wire_claude(cfg, &mut report)?;
    remember_path_choice(cfg, modify_path)?;
    drop(progress);
    ui::title("Automatic routing");
    for (name, shim) in &report.persisted {
        ui::success(format!("`{name}` is set up to use Condense"));
        ui::detail(format!("Launcher: {}", shim.display()));
    }
    for settings in &report.editors {
        ui::success("Claude Code in your editor is set up to use Condense");
        ui::detail(format!("Settings: {}", settings.display()));
    }
    if let Some(entry) = &report.daemon {
        ui::field(
            "Editor updates",
            "Enabled; settings are repaired automatically",
        );
        ui::detail(format!("Service file: {}", entry.display()));
    }
    for name in &report.skipped {
        ui::note(format!("{name} is not supported yet; skipped"));
    }
    for warning in &report.warnings {
        ui::warning(warning);
    }
    report_wiring(cfg, &wiring);
    let names = report
        .persisted
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    ui::hint("Undo", &format!("dense unpersist {names}"));
    Ok(())
}

pub fn remember_path_choice(cfg: &Config, modify_path: bool) -> Result<()> {
    let mut rec = load_record(cfg);
    rec.no_modify_path = Some(!modify_path);
    save_record(cfg, &rec)
}

/// Remove the targets' shims and drop them from the record. Prints nothing;
/// returns `(tool, removed)` — `false` when it was not persisted.
pub fn remove_shims(cfg: &Config, targets: &[String]) -> Result<Vec<(String, bool)>> {
    let selected = select(targets)?;
    let mut rec = load_record(cfg);
    let mut out = Vec::new();
    for spec in selected {
        let path = shim_path(cfg, spec.name);
        let removed = match std::fs::remove_file(&path) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(e).ctx(format!("removing {}", path.display())),
        };
        rec.tools.remove(spec.name);
        out.push((spec.name.to_string(), removed));
    }
    // Recorded first, so daemon passes from here on leave the editors alone.
    // ponytail: a pass that read the record just before can still re-add the
    // wrapper; share desktop's lock if that shows up.
    save_record(cfg, &rec)?;
    if out.iter().any(|(name, _)| name == "claude") {
        unwire_editors(cfg);
    }
    Ok(out)
}

/// Print the PATH-wiring follow-up for the standalone `dense persist`.
pub fn report_wiring(cfg: &Config, wiring: &env_file::PathWiring) {
    match wiring {
        env_file::PathWiring::Wired => {
            ui::note(format!(
                "Shell setup saved. {}.",
                env_file::reload_hint(cfg)
            ));
        }
        env_file::PathWiring::Skipped => {
            ui::note("Shell setup left unchanged (--no-modify-path)");
            ui::detail(env_file::activate_hint(cfg));
        }
        env_file::PathWiring::Manual(notes) => {
            for note in notes {
                ui::warning(note);
            }
            ui::note(format!(
                "Shell setup needs attention. After the steps above, {}.",
                env_file::reload_hint(cfg)
            ));
        }
    }
}

pub fn shim_path(cfg: &Config, name: &str) -> PathBuf {
    #[cfg(windows)]
    let leaf = format!("{name}.cmd");
    #[cfg(not(windows))]
    let leaf = name.to_string();
    cfg.shim_dir().join(leaf)
}

pub fn unpersist(cfg: &Config, targets: &[String]) -> Result<()> {
    let progress = ui::progress("Removing automatic routing...");
    let removed = remove_shims(cfg, targets)?;
    drop(progress);
    ui::title("Automatic routing");
    for (name, removed) in &removed {
        if *removed {
            ui::success(format!("`{name}` no longer uses Condense automatically"));
        } else {
            ui::note(format!(
                "`{name}` is already disconnected. Nothing to remove"
            ));
        }
    }
    if removed.iter().any(|(name, _)| name == "claude") {
        daemon::release(cfg)?;
    }
    ui::hint("Try one session", "dense <tool>");
    Ok(())
}

/// Drop dense's wrapper from every editor, warning about files it can't edit.
pub fn unwire_editors(cfg: &Config) {
    for (path, outcome) in wire_editors(cfg, Wire::Clear) {
        if let Err(e) = outcome {
            ui::warning(format!(
                "Could not update editor settings in {}: {e}",
                path.display()
            ));
        }
    }
}

/// The daemon's editor job: put the wrapper back wherever it's missing — an
/// editor installed since, settings sync, a hand edit, or dense moved. The
/// extension itself needs nothing: each version passes its own engine path.
pub fn watch(cfg: &Config, warned: &mut Vec<String>) -> Duration {
    if !watching(cfg) {
        return EDITOR_POLL;
    }
    // A binary replaced under the daemon reads as `… (deleted)` on Linux; the
    // restarted daemon wires the new one.
    let Some(dense) = std::env::current_exe().ok().filter(|d| d.is_file()) else {
        return EDITOR_POLL;
    };
    let mut warnings = Vec::new();
    for (path, outcome) in wire_editors(cfg, Wire::Repair(&dense.display().to_string())) {
        match outcome {
            Ok(true) => tracing::info!("pointed Claude Code at dense in {}", path.display()),
            Ok(false) => {}
            Err(e) => warnings.push(format!("{}: {e}", path.display())),
        }
    }
    if warnings != *warned {
        for warning in &warnings {
            tracing::warn!("{warning}");
        }
        *warned = warnings;
    }
    EDITOR_POLL
}

/// Whether the daemon's editor job has work.
pub fn watching(cfg: &Config) -> bool {
    load_record(cfg).tools.contains_key("claude") && has_editors(cfg)
}

/// Once `claude` is persisted, point each editor's Claude Code extension at
/// this dense and have the daemon keep it there. `persist` and `setup` only:
/// from inside the daemon, re-activating its own autostart would stop it.
pub fn wire_claude(cfg: &Config, report: &mut ShimReport) -> Result<()> {
    // No editor, no daemon: a CLI-only machine gets nothing running for it.
    if !report.persisted.iter().any(|(name, _)| name == "claude") || !has_editors(cfg) {
        return Ok(());
    }
    let dense = std::env::current_exe().ctx("locating the dense binary")?;
    for (path, outcome) in wire_editors(cfg, Wire::Set(&dense.display().to_string())) {
        match outcome {
            Ok(_) => report.editors.push(path),
            Err(e) => report.warnings.push(format!("{}: {e}", path.display())),
        }
    }
    match daemon::want(cfg) {
        Ok(started) => {
            report.daemon = Some(started.path);
            report.warnings.extend(started.notes);
        }
        Err(e) => report.warnings.push(format!(
            "editor settings won't be re-applied automatically: {e}"
        )),
    }
    Ok(())
}

/// Where VS Code-family editors keep their settings, derived from `cfg` so a
/// test config never reaches the real ones.
pub(crate) fn editor_root(cfg: &Config) -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        return Some(cfg.home().join("Library").join("Application Support"));
    }
    cfg.config_dir().parent().map(Path::to_path_buf)
}

/// Settings files of the VS Code-family editors that have run here, each
/// profile's included.
fn editor_settings(cfg: &Config) -> Vec<PathBuf> {
    let Some(root) = editor_root(cfg) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for user in EDITORS.iter().map(|e| root.join(e).join("User")) {
        if !user.is_dir() {
            continue;
        }
        files.push(user.join("settings.json"));
        if let Ok(profiles) = std::fs::read_dir(user.join("profiles")) {
            let mut found: Vec<_> = profiles
                .flatten()
                .map(|p| p.path().join("settings.json"))
                .filter(|f| f.is_file())
                .collect();
            found.sort();
            files.extend(found);
        }
    }
    files
}

fn save_record(cfg: &Config, rec: &Record) -> Result<()> {
    std::fs::create_dir_all(cfg.data_dir()).ctx("creating dense data dir")?;
    let body = toml::to_string_pretty(rec).ctx("serializing persist record")?;
    let path = cfg.persist_file();
    crate::atomic::replace(&path, body.as_bytes()).ctx("replacing persist record")
}

fn select(targets: &[String]) -> Result<Vec<&'static ToolSpec>> {
    if targets.is_empty() {
        return Ok(TOOLS.iter().collect());
    }
    let mut out = Vec::new();
    for t in targets {
        match TOOLS.iter().find(|s| s.name == t) {
            Some(s) => out.push(s),
            None => {
                return Err(Error::Tool(format!(
                    "unknown tool `{t}` (known: claude, opencode, codex)"
                )));
            }
        }
    }
    Ok(out)
}

/// The shim file contents: a thin `exec dense <tool>` wrapper in the
/// platform's native script flavour.
#[cfg(not(windows))]
fn shim_body(dense: &str, name: &str) -> String {
    format!("#!/bin/sh\nexec \"{dense}\" {name} \"$@\"\n")
}

#[cfg(windows)]
fn shim_body(dense: &str, name: &str) -> String {
    format!("@\"{dense}\" {name} %*\r\n")
}

/// A replaced binary reports a path like `dense (deleted)`, and a shim to that
/// would break the tool until someone ran `dense persist` again.
fn target_exists(dense: &std::path::Path) -> Result<()> {
    if dense.exists() {
        return Ok(());
    }
    Err(Error::msg(format!(
        "not writing a shim: {} is gone",
        dense.display()
    )))
}

/// Whether the file changed. Renamed into place at the link's target, so a
/// failed write never empties it and a symlinked settings.json stays a link.
fn update_settings(path: &Path, wire: Wire) -> Result<bool> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).ctx(format!("reading {}", path.display())),
    };
    let Some(body) = with_wrapper(raw.strip_prefix('\u{feff}').unwrap_or(&raw), wire)? else {
        return Ok(false);
    };
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    atomic::replace(&target, &body).ctx(format!("writing {}", path.display()))?;
    Ok(true)
}

/// Apply `wire` to each editor's Claude Code extension, reporting per
/// settings file.
fn wire_editors(cfg: &Config, wire: Wire) -> Vec<(PathBuf, Result<bool>)> {
    editor_settings(cfg)
        .into_iter()
        .map(|path| {
            let outcome = update_settings(&path, wire);
            (path, outcome)
        })
        .collect()
}

/// `settings` with `wire` applied, or `None` when nothing changes. Someone
/// else's wrapper is never overridden.
// ponytail: plain JSON only — a settings.json with comments or trailing commas
// gets a hand-edit hint; swap in a JSONC-preserving writer if that's common.
fn with_wrapper(settings: &str, wire: Wire) -> Result<Option<Vec<u8>>> {
    let dense = match wire {
        Wire::Set(d) | Wire::Repair(d) => Some(d),
        Wire::Clear => None,
    };
    if dense.is_none() && !settings.contains(WRAPPER_KEY) {
        return Ok(None);
    }
    let mut map: Map<String, Value> = if settings.trim().is_empty() {
        Map::new()
    } else {
        serde_json::from_str(settings).map_err(|_| {
            Error::msg(match dense {
                Some(d) => format!(
                    "not plain JSON; add \"{WRAPPER_KEY}\": {} by hand",
                    Value::from(d)
                ),
                None => format!("not plain JSON; remove \"{WRAPPER_KEY}\" by hand"),
            })
        })?
    };
    // A field cleared in the Settings UI is left as "": unset, not foreign.
    let current = map
        .get(WRAPPER_KEY)
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .map(str::to_owned);
    let ours = current
        .as_deref()
        .is_some_and(|c| Path::new(c).file_stem().is_some_and(|s| s == "dense"));
    match (dense, current) {
        (Some(d), Some(c)) if c == d => return Ok(None),
        (Some(_), Some(c)) if !ours => {
            return Err(Error::msg(format!(
                "{WRAPPER_KEY} is already {c}; left as is"
            )));
        }
        (Some(_), Some(c)) if matches!(wire, Wire::Repair(_)) && Path::new(&c).exists() => {
            return Ok(None);
        }
        (Some(d), _) => {
            map.insert(WRAPPER_KEY.to_string(), d.into());
        }
        (None, _) if ours => {
            map.shift_remove(WRAPPER_KEY);
        }
        (None, _) => return Ok(None),
    }
    let mut out = Vec::new();
    let fmt = serde_json::ser::PrettyFormatter::with_indent(b"\t");
    map.serialize(&mut serde_json::Serializer::with_formatter(&mut out, fmt))
        .ctx("serializing editor settings")?;
    out.push(b'\n');
    Ok(Some(out))
}

fn write_shim(cfg: &Config, name: &str) -> Result<()> {
    let dense = std::env::current_exe().ctx("locating the dense binary")?;
    target_exists(&dense)?;
    std::fs::create_dir_all(cfg.shim_dir()).ctx("creating dense shim dir")?;
    let path = shim_path(cfg, name);

    #[cfg(not(windows))]
    {
        let body = shim_body(&cfg.sh_path(&dense), name);
        std::fs::write(&path, body).ctx(format!("writing {}", path.display()))?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(windows)]
    {
        let body = shim_body(&cfg.win_path(&dense), name);
        std::fs::write(&path, body).ctx(format!("writing {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shim_never_points_at_a_binary_that_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("dense");
        std::fs::write(&live, "").unwrap();
        assert!(target_exists(&live).is_ok());
        assert!(target_exists(&dir.path().join("dense (deleted)")).is_err());
    }

    #[test]
    fn a_wrapper_dropped_from_settings_is_put_back_beside_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        let dense = Wire::Set("/u/bin/dense");
        assert!(!update_settings(&path, Wire::Clear).unwrap());
        assert!(!path.exists());
        assert!(update_settings(&path, dense).unwrap());
        assert!(!update_settings(&path, dense).unwrap());

        std::fs::write(&path, r#"{"editor.fontSize": 14}"#).unwrap();
        assert!(update_settings(&path, dense).unwrap());
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("editor.fontSize") && raw.contains(WRAPPER_KEY));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_settings_file_is_rewritten_at_its_target() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("managed.json");
        std::fs::write(&target, "{}").unwrap();
        let link = tmp.path().join("settings.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(update_settings(&link, Wire::Set("/u/bin/dense")).unwrap());
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert!(
            std::fs::read_to_string(&target)
                .unwrap()
                .contains(WRAPPER_KEY)
        );
    }

    #[test]
    fn editors_are_found_under_the_configs_own_dirs_with_their_profiles() {
        let root = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(root.path(), "http://127.0.0.1:9");
        assert!(!has_editors(&cfg));
        let user = editor_root(&cfg).unwrap().join("Code").join("User");
        assert!(user.starts_with(root.path()));
        let profile = user.join("profiles").join("p1");
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::create_dir_all(user.join("profiles").join("unused")).unwrap();
        std::fs::write(profile.join("settings.json"), "\u{feff}{}").unwrap();

        let wired: Vec<_> = wire_editors(&cfg, Wire::Set("/x/dense"))
            .into_iter()
            .map(|(path, outcome)| outcome.map(|_| path).unwrap())
            .collect();
        assert_eq!(
            wired,
            [user.join("settings.json"), profile.join("settings.json")]
        );
        for file in &wired {
            assert!(std::fs::read_to_string(file).unwrap().contains("/x/dense"));
        }
    }

    #[test]
    fn editor_wrapper_is_set_kept_and_cleared() {
        let set = |s: &str, w: Wire| {
            with_wrapper(s, w)
                .unwrap()
                .map(|b| String::from_utf8(b).unwrap())
        };
        let dense = Wire::Set("/u/bin/dense");
        assert_eq!(
            set("", dense).as_deref(),
            Some("{\n\t\"claudeCode.claudeProcessWrapper\": \"/u/bin/dense\"\n}\n")
        );
        let wired = set(r#"{"b": 1, "a": 2}"#, dense).unwrap();
        assert!(wired.starts_with("{\n\t\"b\": 1,\n\t\"a\": 2,"));
        assert_eq!(set(&wired, dense), None);
        assert!(set(&wired, Wire::Set("/moved/dense")).is_some());
        assert_eq!(
            set(&wired, Wire::Clear).as_deref(),
            Some("{\n\t\"b\": 1,\n\t\"a\": 2\n}\n")
        );

        let theirs = r#"{"claudeCode.claudeProcessWrapper": "/opt/wrap.sh"}"#;
        assert!(with_wrapper(theirs, dense).is_err());
        assert_eq!(set(theirs, Wire::Clear), None);

        let cleared = r#"{"claudeCode.claudeProcessWrapper": ""}"#;
        assert!(set(cleared, Wire::Repair("/u/bin/dense")).is_some());

        let jsonc = "{ // mine\n}";
        assert!(with_wrapper(jsonc, dense).is_err());
        assert_eq!(set(jsonc, Wire::Clear), None);
    }

    #[test]
    fn the_daemon_repairs_only_a_missing_or_dangling_wrapper() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("dense");
        std::fs::write(&live, "").unwrap();
        let wired = |d: &Path| format!("{{\"{WRAPPER_KEY}\": {}}}", Value::from(d.to_str()));
        let repair = Wire::Repair("/old/daemon/dense");
        assert_eq!(with_wrapper(&wired(&live), repair).unwrap(), None);
        assert!(
            with_wrapper(&wired(&tmp.path().join("gone/dense")), repair)
                .unwrap()
                .is_some()
        );
        assert!(with_wrapper("{}", repair).unwrap().is_some());
    }

    #[cfg(not(windows))]
    #[test]
    fn shim_execs_dense_with_tool_name() {
        assert_eq!(
            shim_body("$HOME/.local/bin/dense", "claude"),
            "#!/bin/sh\nexec \"$HOME/.local/bin/dense\" claude \"$@\"\n"
        );
    }

    #[cfg(windows)]
    #[test]
    fn shim_execs_dense_with_tool_name() {
        assert_eq!(
            shim_body("%USERPROFILE%\\dense.exe", "claude"),
            "@\"%USERPROFILE%\\dense.exe\" claude %*\r\n"
        );
    }
}
