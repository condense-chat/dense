//! Claude Code through condense — `Claude<Anthropic>`.

use crate::Result;
use crate::api::dialect::Anthropic;
use crate::config::Config;
use crate::error::{Context, Error};
use crate::harness::{self, ProxyTarget, Tool};

pub struct Claude;

/// Which `--settings` form a `claude agents` invocation used, so the merged
/// value lands back in the right argv slot.
enum SettingsRef {
    Inline { token: usize },
    Separate { flag: usize },
}

impl Tool<Anthropic> for Claude {
    fn apply(&self, cmd: &mut tokio::process::Command, target: &ProxyTarget) {
        cmd.env("ANTHROPIC_BASE_URL", &target.base_url)
            .env("ANTHROPIC_CUSTOM_HEADERS", custom_headers(&target.headers))
            // Claude Code disables the 1M context window when the base URL is
            // not api.anthropic.com, silently falling back to 200K (compacts
            // ~140K). Assert first-party so the 1M window stays on through us.
            .env("_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL", "1")
            // Pin the auto-compact window to the full 1M. Read via parseInt, so
            // "1m" would parse to 1 — pass the literal token count. Overrides a
            // lower settings/experiment/model-default so we don't compact early.
            .env("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "1000000")
            // Force Tool Search on so MCP tool defs stay deferred out of context
            // (lazy-loaded via tool_reference) instead of loading eagerly every
            // turn. Claude Code disables it behind a non-first-party base URL;
            // condense forwards tool_reference blocks verbatim, so this is safe.
            .env("ENABLE_TOOL_SEARCH", "true");
    }

    fn binary(&self) -> &str {
        "claude"
    }

    fn label(&self) -> &str {
        "Claude Code"
    }

    /// `claude agents` dispatches background sessions whose env is stripped of
    /// `ANTHROPIC_BASE_URL`/`ANTHROPIC_CUSTOM_HEADERS`, so they only reach the
    /// proxy if the route rides in a `--settings` env block the agent view
    /// threads down. For that subcommand, merge ours in; leave every other
    /// invocation untouched.
    fn rewrite_args(&self, args: &[String], target: &ProxyTarget) -> Vec<String> {
        if args.first().map(String::as_str) != Some("agents") {
            return args.to_vec();
        }
        match merge_agents_settings(args, &target.base_url, &target.headers) {
            Ok(rewritten) => rewritten,
            Err(e) => {
                eprintln!(
                    "dense: leaving `claude agents --settings` as-is ({e}); \
                     dispatched agents may not route through condense"
                );
                args.to_vec()
            }
        }
    }
}

/// `dense claude` — Claude Code through the Anthropic proxy. The dialect is the
/// concrete `Anthropic`, so no proxy flag is threaded through the run path.
pub async fn run(cfg: &Config, args: &[String]) -> Result<()> {
    harness::launch(cfg, Claude, Anthropic, args).await
}

fn custom_headers(headers: &[(String, String)]) -> String {
    let existing = std::env::var("ANTHROPIC_CUSTOM_HEADERS").ok();
    merge_headers(existing.as_deref(), headers)
}

/// A `--settings` value is either an inline JSON object or a path to a JSON
/// file. Parse whichever it is into a value we can merge into.
fn load_settings(raw: &str) -> Result<serde_json::Value> {
    if raw.trim_start().starts_with('{') {
        serde_json::from_str(raw).ctx("parse --settings JSON")
    } else {
        let body = std::fs::read_to_string(raw).ctx(format!("read --settings file {raw}"))?;
        serde_json::from_str(&body).ctx(format!("parse --settings file {raw}"))
    }
}

/// Fold our route into the agent invocation's `--settings` `env` block —
/// reusing the existing one (inline or file) and keeping the user's keys, or
/// appending a fresh `--settings` when there is none. `ANTHROPIC_BASE_URL` is
/// ours; `ANTHROPIC_CUSTOM_HEADERS` merges through [`merge_headers`].
fn merge_agents_settings(
    args: &[String],
    base_url: &str,
    headers: &[(String, String)],
) -> Result<Vec<String>> {
    let mut found = None;
    for (i, arg) in args.iter().enumerate() {
        if arg == "--settings" {
            found = Some(SettingsRef::Separate { flag: i });
        } else if arg.starts_with("--settings=") {
            found = Some(SettingsRef::Inline { token: i });
        }
    }

    let existing = match &found {
        Some(SettingsRef::Inline { token }) => {
            args[*token].strip_prefix("--settings=").map(str::to_string)
        }
        Some(SettingsRef::Separate { flag }) => args.get(flag + 1).cloned(),
        None => None,
    };

    let mut settings = match existing.as_deref() {
        Some(raw) => load_settings(raw)?,
        None => serde_json::json!({}),
    };
    let env = settings
        .as_object_mut()
        .ok_or_else(|| Error::msg("--settings is not a JSON object"))?
        .entry("env")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or_else(|| Error::msg("settings `env` is not a JSON object"))?;
    let merged_headers = merge_headers(
        env.get("ANTHROPIC_CUSTOM_HEADERS")
            .and_then(serde_json::Value::as_str),
        headers,
    );
    env.insert(
        "ANTHROPIC_BASE_URL".to_string(),
        serde_json::Value::String(base_url.to_string()),
    );
    env.insert(
        "ANTHROPIC_CUSTOM_HEADERS".to_string(),
        serde_json::Value::String(merged_headers),
    );
    let value = serde_json::to_string(&settings).ctx("serialize merged --settings")?;

    let mut out = args.to_vec();
    match found {
        Some(SettingsRef::Inline { token }) => out[token] = format!("--settings={value}"),
        Some(SettingsRef::Separate { flag }) if flag + 1 < out.len() => out[flag + 1] = value,
        Some(SettingsRef::Separate { .. }) => out.push(value),
        None => {
            out.push("--settings".to_string());
            out.push(value);
        }
    }
    Ok(out)
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
    fn non_agents_argv_is_untouched() {
        let target = ProxyTarget {
            base_url: "https://api/anthropic".to_string(),
            headers: vec![],
        };
        let args = vec!["--print".to_string(), "hi".to_string()];
        assert_eq!(Claude.rewrite_args(&args, &target), args);
    }

    #[test]
    fn agents_without_settings_appends_env_block() {
        let headers = vec![("x-condense-user-id".to_string(), "u".to_string())];
        let args = vec!["agents".to_string(), "--cwd".to_string(), "/x".to_string()];
        let out = merge_agents_settings(&args, "https://api/anthropic", &headers).unwrap();
        assert_eq!(out[..3], args[..]);
        assert_eq!(out[3], "--settings");
        let v: serde_json::Value = serde_json::from_str(&out[4]).unwrap();
        assert_eq!(v["env"]["ANTHROPIC_BASE_URL"], "https://api/anthropic");
        assert_eq!(
            v["env"]["ANTHROPIC_CUSTOM_HEADERS"],
            "x-condense-user-id: u"
        );
    }

    #[test]
    fn agents_merges_into_inline_settings_keeping_user_keys() {
        let headers = vec![("x-condense-session-id".to_string(), "s".to_string())];
        let args = vec![
            "agents".to_string(),
            "--settings={\"model\":\"opus\",\"env\":{\"FOO\":\"bar\"}}".to_string(),
        ];
        let out = merge_agents_settings(&args, "https://api/anthropic", &headers).unwrap();
        let raw = out[1].strip_prefix("--settings=").unwrap();
        let v: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert_eq!(v["model"], "opus");
        assert_eq!(v["env"]["FOO"], "bar");
        assert_eq!(v["env"]["ANTHROPIC_BASE_URL"], "https://api/anthropic");
        assert_eq!(
            v["env"]["ANTHROPIC_CUSTOM_HEADERS"],
            "x-condense-session-id: s"
        );
    }

    #[test]
    fn agents_merges_separate_settings_value_and_combines_headers() {
        let headers = vec![("x-condense-user-id".to_string(), "u".to_string())];
        let args = vec![
            "agents".to_string(),
            "--settings".to_string(),
            "{\"env\":{\"ANTHROPIC_CUSTOM_HEADERS\":\"x-user: keep\"}}".to_string(),
        ];
        let out = merge_agents_settings(&args, "https://b", &headers).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out[2]).unwrap();
        assert_eq!(v["env"]["ANTHROPIC_BASE_URL"], "https://b");
        assert_eq!(
            v["env"]["ANTHROPIC_CUSTOM_HEADERS"],
            "x-user: keep\nx-condense-user-id: u"
        );
    }
}
