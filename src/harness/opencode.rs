use std::fs;
use std::path::PathBuf;

use serde_json::{Map, Value, json};

use crate::Result;
use crate::config::Config;
use crate::harness::{self, DialectTarget, MultiTool};

const THOUGHT_SIG_PLUGIN: &str = include_str!("../../assets/opencode/condense-thought-sig.js");

struct OpenCode {
    model: Option<(String, String)>,
}

struct PluginInstall {
    created_root: Option<PathBuf>,
    file: Option<PathBuf>,
}

impl MultiTool for OpenCode {
    fn apply(
        &self,
        cmd: &mut tokio::process::Command,
        targets: &[DialectTarget],
    ) -> Box<dyn FnOnce() + Send> {
        note_active_providers();
        cmd.env(
            "OPENCODE_CONFIG_CONTENT",
            build_config(targets, self.model.as_ref()),
        );
        let plugin = install_plugin().unwrap_or_else(|e| {
            eprintln!("  warning: could not install thought_signature plugin: {e}");
            PluginInstall::noop()
        });
        Box::new(move || plugin.cleanup())
    }

    fn binary(&self) -> &str {
        "opencode"
    }

    fn label(&self) -> &str {
        "OpenCode"
    }
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

/// `dense opencode` — OpenCode routed through condense (Anthropic + OpenAI in
/// one config). Multi-provider, so it rides [`harness::launch_multi`] rather
/// than the single-dialect `Tool` path.
pub async fn run(cfg: &Config, args: &[String]) -> Result<()> {
    let tool = OpenCode {
        model: parse_model_arg(args),
    };
    harness::launch_multi(cfg, tool, args).await
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

/// Two condense providers as an `OPENCODE_CONFIG_CONTENT` env payload, one per
/// dialect target — provider id and route come from [`DialectTarget`], not
/// hardcoded here. Only the caller's `-m` model is declared; OpenCode rejects
/// models absent from the map.
fn build_config(targets: &[DialectTarget], model: Option<&(String, String)>) -> String {
    let mut providers = Map::new();
    for dt in targets {
        let Some((id, npm, name, key_env)) = provider_meta(dt.route) else {
            continue;
        };
        let mut models = Map::new();
        if let Some((provider, requested)) = model {
            if provider == id {
                models.insert(requested.clone(), json!({}));
            }
        }
        let entry = json!({
            "npm": npm,
            "name": name,
            "options": provider_options(
                format!("{}/v1", dt.target.base_url.trim_end_matches('/')),
                &dt.target.headers,
                std::env::var(key_env).ok(),
            ),
            "models": Value::Object(models),
        });
        providers.insert(id.to_string(), entry);
    }
    json!({ "provider": Value::Object(providers) }).to_string()
}

/// A condense dialect route → its OpenCode provider metadata:
/// (provider id, npm package, display name, upstream-key env var).
fn provider_meta(route: &str) -> Option<(&'static str, &'static str, &'static str, &'static str)> {
    match route {
        "anthropic" => Some((
            "condense-anthropic",
            "@ai-sdk/anthropic",
            "Condense (Anthropic)",
            "ANTHROPIC_API_KEY",
        )),
        "openai" => Some((
            "condense-openai",
            "@ai-sdk/openai-compatible",
            "Condense (OpenAI)",
            "OPENAI_API_KEY",
        )),
        _ => None,
    }
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
    use crate::harness::ProxyTarget;

    fn targets(base: &str, headers: &[(String, String)]) -> Vec<DialectTarget> {
        ["anthropic", "openai"]
            .into_iter()
            .map(|route| DialectTarget {
                route,
                target: ProxyTarget {
                    base_url: format!("{base}/{route}"),
                    headers: headers.to_vec(),
                },
            })
            .collect()
    }

    #[test]
    fn config_has_both_providers_and_routes() {
        let headers = vec![("x-condense-session-id".to_string(), "s".to_string())];
        let raw = build_config(&targets("https://api.example.com", &headers), None);
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
        let raw = build_config(&targets("https://x", &[]), Some(&extra));
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
