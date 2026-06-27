use serde_json::{Map, Value, json};

use crate::Result;
use crate::api::Api;
use crate::api::auth;
use crate::api::session::Session;
use crate::config::Config;
use crate::{harness, tool};

/// `dense opencode` — OpenCode routed through condense.
pub async fn run(cfg: &Config, args: &[String]) -> Result<()> {
    let creds = auth::ensure_auth(cfg).await?;
    let api = Api::authed(cfg, &creds)?;
    let session = Session::new();
    let bin = tool::resolve_real(cfg, "opencode")?;

    if args.is_empty() {
        harness::announce(cfg, "OpenCode");
    }

    let headers = harness::condense_headers(cfg, &creds, &session.id);
    note_active_providers();

    let mut cmd = tokio::process::Command::new(&bin);
    cmd.env(
        "OPENCODE_CONFIG_CONTENT",
        build_config(&cfg.api_base_url, &headers),
    );
    cmd.args(args);

    harness::spawn_and_wait(&api, &session, &bin, cmd).await
}

/// The ephemeral OpenCode config: two condense providers, each pointed at a
/// condense route with the `x-condense-*` headers and (when present) the BYO
/// upstream key. Minified — it travels as an env var.
fn build_config(api_base_url: &str, headers: &[(String, String)]) -> String {
    let base = api_base_url.trim_end_matches('/');
    let provider = json!({
        "condense-anthropic": {
            "npm": "@ai-sdk/anthropic",
            "name": "Condense (Anthropic)",
            "options": provider_options(
                format!("{base}/anthropic/v1"),
                headers,
                std::env::var("ANTHROPIC_API_KEY").ok(),
            ),
            "models": {
                "claude-opus-4-8": {},
                "claude-sonnet-4-6": {},
                "claude-haiku-4-5": {},
            },
        },
        "condense-openai": {
            "npm": "@ai-sdk/openai-compatible",
            "name": "Condense (OpenAI)",
            "options": provider_options(
                format!("{base}/openai/v1"),
                headers,
                std::env::var("OPENAI_API_KEY").ok(),
            ),
            "models": {
                "gpt-4o": {},
                "gpt-4o-mini": {},
            },
        },
    });
    json!({ "provider": provider }).to_string()
}

fn provider_options(
    base_url: String,
    headers: &[(String, String)],
    api_key: Option<String>,
) -> Value {
    let mut header_map = Map::new();
    for (name, value) in headers {
        header_map.insert(name.clone(), Value::String(value.clone()));
    }
    let mut options = Map::new();
    options.insert("baseURL".to_string(), Value::String(base_url));
    options.insert("headers".to_string(), Value::Object(header_map));
    if let Some(key) = api_key.filter(|k| !k.is_empty()) {
        options.insert("apiKey".to_string(), Value::String(key));
    }
    Value::Object(options)
}

fn note_active_providers() {
    let has = |k: &str| std::env::var(k).is_ok_and(|v| !v.is_empty());
    let mut active = Vec::new();
    if has("ANTHROPIC_API_KEY") {
        active.push("condense-anthropic");
    }
    if has("OPENAI_API_KEY") {
        active.push("condense-openai");
    }
    if active.is_empty() {
        eprintln!(
            "  no upstream key set — export ANTHROPIC_API_KEY or OPENAI_API_KEY to use a provider"
        );
    } else {
        eprintln!("  active providers: {}", active.join(", "));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_has_both_providers_and_routes() {
        let headers = vec![("x-condense-session-id".to_string(), "s".to_string())];
        let raw = build_config("https://api.example.com/", &headers);
        let v: Value = serde_json::from_str(&raw).unwrap();
        let p = &v["provider"];
        assert_eq!(
            p["condense-anthropic"]["options"]["baseURL"],
            "https://api.example.com/anthropic/v1"
        );
        assert_eq!(
            p["condense-openai"]["options"]["baseURL"],
            "https://api.example.com/openai/v1"
        );
        assert_eq!(
            p["condense-anthropic"]["options"]["headers"]["x-condense-session-id"],
            "s"
        );
    }

    #[test]
    fn api_key_omitted_when_unset() {
        let opts = provider_options("https://x/anthropic/v1".to_string(), &[], None);
        assert!(opts.get("apiKey").is_none());
        let opts = provider_options(
            "https://x/anthropic/v1".to_string(),
            &[],
            Some(String::new()),
        );
        assert!(opts.get("apiKey").is_none());
        let opts = provider_options(
            "https://x/anthropic/v1".to_string(),
            &[],
            Some("k".to_string()),
        );
        assert_eq!(opts["apiKey"], "k");
    }
}
