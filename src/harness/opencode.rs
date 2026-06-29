use std::fs;

use serde_json::{Map, Value, json};

use crate::Result;
use crate::api::Api;
use crate::api::auth;
use crate::api::dialect::{AllDialects, DialectRoute, MultiDialect};
use crate::api::session::Session;
use crate::config::Config;
use crate::{harness, tool};

const THOUGHT_SIG_PLUGIN: &str = include_str!("../../assets/opencode/condense-thought-sig.js");

/// `dense opencode` — OpenCode routed through condense. Declares one provider
/// per dialect condense speaks (Anthropic + OpenAI) in one config, then runs
/// the shared lifecycle.
pub async fn run(cfg: &Config, args: &[String]) -> Result<()> {
    if let Err(e) = ensure_plugin(cfg) {
        eprintln!("  warning: could not install thought_signature plugin: {e}");
    }
    let creds = auth::ensure_auth(cfg).await?;
    let api = Api::authed(cfg, &creds)?;
    let session = Session::new();
    let bin = tool::resolve_real(cfg, "opencode")?;

    if args.is_empty() {
        harness::announce(cfg, "OpenCode");
    }
    note_active_providers();

    let headers = harness::condense_headers(cfg, &creds, &session.id);
    let mut cmd = tokio::process::Command::new(&bin);
    cmd.env(
        "OPENCODE_CONFIG_CONTENT",
        build_config(
            &AllDialects.dialects(cfg),
            &headers,
            parse_model_arg(args).as_ref(),
        ),
    );
    cmd.args(args);

    harness::spawn_and_wait(&api, &session, &bin, cmd).await
}

/// Write the replay plugin into OpenCode's global plugin dir, idempotently.
fn ensure_plugin(cfg: &Config) -> Result<()> {
    let config_root = cfg
        .config_dir()
        .parent()
        .ok_or_else(|| crate::error::Error::msg("cannot locate config root"))?;
    let plugins = config_root.join("opencode").join("plugins");
    let file = plugins.join("condense-thought-sig.js");
    if fs::read_to_string(&file).ok().as_deref() == Some(THOUGHT_SIG_PLUGIN) {
        return Ok(());
    }
    fs::create_dir_all(&plugins)?;
    fs::write(&file, THOUGHT_SIG_PLUGIN)?;
    Ok(())
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
/// dialect condense speaks. Only the caller's `-m` model is declared; OpenCode
/// rejects models absent from the map.
fn build_config(
    dialects: &[DialectRoute],
    headers: &[(String, String)],
    model: Option<&(String, String)>,
) -> String {
    let mut providers = Map::new();
    for dr in dialects {
        add_provider(&mut providers, dr, headers, model);
    }
    json!({ "provider": Value::Object(providers) }).to_string()
}

/// Add one dialect's OpenCode provider entry, keyed by its route.
fn add_provider(
    providers: &mut Map<String, Value>,
    dialect: &DialectRoute,
    headers: &[(String, String)],
    model: Option<&(String, String)>,
) {
    let Some((id, npm, name, key_env)) = provider_meta(dialect.route) else {
        return;
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
            format!("{}/v1", dialect.base_url.trim_end_matches('/')),
            headers,
            std::env::var(key_env).ok(),
        ),
        "models": Value::Object(models),
    });
    providers.insert(id.to_string(), entry);
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

    fn dialects(base: &str) -> Vec<DialectRoute> {
        ["anthropic", "openai"]
            .into_iter()
            .map(|route| DialectRoute {
                route,
                base_url: format!("{base}/{route}"),
            })
            .collect()
    }

    #[test]
    fn config_has_both_providers_and_routes() {
        let headers = vec![("x-condense-session-id".to_string(), "s".to_string())];
        let raw = build_config(&dialects("https://api.example.com"), &headers, None);
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
        let raw = build_config(&dialects("https://x"), &[], Some(&extra));
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
