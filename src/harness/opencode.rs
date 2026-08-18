use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::Result;
use crate::api::dialect::Dialect;
use crate::config::Config;
use crate::harness::{self, Target, Tool};

const ANTHROPIC_NPM: &[&str] = &["@ai-sdk/anthropic"];
/// What OpenCode assumes for a provider that names no npm package.
const DEFAULT_NPM: &str = "@ai-sdk/openai-compatible";
const OPENAI_NPM: &[&str] = &[
    "@ai-sdk/openai",
    "@ai-sdk/openai-compatible",
    "@openrouter/ai-sdk-provider",
];
const THOUGHT_SIG_PLUGIN: &str = include_str!("../../assets/opencode/condense-thought-sig.js");
const UPSTREAM_HEADER: &str = "x-condense-upstream-url";

pub struct OpenCode {
    catalog: Vec<Provider>,
    plugin: Option<PathBuf>,
}

/// One provider OpenCode has credentials for, rewritten for condense: its id,
/// the condense dialect route its SDK speaks, and the upstream base condense
/// should forward to.
struct Provider {
    id: String,
    route: &'static str,
    /// `None` for the two providers that already are condense's own default
    /// upstream — sending the override header for those would trip its
    /// credit-gated capability check for nothing.
    upstream: Option<String>,
}

impl Tool for OpenCode {
    fn dialects(&self) -> &'static [Dialect] {
        &[Dialect::Anthropic, Dialect::OpenAi]
    }

    /// OpenCode takes provider config via `OPENCODE_CONFIG_CONTENT`, which
    /// merges last and so wins over its own config files.
    fn apply(&self, cmd: &mut tokio::process::Command, targets: &[Target]) {
        cmd.env(
            "OPENCODE_CONFIG_CONTENT",
            build_config(targets, &self.catalog, self.plugin.as_deref()),
        );
    }

    fn binary(&self) -> &str {
        "opencode"
    }

    fn label(&self) -> &str {
        "OpenCode"
    }
}

/// `dense opencode` — every OpenAI- or Anthropic-dialect provider OpenCode is
/// already set up for, rerouted through condense with its real API base carried
/// in the upstream-url header. Everything we set rides the child's environment,
/// so an OpenCode launched any other way still goes straight to its vendor.
pub async fn run(cfg: &Config, args: &[String]) -> Result<()> {
    let plugin = match stage_plugin(cfg) {
        Ok(path) => Some(path),
        Err(e) => {
            eprintln!("  warning: could not stage thought_signature plugin: {e}");
            None
        }
    };
    let catalog = load_catalog();
    if catalog.is_empty() {
        eprintln!(
            "  no OpenAI- or Anthropic-dialect provider is set up in OpenCode — \
             nothing to route through condense"
        );
    } else {
        eprintln!("  routing {} providers through condense", catalog.len());
    }
    harness::launch(cfg, OpenCode { catalog, plugin }, args).await
}

/// The reroutable providers as an `OPENCODE_CONFIG_CONTENT` payload. Only
/// `options` is set — npm, name, models and the api key stay whatever OpenCode
/// already resolved, so the model picker looks exactly as it did.
fn build_config(dialects: &[Target], catalog: &[Provider], plugin: Option<&Path>) -> String {
    let mut providers = Map::new();
    for provider in catalog {
        let Some(target) = dialects.iter().find(|t| t.route == provider.route) else {
            continue;
        };
        providers.insert(
            provider.id.clone(),
            json!({
                "options": provider_options(
                    harness::with_v1(&target.base_url),
                    &target.headers,
                    provider.upstream.as_deref(),
                ),
            }),
        );
    }
    let mut cfg = Map::new();
    cfg.insert("provider".into(), Value::Object(providers));
    if let Some(plugin) = plugin {
        cfg.insert("plugin".into(), json!([plugin.to_string_lossy()]));
    }
    Value::Object(cfg).to_string()
}

/// Stage the replay plugin under dense's own data dir and hand back its path
/// for the config payload, so only the OpenCode we launch loads it. Earlier
/// releases dropped it in OpenCode's global plugin dir, where it ran on every
/// launch; clear that copy out.
fn stage_plugin(cfg: &Config) -> Result<PathBuf> {
    if let Some(root) = cfg.config_dir().parent() {
        let _ = fs::remove_file(
            root.join("opencode")
                .join("plugins")
                .join("condense-thought-sig.js"),
        );
    }
    let dir = cfg.data_dir().join("opencode");
    let file = dir.join("condense-thought-sig.js");
    if fs::read_to_string(&file).ok().as_deref() != Some(THOUGHT_SIG_PLUGIN) {
        fs::create_dir_all(&dir)?;
        fs::write(&file, THOUGHT_SIG_PLUGIN)?;
    }
    Ok(file)
}

/// Whether any of a provider's API-key env vars is set in our environment —
/// one of the two ways OpenCode decides a provider is usable.
fn env_key_set(vars: Option<&Value>) -> bool {
    vars.and_then(Value::as_array).is_some_and(|vars| {
        vars.iter()
            .filter_map(Value::as_str)
            .any(|v| std::env::var_os(v).is_some_and(|val| !val.is_empty()))
    })
}

fn field<'a>(entry: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    entry?.get(key)
}

// ponytail: reads OpenCode's own models.dev cache rather than fetching it — a
// cold cache means no reroute until OpenCode has run once. Fetch here if it bites.
fn load_catalog() -> Vec<Provider> {
    reroutable(
        &read_json(opencode_path("XDG_CACHE_HOME", ".cache", "models.json")),
        &user_providers(),
        &read_json(opencode_path("XDG_DATA_HOME", ".local/share", "auth.json")),
    )
}

fn opencode_path(xdg: &str, fallback: &str, file: &str) -> Option<PathBuf> {
    let base = match std::env::var(xdg) {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var("HOME").ok()?).join(fallback),
    };
    Some(base.join("opencode").join(file))
}

fn provider_options(
    base_url: String,
    headers: &[(String, String)],
    upstream: Option<&str>,
) -> Value {
    let mut header_map = Map::new();
    if let Some(upstream) = upstream {
        header_map.insert(
            UPSTREAM_HEADER.to_string(),
            Value::String(upstream.to_string()),
        );
    }
    // An explicit CONDENSE_UPSTREAM_URL rides in on the dialect headers and is a
    // deliberate send-everything-here override, so it wins over the per-provider one.
    for (name, value) in headers {
        header_map.insert(name.clone(), Value::String(value.clone()));
    }
    json!({ "baseURL": base_url, "headers": Value::Object(header_map) })
}

fn read_json(path: Option<PathBuf>) -> Map<String, Value> {
    match path.and_then(|p| fs::read_to_string(p).ok()) {
        Some(raw) => match serde_json::from_str(&raw) {
            Ok(Value::Object(map)) => map,
            _ => Map::new(),
        },
        None => Map::new(),
    }
}

/// Providers OpenCode already has credentials for, paired with the condense
/// route and upstream base each should use. Anything OpenCode would not have
/// enabled is left out — naming a provider in the config *enables* it, so
/// listing the whole catalog would flood the model picker.
fn reroutable(
    catalog: &Map<String, Value>,
    user: &Map<String, Value>,
    auths: &Map<String, Value>,
) -> Vec<Provider> {
    let ids: BTreeSet<&String> = catalog.keys().chain(user.keys()).collect();
    ids.into_iter()
        .filter_map(|id| {
            let entry = catalog.get(id);
            let mine = user.get(id);
            if mine.is_none() && !auths.contains_key(id) && !env_key_set(field(entry, "env")) {
                return None;
            }
            let npm = field(mine, "npm")
                .or_else(|| field(entry, "npm"))
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_NPM);
            // a hand-set baseURL is the user's real upstream; else models.dev's
            let api = field(mine, "options")
                .and_then(|o| o.get("baseURL"))
                .or_else(|| field(entry, "api"));
            let (route, upstream) = reroute(npm, id, api)?;
            Some(Provider {
                id: id.clone(),
                route,
                upstream,
            })
        })
        .collect()
}

/// A provider's npm package → the condense dialect route that speaks its wire
/// format, plus the upstream base condense forwards to. `None` for SDKs whose
/// wire format condense does not proxy, or whose base we cannot name.
fn reroute(npm: &str, id: &str, api: Option<&Value>) -> Option<(&'static str, Option<String>)> {
    let api = api.and_then(Value::as_str).filter(|s| !s.is_empty());
    if ANTHROPIC_NPM.contains(&npm) {
        // condense appends `/v1/messages`, models.dev states the SDK baseURL
        // (which already ends in `/v1`), so drop the overlap.
        let upstream = api.map(|base| base.strip_suffix("/v1").unwrap_or(base).to_string());
        return Some(("anthropic", upstream.filter(|_| id != "anthropic")));
    }
    if OPENAI_NPM.contains(&npm) {
        // condense appends `/chat/completions`, exactly what the SDK does, so
        // the base passes through unchanged.
        if id == "openai" {
            return Some(("openai", None));
        }
        return Some(("openai", Some(api?.to_string())));
    }
    None
}

/// `provider` entries from OpenCode's own config. A config with comments does
/// not parse here; that only costs the reroute for hand-declared providers.
fn user_providers() -> Map<String, Value> {
    let mut out = Map::new();
    for file in ["opencode.json", "opencode.jsonc"] {
        let cfg = read_json(opencode_path("XDG_CONFIG_HOME", ".config", file));
        if let Some(Value::Object(providers)) = cfg.get("provider") {
            out.extend(providers.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const CATALOG: &str = r#"{
      "anthropic":  {"npm": "@ai-sdk/anthropic", "api": null, "env": ["ANTHROPIC_API_KEY"]},
      "openai":     {"npm": "@ai-sdk/openai", "api": null, "env": ["OPENAI_API_KEY"]},
      "openrouter": {"npm": "@openrouter/ai-sdk-provider", "api": "https://openrouter.ai/api/v1", "env": ["OPENROUTER_API_KEY"]},
      "minimax":    {"npm": "@ai-sdk/anthropic", "api": "https://api.minimax.io/anthropic/v1", "env": ["MINIMAX_API_KEY"]},
      "groq":       {"npm": "@ai-sdk/groq", "api": "https://api.groq.com", "env": ["GROQ_API_KEY"]}
    }"#;

    fn map(raw: &str) -> Map<String, Value> {
        serde_json::from_str(raw).unwrap_or_default()
    }

    fn targets() -> Vec<Target> {
        vec![
            Target {
                route: "anthropic",
                base_url: "https://api.example.com/anthropic".into(),
                headers: vec![("x-condense-session-id".into(), "s".into())],
                upstream: None,
            },
            Target {
                route: "openai",
                base_url: "https://api.example.com/openai".into(),
                headers: vec![("x-condense-session-id".into(), "s".into())],
                upstream: None,
            },
        ]
    }

    #[test]
    fn anthropic_upstream_drops_the_v1_condense_re_adds() {
        // condense builds `<upstream>/v1/messages`
        let (route, upstream) = reroute(
            "@ai-sdk/anthropic",
            "minimax",
            Some(&json!("https://api.minimax.io/anthropic/v1")),
        )
        .unwrap_or_default();
        assert_eq!(route, "anthropic");
        assert_eq!(
            upstream.unwrap_or_default(),
            "https://api.minimax.io/anthropic"
        );

        // vendor-native: condense already defaults here, so no override header
        let (_, native) = reroute("@ai-sdk/anthropic", "anthropic", None).unwrap_or_default();
        assert!(native.is_none());
    }

    #[test]
    fn explicit_global_upstream_wins_over_the_per_provider_one() {
        let headers = vec![(UPSTREAM_HEADER.to_string(), "https://forced".to_string())];
        let opts = provider_options(
            "https://x/openai/v1".to_string(),
            &headers,
            Some("https://per"),
        );
        assert_eq!(opts["headers"][UPSTREAM_HEADER], "https://forced");
    }

    #[test]
    fn only_providers_opencode_can_already_use_are_rerouted() {
        let picked = reroutable(&map(CATALOG), &Map::new(), &map(r#"{"openrouter": {}}"#));
        let ids: Vec<&str> = picked.iter().map(|p| p.id.as_str()).collect();
        // groq is authless here anyway, but note it is also a dialect we skip
        assert_eq!(ids, ["openrouter"]);
    }

    #[test]
    fn openai_upstream_passes_through_and_unknown_bases_are_skipped() {
        // condense builds `<upstream>/chat/completions`, same as the SDK would
        let (route, upstream) = reroute(
            "@ai-sdk/openai-compatible",
            "deepseek",
            Some(&json!("https://api.deepseek.com")),
        )
        .unwrap_or_default();
        assert_eq!(route, "openai");
        assert_eq!(upstream.unwrap_or_default(), "https://api.deepseek.com");

        let (_, native) = reroute("@ai-sdk/openai", "openai", None).unwrap_or_default();
        assert!(native.is_none());

        // no api base and no known default → left direct rather than misrouted
        assert!(reroute("@ai-sdk/openai-compatible", "mystery", None).is_none());
        // dialect condense does not proxy
        assert!(reroute("@ai-sdk/groq", "groq", Some(&json!("https://x/v1"))).is_none());
    }

    /// Everything condense adds must live in the payload we hand our own child,
    /// so an OpenCode started any other way is untouched.
    #[test]
    fn the_plugin_rides_the_launch_payload_not_opencodes_plugin_dir() {
        let cfg: Value = serde_json::from_str(&build_config(
            &targets(),
            &[],
            Some(Path::new("/d/opencode/condense-thought-sig.js")),
        ))
        .unwrap_or_default();
        assert_eq!(cfg["plugin"][0], "/d/opencode/condense-thought-sig.js");

        let bare: Value =
            serde_json::from_str(&build_config(&targets(), &[], None)).unwrap_or_default();
        assert!(bare["plugin"].is_null());
    }

    #[test]
    fn rerouted_config_points_at_condense_and_names_the_real_upstream() {
        let picked = reroutable(
            &map(CATALOG),
            &Map::new(),
            &map(r#"{"openrouter": {}, "anthropic": {}, "groq": {}}"#),
        );
        let cfg: Value =
            serde_json::from_str(&build_config(&targets(), &picked, None)).unwrap_or_default();
        let p = &cfg["provider"];

        assert_eq!(
            p["openrouter"]["options"]["baseURL"],
            "https://api.example.com/openai/v1"
        );
        assert_eq!(
            p["openrouter"]["options"]["headers"][UPSTREAM_HEADER],
            "https://openrouter.ai/api/v1"
        );
        assert_eq!(
            p["openrouter"]["options"]["headers"]["x-condense-session-id"],
            "s"
        );
        // npm, name, models and the api key are left to OpenCode
        assert!(p["openrouter"]["models"].is_null());
        assert!(p["openrouter"]["options"]["apiKey"].is_null());

        // condense's own default upstream — no override header, so no needless
        // trip through its credit-gated capability check
        assert_eq!(
            p["anthropic"]["options"]["baseURL"],
            "https://api.example.com/anthropic/v1"
        );
        assert!(p["anthropic"]["options"]["headers"][UPSTREAM_HEADER].is_null());

        assert!(p["groq"].is_null());
    }

    #[test]
    fn a_hand_declared_provider_reroutes_to_its_own_base_url() {
        let user = map(r#"{"mycorp": {"npm": "@ai-sdk/openai-compatible",
                           "options": {"baseURL": "https://llm.mycorp.internal/v1"}}}"#);
        let picked = reroutable(&map(CATALOG), &user, &Map::new());
        let ids: Vec<&str> = picked.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["mycorp"]);
        assert_eq!(
            picked[0].upstream.as_deref(),
            Some("https://llm.mycorp.internal/v1")
        );
    }
}
