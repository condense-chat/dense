//! Persist/unpersist non-destructive shims so the bare tool name routes
//! through dense. A shim is a thin wrapper (`exec dense <tool> "$@"`) in
//! dense's override dir, which the env file puts ahead of the real tool on
//! PATH. The real binary is never moved, so unpersist is instant and safe.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::config::Config;
use crate::error::{Context, Error};
use crate::{env_file, tool};

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
    let wiring = env_file::ensure_env(cfg, modify_path)?;
    let report = install_shims(cfg, targets)?;
    remember_path_choice(cfg, modify_path)?;
    for (name, shim) in &report.persisted {
        println!("persisted {name} -> {}", shim.display());
    }
    for name in &report.skipped {
        println!("{name}: coming soon — skipped.");
    }
    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }
    report_wiring(cfg, &wiring);
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
    save_record(cfg, &rec)?;
    Ok(out)
}

/// Print the PATH-wiring follow-up for the standalone `dense persist`.
pub fn report_wiring(cfg: &Config, wiring: &env_file::PathWiring) {
    match wiring {
        env_file::PathWiring::Wired => {
            println!("PATH updated — {}.", env_file::reload_hint(cfg));
        }
        env_file::PathWiring::Skipped => {
            println!("PATH left unchanged (--no-modify-path).");
        }
        env_file::PathWiring::Manual(notes) => {
            for note in notes {
                println!("note: {note}");
            }
            println!(
                "PATH not wired — after the steps above, {}.",
                env_file::reload_hint(cfg)
            );
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
    for (name, removed) in remove_shims(cfg, targets)? {
        if removed {
            println!("unpersisted {name}");
        } else {
            println!("{name}: not persisted");
        }
    }
    Ok(())
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
