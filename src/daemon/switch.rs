//! Apply the admin's on/off switch: on is `dense persist`, off is `dense
//! unpersist`, both through the persist module's own functions. Idempotent —
//! a tick that finds the machine already in the wanted state writes nothing.

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::config::Config;
use crate::{env_file, persist};

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

/// Bring the machine to `enabled`. `managed` is the tool set the switch
/// owns: filled from whatever is shimmed when first seen (all tools when
/// nothing is), kept across an off period so on restores the same set.
pub fn apply(
    cfg: &Config,
    managed: &mut Vec<String>,
    enabled: bool,
    modify_path: bool,
) -> Result<Outcome> {
    let present = shimmed(cfg);
    if !present.is_empty() && (!enabled || managed.is_empty()) {
        *managed = present.clone();
    }
    let changed = if enabled {
        if managed.is_empty() {
            *managed = persist::names().map(str::to_string).collect();
        }
        let missing: Vec<String> = managed
            .iter()
            .filter(|t| !present.contains(t))
            .cloned()
            .collect();
        if missing.is_empty() && cfg.env_file().exists() {
            false
        } else {
            env_file::ensure_env(cfg, modify_path)?;
            // An empty target list means "every tool" to persist.
            if !missing.is_empty() {
                persist::install_shims(cfg, &missing)?;
            }
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

    #[test]
    fn enable_persists_all_tools_once() {
        let root = tempfile::tempdir().unwrap();
        let cfg = cfg(root.path());
        let mut managed = Vec::new();
        let first = apply(&cfg, &mut managed, true, false).unwrap();
        assert_eq!(
            first,
            Outcome {
                changed: true,
                mode: Mode::Condensing
            }
        );
        let all: Vec<String> = persist::names().map(str::to_string).collect();
        assert_eq!(managed, all);
        for name in &all {
            assert!(persist::shim_path(&cfg, name).exists(), "{name} shim");
        }
        let before = mtime(&cfg, "claude");
        let second = apply(&cfg, &mut managed, true, false).unwrap();
        assert!(!second.changed);
        assert_eq!(mtime(&cfg, "claude"), before);
    }

    #[test]
    fn disable_removes_shims_once_and_reenable_restores_the_same_set() {
        let root = tempfile::tempdir().unwrap();
        let cfg = cfg(root.path());
        persist::install_shims(&cfg, &["claude".to_string()]).unwrap();
        env_file::ensure_env(&cfg, false).unwrap();

        let mut managed = Vec::new();
        let off = apply(&cfg, &mut managed, false, false).unwrap();
        assert_eq!(
            off,
            Outcome {
                changed: true,
                mode: Mode::Transparent
            }
        );
        assert_eq!(managed, vec!["claude".to_string()]);
        assert!(!persist::shim_path(&cfg, "claude").exists());
        assert!(persist::load_record(&cfg).tools.is_empty());

        assert!(!apply(&cfg, &mut managed, false, false).unwrap().changed);

        let on = apply(&cfg, &mut managed, true, false).unwrap();
        assert!(on.changed);
        assert!(persist::shim_path(&cfg, "claude").exists());
        assert!(!persist::shim_path(&cfg, "codex").exists());
    }

    #[test]
    fn enabled_repairs_a_missing_managed_shim() {
        let root = tempfile::tempdir().unwrap();
        let cfg = cfg(root.path());
        let mut managed = Vec::new();
        apply(&cfg, &mut managed, true, false).unwrap();
        persist::remove_shims(&cfg, &["codex".to_string()]).unwrap();
        let again = apply(&cfg, &mut managed, true, false).unwrap();
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
