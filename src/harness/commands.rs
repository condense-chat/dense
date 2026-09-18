//! The `/dense:info` slash command, scoped to the tool process dense
//! launches. Claude Code loads it from a plugin dir under dense's data dir
//! via `--plugin-dir`; OpenCode takes it inline in `OPENCODE_CONFIG_CONTENT`.
//! Codex takes it as a plugin skill staged under its own plugin cache and
//! enabled for the one process by a `-c` override, plus a prompt file in
//! `$CODEX_HOME/prompts` for releases that still read it, written only when
//! nothing else owns that path and tracked in a manifest. Earlier releases
//! wrote into every tool's user-level command dir; those copies are swept
//! when their content is one dense shipped.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::Result;
use crate::config::Config;
use crate::error::Context;

const CLAUDE_BODY: &str = include_str!("../../assets/claude/info.md");
const CLAUDE_USAGE: &str = include_str!("../../assets/claude/usage.md");
const CODEX_BODY: &str = include_str!("../../assets/codex/info.md");
const CODEX_SKILL: &str = include_str!("../../assets/codex/SKILL.md");
const OPENCODE_BODY: &str = include_str!("../../assets/opencode/info.md");

/// sha256 of every command-file body a dense release has ever written into a
/// user dir. A file at a legacy path matching one of these is dense's to
/// remove; anything else there is the user's.
const SHIPPED: &[&str] = &[
    "09beb43ecc53326fad54abcc2703b1e61332fe4036093cde26236db39d226e11",
    "197d1b09efe9212deec1f4e0c6cd8d3fc269416a2e712bf5a1f76381bd8c1605",
    "1f38688375344bf6cd0ae54577fe7246c80cde4c4c730f50386ff6f6ed6ead71",
    "2871858f94a017d674e4561477626400f3eb401476822ad6f5cc8abfa1b5f0ef",
    "52eceffc44623528f8ba09356635ee53c96ef2ed51a43ebf9b719020caff339b",
    "5389044861d5132c21b6b887d967e169c530fc6445aa9ddb828f2a6c594421cb",
    "55e57707d85b9b5bf0e06fbba0b0e1134a0776f5d82b88ed6547c0cba19f5a6d",
    "8c3a7596f17946ea0c80afc2e09f4b578acabcb1e219b8318a4305483c783eb3",
    "976240f30f8a728bb04a9b42c5b31824823a7b174d34fd61101dc57c1516600a",
    "983ef67051a4616b3a5b6eb655fe3d070d46b92c7963a3c814bb9d71c605294e",
    "9fc77d006227047b6b29ac5710dfa6874d532f5a715b785e8c7cd38dacc0a8c2",
    "f0c9dda3e68ed67b1b22dca32a415ce978f353430ebc13cfe46cb8f8f1bea2d0",
    // assets/opencode/condense-thought-sig.js, once dropped in OpenCode's
    // global plugin dir.
    "a239a4ec828ec6c0daaba8eca2d143b577a06fffa0e11f4b984e1deb522038ea",
];

/// Both halves of the codex plugin id `dense@dense`: the cache path supplies
/// the marketplace name, so the two are the same string.
const CODEX_PLUGIN: &str = "dense";

const MANIFEST: &str = "manifest.toml";

/// Files dense wrote outside its own data dir, keyed by path, with the
/// sha256 of what it wrote. Uninstall removes each only while it still
/// matches.
#[derive(Default, Serialize, Deserialize)]
struct Manifest {
    #[serde(default)]
    files: BTreeMap<String, String>,
}

/// Stage the Claude Code plugin and hand back its root for `--plugin-dir`.
pub fn claude_plugin_dir(cfg: &Config) -> Result<PathBuf> {
    let root = cfg.data_dir().join("harness").join("claude");
    stage_claude_plugin(&root)?;
    Ok(root)
}

// Plugin `dense` with command `info` surfaces as `/dense:info` (and as `/info`
// unless the user has their own), `usage` as `/dense:usage`. The dir is
// dense-owned, so anything else under commands/ is a stale earlier name.
fn stage_claude_plugin(root: &Path) -> Result<()> {
    write_if_changed(
        &root.join(".claude-plugin").join("plugin.json"),
        &plugin_manifest(),
    )?;
    let commands = root.join("commands");
    write_if_changed(&commands.join("info.md"), CLAUDE_BODY)?;
    write_if_changed(&commands.join("usage.md"), CLAUDE_USAGE)?;
    if let Ok(entries) = fs::read_dir(&commands) {
        for e in entries.flatten() {
            if e.file_name() != "info.md" && e.file_name() != "usage.md" {
                let _ = fs::remove_file(e.path());
            }
        }
    }
    Ok(())
}

/// The `command` entry for `OPENCODE_CONFIG_CONTENT`.
pub fn opencode_command() -> Value {
    let (front, body) = split_frontmatter(OPENCODE_BODY);
    json!({ "dense:info": { "description": description(front), "template": body } })
}

/// Codex enables a plugin per process from argv; the body still has to sit in
/// its plugin cache, so dense stages one there and owns that whole subtree.
pub fn stage_codex_plugin(cfg: &Config) {
    let root = codex_plugin_dir(cfg.home(), |k| std::env::var(k).ok());
    let write = || -> Result<()> {
        write_if_changed(
            &root.join(".codex-plugin").join("plugin.json"),
            &codex_plugin_manifest(),
        )?;
        write_if_changed(
            &root.join("skills").join("info").join("SKILL.md"),
            CODEX_SKILL,
        )?;
        Ok(())
    };
    if let Err(e) = write() {
        eprintln!("dense: could not stage the codex plugin: {e}");
    }
    drop_stale_siblings(&root);
}

/// `-c` value enabling the staged plugin for this process only. Table form is
/// required: the dotted-key spelling parses but never reaches the loader.
pub fn codex_plugin_override() -> String {
    format!(r#"plugins={{"{CODEX_PLUGIN}@{CODEX_PLUGIN}"={{enabled=true}}}}"#)
}

/// Write the Codex prompt unless something else owns that path, and record
/// it so uninstall can take it back.
pub fn stage_codex_prompt(cfg: &Config) {
    let path = codex_prompt_path(cfg.home(), |k| std::env::var(k).ok());
    if let Ok(cur) = fs::read(&path)
        && !SHIPPED.contains(&sha256_hex(&cur).as_str())
    {
        return;
    }
    if let Err(e) = write_if_changed(&path, CODEX_BODY) {
        eprintln!("dense: could not write {}: {e}", path.display());
        return;
    }
    let _ = record(cfg, &path, CODEX_BODY.as_bytes());
}

/// Remove every legacy user-dir copy whose content dense shipped. Runs on
/// each launch (a handful of stats) and on uninstall.
pub fn sweep_legacy(cfg: &Config) {
    for path in legacy_paths(cfg.home(), |k| std::env::var(k).ok()) {
        remove_if_shipped(&path);
    }
}

/// Uninstall hook: manifest-tracked files (while unchanged) plus the legacy
/// sweep. Called before the data dir goes.
pub fn cleanup(cfg: &Config) {
    let manifest = load_manifest(cfg);
    for (path, sha) in &manifest.files {
        let path = Path::new(path);
        if fs::read(path).is_ok_and(|cur| sha256_hex(&cur) == *sha) {
            let _ = fs::remove_file(path);
        }
    }
    let env = |k: &str| std::env::var(k).ok();
    let owned = codex_plugin_owned_root(cfg.home(), env);
    let _ = fs::remove_dir_all(&owned);
    prune_empty_parents(&owned, &codex_home(cfg.home(), env));
    sweep_legacy(cfg);
}

/// Versions staged by an older dense next to the current one; codex would load
/// them as the same plugin id.
fn drop_stale_siblings(current: &Path) {
    let (Some(parent), Some(keep)) = (current.parent(), current.file_name()) else {
        return;
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    for e in entries.flatten() {
        if e.file_name() != keep {
            let _ = fs::remove_dir_all(e.path());
        }
    }
}

fn prune_empty_parents(from: &Path, stop: &Path) {
    let mut dir = from.parent();
    while let Some(d) = dir.filter(|d| *d != stop) {
        if fs::remove_dir(d).is_err() {
            return;
        }
        dir = d.parent();
    }
}

fn plugin_manifest() -> String {
    json!({
        "name": "dense",
        "description": "Condense account, lifetime savings, and this session's context and spend",
        "version": env!("CARGO_PKG_VERSION"),
    })
    .to_string()
}

pub(crate) fn codex_prompt_path(home: &Path, env: impl Fn(&str) -> Option<String>) -> PathBuf {
    codex_home(home, env).join("prompts").join("dense.md")
}

/// Dense owns everything under `<codex home>/plugins/cache/dense/dense`; the
/// `dense` path segment supplies the marketplace half of the plugin id, so no
/// marketplace has to be configured or to exist.
pub(crate) fn codex_plugin_dir(home: &Path, env: impl Fn(&str) -> Option<String>) -> PathBuf {
    codex_plugin_owned_root(home, env).join(env!("CARGO_PKG_VERSION"))
}

pub(crate) fn codex_plugin_owned_root(
    home: &Path,
    env: impl Fn(&str) -> Option<String>,
) -> PathBuf {
    codex_home(home, env)
        .join("plugins")
        .join("cache")
        .join(CODEX_PLUGIN)
        .join(CODEX_PLUGIN)
}

pub(crate) fn claude_home(home: &Path, env: impl Fn(&str) -> Option<String>) -> PathBuf {
    root(home, env("CLAUDE_CONFIG_DIR"), ".claude")
}

fn codex_home(home: &Path, env: impl Fn(&str) -> Option<String>) -> PathBuf {
    root(home, env("CODEX_HOME"), ".codex")
}

fn codex_plugin_manifest() -> String {
    json!({
        "name": CODEX_PLUGIN,
        "description": "Condense account, lifetime savings, and this session's context and spend",
        "version": env!("CARGO_PKG_VERSION"),
        "skills": "./skills/",
    })
    .to_string()
}

pub(crate) fn legacy_paths(home: &Path, env: impl Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    let claude = root(home, env("CLAUDE_CONFIG_DIR"), ".claude").join("commands");
    let codex = root(home, env("CODEX_HOME"), ".codex").join("prompts");
    let opencode = root(home, env("XDG_CONFIG_HOME"), ".config").join("opencode");
    vec![
        claude.join("dense.md"),
        claude.join("dense-context.md"),
        codex.join("dense-context.md"),
        opencode.join("commands").join("dense.md"),
        opencode.join("commands").join("dense-context.md"),
        opencode.join("plugins").join("condense-thought-sig.js"),
    ]
}

fn root(home: &Path, over: Option<String>, default: &str) -> PathBuf {
    over.filter(|v| !v.trim().is_empty())
        .map_or_else(|| home.join(default), PathBuf::from)
}

pub(crate) fn remove_if_shipped(path: &Path) -> bool {
    let Ok(cur) = fs::read(path) else {
        return false;
    };
    SHIPPED.contains(&sha256_hex(&cur).as_str()) && fs::remove_file(path).is_ok()
}

fn record(cfg: &Config, path: &Path, body: &[u8]) -> Result<()> {
    let mut m = load_manifest(cfg);
    m.files
        .insert(path.to_string_lossy().into_owned(), sha256_hex(body));
    let text = toml::to_string_pretty(&m).ctx("serializing manifest")?;
    write_if_changed(&cfg.data_dir().join(MANIFEST), &text).map(|_| ())
}

fn load_manifest(cfg: &Config) -> Manifest {
    fs::read_to_string(cfg.data_dir().join(MANIFEST))
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default()
}

/// `(frontmatter, body)` of a `---`-fenced markdown file; no fence means
/// the whole text is body.
pub(crate) fn split_frontmatter(text: &str) -> (&str, &str) {
    let Some(rest) = text.strip_prefix("---\n") else {
        return ("", text);
    };
    match rest.find("\n---\n") {
        Some(i) => (&rest[..i], rest[i + 5..].trim_start_matches('\n')),
        None => ("", text),
    }
}

fn description(front: &str) -> String {
    front
        .lines()
        .find_map(|l| l.strip_prefix("description:"))
        .map(|d| d.trim().to_owned())
        .unwrap_or_default()
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub(crate) fn write_if_changed(path: &Path, body: &str) -> Result<bool> {
    if fs::read_to_string(path).ok().as_deref() == Some(body) {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ctx(format!("creating {}", parent.display()))?;
    }
    fs::write(path, body).ctx(format!("writing {}", path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn only(key: &'static str, value: &'static str) -> impl Fn(&str) -> Option<String> {
        move |k| (k == key).then(|| value.to_owned())
    }

    #[test]
    fn codex_prompt_path_defaults_and_honors_codex_home() {
        let home = Path::new("/h");
        assert_eq!(
            codex_prompt_path(home, no_env),
            PathBuf::from("/h/.codex/prompts/dense.md")
        );
        assert_eq!(
            codex_prompt_path(home, only("CODEX_HOME", "/o")),
            PathBuf::from("/o/prompts/dense.md")
        );
        assert_eq!(
            codex_prompt_path(home, only("CODEX_HOME", "")),
            PathBuf::from("/h/.codex/prompts/dense.md")
        );
    }

    #[test]
    fn codex_plugin_lives_in_the_cache_dir_codex_reads() {
        let home = Path::new("/h");
        assert_eq!(
            codex_plugin_dir(home, no_env),
            PathBuf::from(format!(
                "/h/.codex/plugins/cache/dense/dense/{}",
                env!("CARGO_PKG_VERSION")
            ))
        );
        assert_eq!(
            codex_plugin_owned_root(home, only("CODEX_HOME", "/o")),
            PathBuf::from("/o/plugins/cache/dense/dense")
        );
    }

    #[test]
    fn codex_plugin_override_is_the_table_form_codex_accepts() {
        assert_eq!(
            codex_plugin_override(),
            r#"plugins={"dense@dense"={enabled=true}}"#
        );
    }

    #[test]
    fn legacy_paths_cover_every_dir_dense_once_wrote_to() {
        let home = Path::new("/h");
        let got = legacy_paths(home, no_env);
        let want = [
            "/h/.claude/commands/dense.md",
            "/h/.claude/commands/dense-context.md",
            "/h/.codex/prompts/dense-context.md",
            "/h/.config/opencode/commands/dense.md",
            "/h/.config/opencode/commands/dense-context.md",
            "/h/.config/opencode/plugins/condense-thought-sig.js",
        ];
        assert_eq!(got, want.map(PathBuf::from));
        let over = legacy_paths(home, only("CLAUDE_CONFIG_DIR", "/o"));
        assert_eq!(over[0], PathBuf::from("/o/commands/dense.md"));
        assert_eq!(over[2], PathBuf::from("/h/.codex/prompts/dense-context.md"));
    }

    #[test]
    fn shipped_bodies_include_the_current_assets() {
        for body in [CLAUDE_BODY, CODEX_BODY, OPENCODE_BODY] {
            assert!(SHIPPED.contains(&sha256_hex(body.as_bytes()).as_str()));
        }
        assert!(
            SHIPPED.contains(
                &sha256_hex(include_bytes!(
                    "../../assets/opencode/condense-thought-sig.js"
                ))
                .as_str()
            )
        );
    }

    #[test]
    fn remove_if_shipped_leaves_user_files_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ours = dir.path().join("ours.md");
        let theirs = dir.path().join("theirs.md");
        fs::write(&ours, CLAUDE_BODY).expect("write");
        fs::write(&theirs, "# my own /dense\n").expect("write");

        assert!(remove_if_shipped(&ours));
        assert!(!ours.exists());
        assert!(!remove_if_shipped(&theirs));
        assert!(theirs.exists());
        assert!(!remove_if_shipped(&dir.path().join("missing.md")));
    }

    #[test]
    fn split_frontmatter_separates_fence_from_body() {
        let (front, body) = split_frontmatter("---\ndescription: x\n---\n\nhello\n");
        assert_eq!(front, "description: x");
        assert_eq!(body, "hello\n");
        assert_eq!(split_frontmatter("plain\n"), ("", "plain\n"));
        assert_eq!(
            split_frontmatter("---\nunterminated\n"),
            ("", "---\nunterminated\n")
        );
    }

    #[test]
    fn opencode_command_carries_description_and_shell_template() {
        let v = opencode_command();
        let dense = v.get("dense:info").expect("dense:info");
        assert_eq!(
            dense["description"].as_str(),
            Some("Condense account, lifetime savings, and this session's context and spend")
        );
        let tpl = dense["template"].as_str().expect("template");
        assert!(tpl.starts_with("!`dense info"));
        assert!(!tpl.contains("---"));
    }

    #[test]
    fn claude_plugin_stages_info_and_drops_stale_command_names() {
        let dir = tempfile::tempdir().expect("tempdir");
        let commands = dir.path().join("commands");
        fs::create_dir_all(&commands).expect("mkdir");
        fs::write(commands.join("dense.md"), "old").expect("write");

        stage_claude_plugin(dir.path()).expect("stage");

        assert_eq!(
            fs::read_to_string(commands.join("info.md")).expect("read"),
            CLAUDE_BODY
        );
        assert_eq!(
            fs::read_to_string(commands.join("usage.md")).expect("read"),
            CLAUDE_USAGE
        );
        assert!(!commands.join("dense.md").exists());
        assert!(dir.path().join(".claude-plugin/plugin.json").exists());
    }

    #[test]
    fn plugin_manifest_is_named_dense() {
        let v: Value = serde_json::from_str(&plugin_manifest()).expect("json");
        assert_eq!(v["name"], "dense");
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn write_if_changed_creates_nested_dir_and_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a").join("b").join("dense.md");
        let body = "---\ndescription: x\n---\nbody\n";

        assert!(write_if_changed(&path, body).expect("first write"));
        assert_eq!(fs::read_to_string(&path).expect("read"), body);

        assert!(!write_if_changed(&path, body).expect("second write"));
        assert_eq!(fs::read_to_string(&path).expect("read"), body);
    }

    #[test]
    fn write_if_changed_restores_tampered_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dense.md");
        let body = "canonical\n";

        assert!(write_if_changed(&path, body).expect("first write"));
        fs::write(&path, "tampered\n").expect("tamper");

        assert!(write_if_changed(&path, body).expect("rewrite"));
        assert_eq!(fs::read_to_string(&path).expect("read"), body);
    }
}
