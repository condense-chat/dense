//! Apply the admin's on/off switch: on is `dense persist`, off is `dense
//! unpersist`, both through the persist module's own functions. Idempotent —
//! a tick that finds the machine already in the wanted state writes nothing.

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::config::Config;
use crate::{env_file, persist, tool};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// At least one tool is shimmed through condense.
    Condensing,
    /// No shims: every tool talks to its upstream directly.
    Transparent,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Outcome {
    /// Whether this apply wrote or removed anything.
    pub changed: bool,
    pub mode: Mode,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Condensing => "condensing",
            Self::Transparent => "transparent",
        }
    }
}

/// Bring the machine to `enabled`. On, every tool in `installed` gets a shim
/// (PATH wiring is touched only when a shim is actually added); off removes
/// every shim. Settled is judged from the shims alone, on every platform.
pub fn apply(
    cfg: &Config,
    enabled: bool,
    installed: &[String],
    modify_path: bool,
) -> Result<Outcome> {
    let present = shimmed(cfg);
    let changed = if enabled {
        let missing: Vec<String> = installed
            .iter()
            .filter(|t| !present.contains(t))
            .cloned()
            .collect();
        if missing.is_empty() {
            false
        } else {
            env_file::ensure_env(cfg, modify_path)?;
            persist::install_shims(cfg, &missing)?;
            true
        }
    } else if present.is_empty() {
        false
    } else {
        persist::remove_shims(cfg, &present)?;
        true
    };
    Ok(Outcome {
        changed,
        mode: observe(cfg),
    })
}

/// The shimmable tools that are installed (found on PATH outside the shim dir).
pub fn installed(cfg: &Config) -> Vec<String> {
    persist::names()
        .filter(|n| tool::resolve_real(cfg, n).is_ok())
        .map(str::to_string)
        .collect()
}

/// The mode the machine is actually in, from the shims on disk.
pub fn observe(cfg: &Config) -> Mode {
    if shimmed(cfg).is_empty() {
        Mode::Transparent
    } else {
        Mode::Condensing
    }
}

fn shimmed(cfg: &Config) -> Vec<String> {
    persist::names()
        .filter(|n| persist::shim_path(cfg, n).exists())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(root: &std::path::Path) -> Config {
        Config::for_test(root, "http://127.0.0.1:9")
    }

    fn mtime(cfg: &Config, name: &str) -> std::time::SystemTime {
        std::fs::metadata(persist::shim_path(cfg, name))
            .unwrap()
            .modified()
            .unwrap()
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn enable_shims_installed_tools_once() {
        let root = tempfile::tempdir().unwrap();
        let cfg = cfg(root.path());
        let tools = names(&["claude", "codex"]);
        let first = apply(&cfg, true, &tools, false).unwrap();
        assert_eq!(
            first,
            Outcome {
                changed: true,
                mode: Mode::Condensing
            }
        );
        assert!(persist::shim_path(&cfg, "claude").exists());
        assert!(persist::shim_path(&cfg, "codex").exists());
        assert!(!persist::shim_path(&cfg, "opencode").exists());
        let before = mtime(&cfg, "claude");
        let record_before = std::fs::read_to_string(cfg.persist_file()).unwrap();
        let second = apply(&cfg, true, &tools, false).unwrap();
        assert!(!second.changed);
        assert_eq!(mtime(&cfg, "claude"), before);
        assert_eq!(
            std::fs::read_to_string(cfg.persist_file()).unwrap(),
            record_before
        );
    }

    #[test]
    fn settled_does_not_depend_on_the_env_file() {
        let root = tempfile::tempdir().unwrap();
        let cfg = cfg(root.path());
        let tools = names(&["claude"]);
        apply(&cfg, true, &tools, false).unwrap();
        // Windows never writes one; a missing env file must not read as unsettled.
        let _ = std::fs::remove_file(cfg.env_file());
        assert!(!apply(&cfg, true, &tools, false).unwrap().changed);
    }

    #[test]
    fn nothing_installed_writes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let cfg = cfg(root.path());
        let out = apply(&cfg, true, &[], false).unwrap();
        assert_eq!(
            out,
            Outcome {
                changed: false,
                mode: Mode::Transparent
            }
        );
        assert!(!cfg.env_file().exists());
    }

    #[test]
    fn disable_removes_every_shim_once_and_reenable_restores() {
        let root = tempfile::tempdir().unwrap();
        let cfg = cfg(root.path());
        persist::install_shims(&cfg, &names(&["claude", "opencode"])).unwrap();
        let off = apply(&cfg, false, &names(&["claude"]), false).unwrap();
        assert_eq!(
            off,
            Outcome {
                changed: true,
                mode: Mode::Transparent
            }
        );
        assert!(!persist::shim_path(&cfg, "claude").exists());
        assert!(!persist::shim_path(&cfg, "opencode").exists());
        assert!(persist::load_record(&cfg).tools.is_empty());
        assert!(!apply(&cfg, false, &[], false).unwrap().changed);

        let on = apply(&cfg, true, &names(&["claude"]), false).unwrap();
        assert!(on.changed);
        assert!(persist::shim_path(&cfg, "claude").exists());
    }

    #[test]
    fn enabled_repairs_a_removed_shim() {
        let root = tempfile::tempdir().unwrap();
        let cfg = cfg(root.path());
        let tools = names(&["claude", "codex"]);
        apply(&cfg, true, &tools, false).unwrap();
        persist::remove_shims(&cfg, &names(&["codex"])).unwrap();
        let again = apply(&cfg, true, &tools, false).unwrap();
        assert!(again.changed);
        assert!(persist::shim_path(&cfg, "codex").exists());
    }

    #[test]
    fn observe_reads_shims() {
        let root = tempfile::tempdir().unwrap();
        let cfg = cfg(root.path());
        assert_eq!(observe(&cfg), Mode::Transparent);
        persist::install_shims(&cfg, &["opencode".to_string()]).unwrap();
        assert_eq!(observe(&cfg), Mode::Condensing);
    }
}
