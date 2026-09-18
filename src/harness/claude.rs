//! Claude Code through condense (Anthropic dialect).

use std::path::Path;

use serde::Deserialize;

use crate::Result;
use crate::api::dialect::Dialect;
use crate::config::Config;
use crate::harness::commands;
use crate::harness::{self, Target, Tool};

pub struct Claude {
    /// Plugin dir carrying `/dense`; `None` when staging failed and the
    /// launch should proceed without the command.
    plugin_dir: Option<std::path::PathBuf>,
}

/// Claude Code's claude.ai login (`claudeAiOauth`). No `Debug`: it holds the token.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClaudeOauth {
    pub access_token: String,
    /// Epoch milliseconds.
    pub expires_at: i64,
}

#[derive(Deserialize)]
struct Stored {
    #[serde(rename = "claudeAiOauth")]
    oauth: ClaudeOauth,
}

impl Tool for Claude {
    fn dialects(&self) -> &'static [Dialect] {
        &[Dialect::Anthropic]
    }

    fn apply(&self, cmd: &mut tokio::process::Command, targets: &[Target]) {
        let target = &targets[0];
        cmd.env("ANTHROPIC_BASE_URL", &target.base_url)
            .env("ANTHROPIC_CUSTOM_HEADERS", custom_headers(&target.headers))
            // Pin the auto-compact window to the full 1M. Read via parseInt, so
            // "1m" would parse to 1 — pass the literal token count. Overrides a
            // lower settings/experiment/model-default so we don't compact early.
            .env("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "1000000");
        // Anthropic-only knobs, both keyed on the same fact. The first-party
        // assert keeps the 1M window on (Claude Code silently drops to 200K off
        // api.anthropic.com) but also makes it prepend an
        // `x-anthropic-billing-header:` system block whose `cch=` is a
        // per-request nonce: Anthropic's edge lifts it back out, a gateway reads
        // it as prompt text and forfeits the whole prefix cache (0% cache read
        // on gpt-5.6-luna and grok via Requesty, ~99.5% without it). Tool Search
        // is likewise Anthropic-only, and off must be explicit since a
        // first-party Claude Code turns it on by itself.
        anthropic_only(
            cmd,
            target,
            "_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL",
            "1",
            None,
        );
        anthropic_only(cmd, target, "ENABLE_TOOL_SEARCH", "true", Some("false"));
        // Session-scoped: loads in place for this process only, never
        // recorded in settings or the plugin cache.
        if let Some(dir) = &self.plugin_dir {
            cmd.arg("--plugin-dir").arg(dir);
        }
    }

    fn binary(&self) -> &str {
        "claude"
    }

    fn kind(&self) -> &'static str {
        "claude_code"
    }

    fn label(&self) -> &str {
        "Claude Code"
    }
}

/// `dense claude` — Claude Code through the Anthropic proxy.
pub async fn run(cfg: &Config, args: &[String]) -> Result<()> {
    commands::sweep_legacy(cfg);
    let plugin_dir = commands::claude_plugin_dir(cfg)
        .map_err(|e| eprintln!("  warning: could not stage the /dense command: {e}"))
        .ok();
    harness::launch(cfg, Claude { plugin_dir }, args).await
}

/// An Anthropic-only knob: `on` when we forward to Anthropic itself, `off`
/// behind an upstream override where that provider's own behaviour governs
/// (`None` leaves it unset). A caller's own value always wins.
fn anthropic_only(
    cmd: &mut tokio::process::Command,
    target: &Target,
    key: &str,
    on: &str,
    off: Option<&str>,
) {
    if std::env::var_os(key).is_some() {
        return;
    }
    let value = if target.upstream.is_some() {
        off
    } else {
        Some(on)
    };
    if let Some(value) = value {
        cmd.env(key, value);
    }
}

fn custom_headers(headers: &[(String, String)]) -> String {
    let existing = std::env::var("ANTHROPIC_CUSTOM_HEADERS").ok();
    merge_headers(existing.as_deref(), headers)
}

/// Newline-joined `Name: Value` for ANTHROPIC_CUSTOM_HEADERS. Preserves a
/// user's own entries from an inherited value; drops stale `x-condense-*` so
/// our fresh creds win.
fn merge_headers(existing: Option<&str>, headers: &[(String, String)]) -> String {
    let mut lines: Vec<String> = Vec::new();
    if let Some(existing) = existing {
        for line in existing.split('\n') {
            let name = line
                .split(':')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            if !line.trim().is_empty() && !name.starts_with("x-condense-") {
                lines.push(line.to_string());
            }
        }
    }
    for (name, value) in headers {
        lines.push(format!("{name}: {value}"));
    }
    lines.join("\n")
}

/// Claude Code's stored login: `.credentials.json`, else (macOS) its keychain item.
pub(crate) fn read_oauth(home: &Path) -> Option<ClaudeOauth> {
    let custom = std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let dir = commands::claude_home(home, |_| custom.clone());
    let parse = |raw: Vec<u8>| serde_json::from_slice::<Stored>(&raw).ok().map(|s| s.oauth);
    std::fs::read(dir.join(".credentials.json"))
        .ok()
        .and_then(parse)
        .or_else(|| keychain(custom.as_deref()).and_then(parse))
}

fn keychain(custom_dir: Option<&str>) -> Option<Vec<u8>> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let mut service = "Claude Code-credentials".to_string();
    if let Some(dir) = custom_dir {
        service.push('-');
        service.push_str(&commands::sha256_hex(dir.as_bytes())[..8]);
    }
    let user = std::env::var("USER").ok()?;
    let out = std::process::Command::new("security")
        .args(["find-generic-password", "-a", &user, "-w", "-s", &service])
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_login_parses_claude_code_credentials() {
        let raw = br#"{"claudeAiOauth":{"accessToken":"t","expiresAt":7,"scopes":["user:profile"]},"mcpOAuth":{}}"#;
        let s: Stored = serde_json::from_slice(raw).unwrap();
        assert_eq!(
            (s.oauth.access_token.as_str(), s.oauth.expires_at),
            ("t", 7)
        );
    }

    #[test]
    fn condense_headers_carry_claude_code_kind() {
        let cfg = Config::resolve(Some("https://api.example.com".into()), None).unwrap();
        let creds = crate::api::auth::Creds {
            token: Some("t".into()),
            user_id: Some("u".into()),
        };
        let h = harness::condense_headers(&cfg, &creds, "s", Claude { plugin_dir: None }.kind());
        assert!(h.contains(&("x-condense-kind".to_string(), "claude_code".to_string())));
    }

    #[test]
    fn merge_drops_stale_condense_headers_keeps_users() {
        let ours = vec![("x-condense-session-id".to_string(), "new".to_string())];
        let merged = merge_headers(
            Some("X-Condense-Auth-Token: stale\nx-my-header: keep\n"),
            &ours,
        );
        assert_eq!(merged, "x-my-header: keep\nx-condense-session-id: new");
    }

    #[test]
    fn merge_without_existing_is_just_ours() {
        let ours = vec![("x-condense-user-id".to_string(), "u".to_string())];
        assert_eq!(merge_headers(None, &ours), "x-condense-user-id: u");
    }

    #[test]
    fn first_party_assert_is_dropped_behind_an_upstream_override() {
        const KEY: &str = "_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL";
        assert_eq!(env_of(&target(None), KEY), Some("1".to_string()));
        assert_eq!(env_of(&target(Some("https://router.example")), KEY), None);
    }

    #[test]
    fn auto_compact_window_is_pinned_either_way() {
        const KEY: &str = "CLAUDE_CODE_AUTO_COMPACT_WINDOW";
        for t in [target(None), target(Some("https://router.example"))] {
            assert_eq!(env_of(&t, KEY), Some("1000000".to_string()));
        }
    }

    #[test]
    fn tool_search_follows_upstream_override() {
        assert_eq!(tool_search_env(&target(None)), Some("true".to_string()));
        assert_eq!(
            tool_search_env(&target(Some("https://router.example"))),
            Some("false".to_string())
        );
    }

    fn target(upstream: Option<&str>) -> Target {
        Target {
            route: "anthropic",
            base_url: "https://api.condense.chat/anthropic".to_string(),
            headers: vec![],
            upstream: upstream.map(str::to_string),
        }
    }

    fn env_of(target: &Target, name: &str) -> Option<String> {
        let mut cmd = tokio::process::Command::new("claude");
        Claude { plugin_dir: None }.apply(&mut cmd, std::slice::from_ref(target));
        cmd.as_std()
            .get_envs()
            .find(|(key, _)| *key == name)
            .and_then(|(_, value)| value.map(|v| v.to_string_lossy().into_owned()))
    }

    fn tool_search_env(target: &Target) -> Option<String> {
        env_of(target, "ENABLE_TOOL_SEARCH")
    }
}

#[cfg(test)]
mod plugin_tests {
    use super::*;

    #[test]
    fn plugin_dir_rides_the_argv_ahead_of_user_args() {
        let mut cmd = tokio::process::Command::new("claude");
        let claude = Claude {
            plugin_dir: Some(std::path::PathBuf::from("/d/harness/claude")),
        };
        claude.apply(
            &mut cmd,
            &[Target {
                route: "anthropic",
                base_url: "https://api.condense.chat/anthropic".to_string(),
                headers: vec![],
                upstream: None,
            }],
        );
        let argv: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(argv, ["--plugin-dir", "/d/harness/claude"]);
    }
}
