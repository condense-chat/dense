use std::fs;
use std::path::PathBuf;

use serde_json::{Map, Value, json};

use crate::Result;
use crate::api::Api;
use crate::api::auth;
use crate::api::session::Session;
use crate::config::Config;
use crate::{harness, tool};

const THOUGHT_SIG_PLUGIN: &str = include_str!("../../assets/opencode/condense-thought-sig.js");

struct PluginInstall {
    created_root: Option<PathBuf>,
    file: Option<PathBuf>,
}

impl PluginInstall {
    fn noop() -> Self {
        Self {
            created_root: None,
            file: None,
        }
    }

    fn cleanup(self) {
        if let Some(root) = self.created_root {
            let _ = fs::remove_dir_all(root);
        } else if let Some(f) = self.file {
            let _ = fs::remove_file(f);
        }
    }
}

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
    let plugin = install_plugin().unwrap_or_else(|e| {
        eprintln!("  warning: could not install thought_signature plugin: {e}");
        PluginInstall::noop()
    });

    let mut cmd = tokio::process::Command::new(&bin);
    cmd.env(
        "OPENCODE_CONFIG_CONTENT",
        build_config(&cfg.api_base_url, &headers, parse_model_arg(args).as_ref()),
    );
    cmd.args(args);

    harness::spawn_and_wait(&api, &session, &bin, cmd, move || plugin.cleanup()).await
}

/// Install the session-scoped thought_signature plugin into
/// `<cwd>/.opencode/plugins/`, tracking only what we create so cleanup never
/// touches a pre-existing file or dir.
fn install_plugin() -> Result<PluginInstall> {
    let cwd = std::env::current_dir()?;
    let opencode = cwd.join(".opencode");
    let plugins = opencode.join("plugins");
    let file = plugins.join("condense-thought-sig.js");
    if file.exists() {
        return Ok(PluginInstall::noop());
    }
    let made_opencode = !opencode.exists();
    let made_plugins = !plugins.exists();
    fs::create_dir_all(&plugins)?;
    fs::write(&file, THOUGHT_SIG_PLUGIN)?;
    let created_root = if made_opencode {
        Some(opencode)
    } else if made_plugins {
        Some(plugins)
    } else {
        None
    };
    Ok(PluginInstall {
        created_root,
        file: Some(file),
    })
}

/// Pull `-m/--model <provider>/<model>` out of the passthrough args. Splits on
/// the first `/` only; the model id itself may contain `/` (e.g. `openai/gpt-x`).
fn parse_model_arg(args: &[String]) -> Option<(String, String)> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let spec = match a.as_str() {
            "-m" | "--model" => it.next().map(String::as_str),
            _ => a.strip_prefix("--model=").or_else(|| a.strip_prefix("-m=")),
        };
        if let Some(spec) = spec {
            let (provider, model) = spec.split_once('/')?;
            return Some((provider.to_string(), model.to_string()));
        }
    }
    None
}

/// Two condense providers as an `OPENCODE_CONFIG_CONTENT` env payload. Only the
/// caller's `-m` model is declared — OpenCode rejects models absent from the map.
fn build_config(
    api_base_url: &str,
    headers: &[(String, String)],
    extra_model: Option<&(String, String)>,
) -> String {
    let base = api_base_url.trim_end_matches('/');
    let mut anthropic_models = Map::new();
    let mut openai_models = Map::new();
    if let Some((provider, model)) = extra_model {
        match provider.as_str() {
            "condense-anthropic" => anthropic_models.insert(model.clone(), json!({})),
            "condense-openai" => openai_models.insert(model.clone(), json!({})),
            _ => None,
        };
    }
    let provider = json!({
        "condense-anthropic": {
            "npm": "@ai-sdk/anthropic",
            "name": "Condense (Anthropic)",
            "options": provider_options(
                format!("{base}/anthropic/v1"),
                headers,
                std::env::var("ANTHROPIC_API_KEY").ok(),
            ),
            "models": Value::Object(anthropic_models),
        },
        "condense-openai": {
            "npm": "@ai-sdk/openai-compatible",
            "name": "Condense (OpenAI)",
            "options": provider_options(
                format!("{base}/openai/v1"),
                headers,
                std::env::var("OPENAI_API_KEY").ok(),
            ),
            "models": Value::Object(openai_models),
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
        let raw = build_config("https://api.example.com/", &headers, None);
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
    fn requested_model_is_the_only_one_declared() {
        let extra = (
            "condense-openai".to_string(),
            "openai/gpt-5.4-nano".to_string(),
        );
        let raw = build_config("https://x", &[], Some(&extra));
        let v: Value = serde_json::from_str(&raw).unwrap();
        let openai = &v["provider"]["condense-openai"]["models"];
        // exactly the requested model, nothing hardcoded
        assert!(openai["openai/gpt-5.4-nano"].is_object());
        assert_eq!(openai.as_object().unwrap().len(), 1);
        // not leaked onto the other provider, which stays empty
        assert!(v["provider"]["condense-anthropic"]["models"]["openai/gpt-5.4-nano"].is_null());
        assert!(
            v["provider"]["condense-anthropic"]["models"]
                .as_object()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn parse_model_arg_handles_forms_and_slashes() {
        let got = parse_model_arg(&[
            "run".into(),
            "-m".into(),
            "condense-openai/openai/gpt-5.4-nano".into(),
            "hi".into(),
        ]);
        assert_eq!(
            got,
            Some((
                "condense-openai".to_string(),
                "openai/gpt-5.4-nano".to_string()
            ))
        );
        let eqform = parse_model_arg(&["--model=condense-anthropic/claude-haiku-4-5".into()]);
        assert_eq!(
            eqform,
            Some((
                "condense-anthropic".to_string(),
                "claude-haiku-4-5".to_string()
            ))
        );
        assert_eq!(parse_model_arg(&["run".into(), "hi".into()]), None);
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
