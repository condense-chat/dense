//! Claude Code through condense (Anthropic dialect).

use crate::Result;
use crate::api::dialect::Dialect;
use crate::config::Config;
use crate::harness::{self, Target, Tool};

pub struct Claude;

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
        // Claude Code disables the 1M context window when the base URL is not
        // api.anthropic.com, silently falling back to 200K (compacts ~140K).
        // Assert first-party so the 1M window stays on through us — but only
        // when we forward to Anthropic. Behind an upstream override the base
        // URL genuinely is not first-party, and asserting it makes Claude Code
        // prepend an `x-anthropic-billing-header:` system block whose `cch=`
        // token is a per-request nonce: Anthropic's edge lifts it back out, a
        // gateway reads it as prompt text that differs every turn, and the
        // whole prefix cache is forfeit (measured 0% cache read on
        // gpt-5.6-luna and grok via Requesty, restored to ~99.5% without it).
        // That provider's own window governs there anyway.
        if target.upstream.is_none() {
            cmd.env("_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL", "1");
        }
        // Tool Search (deferred MCP tool defs via tool_reference) is an
        // Anthropic feature; behind an upstream override, respect how that
        // provider already works. Off must be explicit: the first-party
        // assert above makes Claude Code enable it by itself. A caller's
        // own value wins.
        if std::env::var_os("ENABLE_TOOL_SEARCH").is_none() {
            cmd.env(
                "ENABLE_TOOL_SEARCH",
                if target.upstream.is_some() {
                    "false"
                } else {
                    "true"
                },
            );
        }
    }

    fn binary(&self) -> &str {
        "claude"
    }

    fn label(&self) -> &str {
        "Claude Code"
    }
}

/// `dense claude` — Claude Code through the Anthropic proxy.
pub async fn run(cfg: &Config, args: &[String]) -> Result<()> {
    harness::launch(cfg, Claude, args).await
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

#[cfg(test)]
mod tests {
    use super::*;

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
        Claude.apply(&mut cmd, std::slice::from_ref(target));
        cmd.as_std()
            .get_envs()
            .find(|(key, _)| *key == name)
            .and_then(|(_, value)| value.map(|v| v.to_string_lossy().into_owned()))
    }

    fn tool_search_env(target: &Target) -> Option<String> {
        env_of(target, "ENABLE_TOOL_SEARCH")
    }
}
