//! Codex through condense — `Codex<OpenAi>`.

use crate::Result;
use crate::api::dialect::OpenAi;
use crate::config::Config;
use crate::harness::{self, ProxyTarget, Tool};

const KEY_ENV_VAR: &str = "CONDENSE_CODEX_KEY";
const PROVIDER_ID: &str = "condense";

pub struct Codex;

impl Tool<OpenAi> for Codex {
    /// Codex has no single custom-headers env var, so the provider is defined
    /// inline via `-c` overrides (top of the precedence stack, untouched by the
    /// project-config security boundary). Secret header values ride in env vars
    /// referenced by `env_http_headers`, never in argv.
    fn apply(&self, cmd: &mut tokio::process::Command, target: &ProxyTarget) {
        // The dialect base is `<api>/openai`; Codex appends `/responses` to the
        // provider base_url, so the `/v1` lands us on `/openai/v1/responses`.
        let base_url = format!("{}/v1", target.base_url);

        let mut header_entries: Vec<String> = Vec::new();
        for (name, value) in &target.headers {
            let env_name = header_env_var(name);
            cmd.env(&env_name, value);
            header_entries.push(format!("{name:?} = {env_name:?}"));
        }
        let env_http_headers = format!("{{ {} }}", header_entries.join(", "));

        // Real auth rides in x-condense-auth-token; the bearer is a placeholder
        // the proxy replaces with the upstream key. requires_openai_auth=false
        // drops Codex's `sk-` prefix assumption for the placeholder.
        cmd.env(KEY_ENV_VAR, "condense");

        let overrides = [
            format!(r#"model_provider="{PROVIDER_ID}""#),
            format!(r#"model_providers.{PROVIDER_ID}.name="condense""#),
            format!(r#"model_providers.{PROVIDER_ID}.base_url="{base_url}""#),
            format!(r#"model_providers.{PROVIDER_ID}.wire_api="responses""#),
            format!("model_providers.{PROVIDER_ID}.requires_openai_auth=false"),
            format!(r#"model_providers.{PROVIDER_ID}.env_key="{KEY_ENV_VAR}""#),
            format!("model_providers.{PROVIDER_ID}.env_http_headers={env_http_headers}"),
        ];
        for o in overrides {
            cmd.arg("-c").arg(o);
        }
    }

    fn binary(&self) -> &str {
        "codex"
    }

    fn label(&self) -> &str {
        "Codex"
    }
}

/// `dense codex` — Codex through the OpenAI Responses proxy. The dialect is the
/// concrete `OpenAi`, so no proxy flag is threaded through the run path.
pub async fn run(cfg: &Config, args: &[String]) -> Result<()> {
    harness::launch(cfg, Codex, OpenAi, args).await
}

/// Per-header env var name holding the secret value; the `env_http_headers` map
/// references these names so secrets never land in argv.
fn header_env_var(name: &str) -> String {
    let suffix: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("CONDENSE_HDR_{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_env_var_sanitises_name() {
        assert_eq!(
            header_env_var("x-condense-auth-token"),
            "CONDENSE_HDR_X_CONDENSE_AUTH_TOKEN"
        );
    }

    #[test]
    fn apply_builds_provider_overrides_and_keeps_secrets_out_of_argv() {
        let target = ProxyTarget {
            base_url: "https://api.condense.chat/openai".to_string(),
            headers: vec![(
                "x-condense-auth-token".to_string(),
                "secret-token".to_string(),
            )],
        };
        let mut cmd = tokio::process::Command::new("codex");
        Codex.apply(&mut cmd, &target);

        let std_cmd = cmd.as_std();
        let args: Vec<String> = std_cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let argv = args.join(" ");

        // Codex appends /responses to the provider base_url, so /v1 lands on
        // /openai/v1/responses.
        assert!(argv.contains(
            r#"model_providers.condense.base_url="https://api.condense.chat/openai/v1""#
        ));
        assert!(argv.contains(r#"model_provider="condense""#));
        assert!(argv.contains(r#"model_providers.condense.wire_api="responses""#));
        assert!(argv.contains("model_providers.condense.requires_openai_auth=false"));
        // The header value rides in an env var referenced by env_http_headers —
        // never in argv.
        assert!(argv.contains("CONDENSE_HDR_X_CONDENSE_AUTH_TOKEN"));
        assert!(!argv.contains("secret-token"));

        let has_secret_env = std_cmd.get_envs().any(|(k, v)| {
            k == "CONDENSE_HDR_X_CONDENSE_AUTH_TOKEN" && v == Some("secret-token".as_ref())
        });
        assert!(has_secret_env);
    }
}
