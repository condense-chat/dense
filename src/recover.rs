//! `dense recover` — resolve a c:/r:/pack id from a condensed summary back
//! to the original tool call/result via the api's `/v1/recovery`.

use serde_json::Value;

use crate::Result;
use crate::api::{Api, auth};
use crate::config::Config;
use crate::error::{Context, Error};

pub async fn run(cfg: &Config, id: &str, critical: bool) -> Result<()> {
    let creds = auth::load_creds(cfg);
    let api = Api::authed(cfg, &creds)?;
    let mut body = serde_json::json!({ "hash": id });
    if critical {
        body["critical"] = Value::Bool(true);
    }
    let resp = api.post_response("/v1/recovery", &body).await?;
    let status = resp.status().as_u16();
    let text = resp.text().await.ctx("reading /v1/recovery response")?;
    match status {
        200..=299 => {
            println!("{}", pretty(&text));
            Ok(())
        }
        401 | 403 => Err(Error::Auth(format!(
            "recovery rejected ({status}) — run `dense login` and retry"
        ))),
        _ => Err(Error::msg(format!(
            "recovery failed ({status}): {}",
            detail(&text)
        ))),
    }
}

fn detail(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("detail")?.as_str().map(String::from))
        .unwrap_or_else(|| body.trim().to_string())
}

fn pretty(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .and_then(|v| serde_json::to_string_pretty(&v))
        .unwrap_or_else(|_| body.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detail_prefers_fastapi_field() {
        assert_eq!(
            detail(r#"{"detail": "unknown recovery id"}"#),
            "unknown recovery id"
        );
        assert_eq!(detail("plain text\n"), "plain text");
    }
}
