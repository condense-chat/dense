//! Claude Code through condense — `Claude<Anthropic>`.

use crate::Result;
use crate::api::dialect::Anthropic;
use crate::config::Config;
use crate::harness::{self, ProxyTarget, Tool};

pub struct Claude;

impl Tool<Anthropic> for Claude {
    fn apply(&self, cmd: &mut tokio::process::Command, target: &ProxyTarget) {
        cmd.env("ANTHROPIC_BASE_URL", &target.base_url)
            .env("ANTHROPIC_CUSTOM_HEADERS", custom_headers(&target.headers))
            // Claude Code disables the 1M context window when the base URL is
            // not api.anthropic.com, silently falling back to 200K (compacts
            // ~140K). Assert first-party so the 1M window stays on through us.
            .env("_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL", "1");
    }

    fn binary(&self) -> &str {
        "claude"
    }

    fn label(&self) -> &str {
        "Claude Code"
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
}
