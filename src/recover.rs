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
            println!("{}", render(&text));
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

/// Text payload of a wire block: Anthropic `text`/`content`, OpenAI chat
/// `content`, Responses `output` — string or text-part array.
fn block_text(b: &Value) -> Option<String> {
    for key in ["text", "content", "output"] {
        match b.get(key) {
            Some(Value::String(s)) => return Some(s.clone()),
            Some(Value::Array(parts)) => {
                let texts: Vec<&str> = parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect();
                if !texts.is_empty() {
                    return Some(texts.join("\n"));
                }
            }
            _ => {}
        }
    }
    None
}

fn detail(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("detail")?.as_str().map(String::from))
        .unwrap_or_else(|| body.trim().to_string())
}

/// Agent-facing render: a header line, the pin sentinel when present, then
/// each block's content directly — not the JSON envelope.
fn render(body: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return body.trim_end().to_string();
    };
    let kind = v.get("kind").and_then(Value::as_str).unwrap_or("?");
    let id = v.get("id").and_then(Value::as_str).unwrap_or("?");
    let mut out = format!("recovered {kind} {id}");
    if let Some(pin) = v.get("pin").and_then(Value::as_str) {
        out.push_str(&format!("\npin: {pin}"));
    }
    let blocks: Vec<&Value> = if let Some(b) = v.get("block") {
        vec![b]
    } else {
        v.get("blocks")
            .and_then(Value::as_array)
            .map(|a| a.iter().collect())
            .unwrap_or_default()
    };
    for b in blocks {
        out.push('\n');
        out.push_str(&render_block(b));
    }
    out
}

fn render_block(b: &Value) -> String {
    if let Some(name) = b.get("name").and_then(Value::as_str) {
        let args = b
            .get("input")
            .map(|i| i.to_string())
            .or_else(|| b.get("arguments").and_then(Value::as_str).map(String::from))
            .unwrap_or_default();
        return format!("call {name}({args})");
    }
    if let Some(calls) = b.get("tool_calls").and_then(Value::as_array) {
        let lines: Vec<String> = calls
            .iter()
            .map(|c| {
                let f = c.get("function");
                let name = f
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                let args = f
                    .and_then(|f| f.get("arguments"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                format!("call {name}({args})")
            })
            .collect();
        return lines.join("\n");
    }
    if let Some(text) = block_text(b) {
        return text;
    }
    serde_json::to_string_pretty(b).unwrap_or_else(|_| b.to_string())
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

    #[test]
    fn render_pack_lists_blocks_and_pin() {
        let body = r#"{
            "id": "ab12", "kind": "pack", "provider": "anthropic",
            "pin": "condense-critical:ab12",
            "blocks": [
                {"type": "tool_use", "name": "Bash", "input": {"command": "ls"}},
                {"type": "tool_result", "content": [{"type": "text", "text": "a.py\nb.py"}]}
            ]
        }"#;
        assert_eq!(
            render(body),
            "recovered pack ab12\npin: condense-critical:ab12\ncall Bash({\"command\":\"ls\"})\na.py\nb.py"
        );
    }

    #[test]
    fn render_single_block_content_directly() {
        let body = r#"{"id": "cd34", "kind": "tool_result", "provider": "openai",
                       "role": "tool", "block": {"role": "tool", "content": "file body"}}"#;
        assert_eq!(render(body), "recovered tool_result cd34\nfile body");
    }

    #[test]
    fn render_responses_function_call_uses_arguments() {
        let b = serde_json::json!({"type": "function_call", "name": "shell",
                                   "arguments": "{\"cmd\": \"ls\"}"});
        assert_eq!(render_block(&b), "call shell({\"cmd\": \"ls\"})");
        let o = serde_json::json!({"type": "function_call_output", "output": "done"});
        assert_eq!(render_block(&o), "done");
    }

    #[test]
    fn render_chat_tool_calls_message() {
        let b = serde_json::json!({"role": "assistant", "tool_calls": [
            {"function": {"name": "read", "arguments": "{}"}}
        ]});
        assert_eq!(render_block(&b), "call read({})");
    }

    #[test]
    fn render_falls_back_to_raw_on_non_json() {
        assert_eq!(render("not json\n"), "not json");
    }
}
