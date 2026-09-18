//! `dense usage`: how much of a subscription's limits condense saved.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::Result;
use crate::api::{Api, auth};
use crate::config::Config;
use crate::error::{Context, Error};
use crate::harness::claude;
use crate::info::{self, BAR, MOON_SAVED, MOON_SPENT};
use crate::ui;

/// Headroom the plan would have spent without condense.
const MOON_WOULD: &str = "🌓";
const OAUTH_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// The endpoint answers an empty 200 to any other agent.
const CC_USER_AGENT: &str = "claude-cli/2.1.276 (external, cli)";
const SESSION_SECS: u64 = 18_000;
const WEEK_SECS: u64 = 604_800;
/// `(key, title, window, model filter)` for the fixed windows of the usage body.
const WINDOWS: [(&str, &str, u64, &str); 4] = [
    ("five_hour", "Current session", SESSION_SECS, ""),
    ("seven_day", "Current week (all models)", WEEK_SECS, ""),
    (
        "seven_day_sonnet",
        "Current week (Sonnet only)",
        WEEK_SECS,
        "sonnet",
    ),
    (
        "seven_day_opus",
        "Current week (Opus only)",
        WEEK_SECS,
        "opus",
    ),
];

struct Window {
    key: String,
    model: String,
    resets_at: String,
    secs: u64,
    title: String,
    utilization: f64,
}

pub async fn run(cfg: &Config, sub: &str, json: bool) -> Result<()> {
    if sub != "claude" {
        return Err(Error::msg(format!(
            "`dense usage {sub}` is not supported yet"
        )));
    }
    let oauth = claude::read_oauth(cfg.home())
        .ok_or_else(|| Error::msg("no claude.ai login found — run `claude` and log in"))?;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
    if oauth.expires_at <= now_ms {
        return Err(Error::msg(
            "claude login expired — run `claude` once to refresh it (dense never refreshes it)",
        ));
    }
    let creds = auth::load_creds(cfg);
    if !creds.is_authenticated() {
        return Err(Error::Auth("not logged in — run `dense login`".into()));
    }
    let api = Api::authed(cfg, &creds)?;
    let plan = fetch_plan(&oauth.access_token).await?;
    let mut rows = Vec::new();
    for w in windows(&plan) {
        let url = reqwest::Url::parse_with_params(
            &format!("{}/v1/me/usage/sub", cfg.api_base_url.trim_end_matches('/')),
            [
                ("sub", "claude_code"),
                ("until", &w.resets_at),
                ("window", &w.secs.to_string()),
                ("model", &w.model),
            ],
        )
        .ctx("building the usage URL")?;
        rows.push(row(&w, &info::get(&api, url.as_str()).await?));
    }
    if json {
        let out = json!({"sub": sub, "windows": rows});
        println!(
            "{}",
            serde_json::to_string_pretty(&out).ctx("rendering usage JSON")?
        );
    } else {
        print!("{}", summary(&rows));
    }
    Ok(())
}

fn bar(used: f64, without: f64) -> String {
    let bp = |p: f64| (p * 100.0).round().clamp(0.0, 10_000.0) as i64;
    let used = bp(used);
    let saved = bp(without).saturating_sub(used).max(0);
    let cells = info::allocate(&[used, saved], 10_000, BAR);
    let n = |i: usize| cells.get(i).map_or(0, |c| c.0);
    format!(
        "{}{}{}",
        MOON_SPENT.repeat(n(0)),
        MOON_WOULD.repeat(n(1)),
        MOON_SAVED.repeat(BAR.saturating_sub(n(0) + n(1)))
    )
}

async fn fetch_plan(token: &str) -> Result<Value> {
    let resp = reqwest::Client::new()
        .get(OAUTH_USAGE_URL)
        .bearer_auth(token)
        .header("anthropic-beta", "oauth-2025-04-20")
        .header(reqwest::header::USER_AGENT, CC_USER_AGENT)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .ctx("fetching Claude plan usage")?;
    let status = resp.status();
    if matches!(status.as_u16(), 401 | 403) {
        return Err(Error::Auth(
            "claude.ai rejected the login — run `claude` once to refresh it".into(),
        ));
    }
    if !status.is_success() {
        return Err(Error::msg(format!("Claude plan usage failed: {status}")));
    }
    let body = resp.bytes().await.ctx("reading Claude plan usage")?;
    serde_json::from_slice::<Value>(&body)
        .ok()
        .filter(Value::is_object)
        .ok_or_else(|| Error::msg("Claude plan usage came back empty"))
}

fn resets(at: &str) -> String {
    match (at.get(..16), at.ends_with("+00:00")) {
        (Some(t), true) => format!("{} UTC", t.replace('T', " ")),
        _ => at.to_string(),
    }
}

fn row(w: &Window, got: &Value) -> Value {
    let field = |k: &str| got.get(k).cloned().unwrap_or(Value::Null);
    let usd = |k: &str| got.get(k).map_or(0.0, info::usd);
    let without = without_pct(
        w.utilization,
        usd("attributed_pre_usd"),
        usd("attributed_post_usd"),
        usd("raw_usd"),
    );
    json!({
        "key": w.key,
        "title": w.title,
        "utilization": w.utilization,
        "without": without,
        "saved": without - w.utilization,
        "resets_at": w.resets_at,
        "requests": field("requests"),
        "attributed": field("attributed"),
        "attributed_pre_usd": field("attributed_pre_usd"),
        "attributed_post_usd": field("attributed_post_usd"),
        "raw_usd": field("raw_usd"),
    })
}

fn summary(rows: &[Value]) -> String {
    if rows.is_empty() {
        return "no active plan limits on this claude.ai login\n".into();
    }
    let num = |r: &Value, k: &str| r.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    let text = |r: &Value, k: &str| r.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let mut out = String::new();
    for r in rows {
        let (used, without) = (num(r, "utilization"), num(r, "without"));
        out.push_str(&format!(
            "{}  {}\n  {}  with condense {used:.0}% · without {without:.0}% · saved {:.0} pts\n  {}\n\n",
            ui::bold(&text(r, "title")),
            ui::dim(&format!("resets {}", resets(&text(r, "resets_at")))),
            bar(used, without),
            without - used,
            ui::dim(&format!(
                "attributed {}/{} requests · pre ${} → post ${} · raw ${}",
                num(r, "attributed"),
                num(r, "requests"),
                info::dollars(r.get("attributed_pre_usd")),
                info::dollars(r.get("attributed_post_usd")),
                info::dollars(r.get("raw_usd")),
            )),
        ));
    }
    out.push_str(&ui::dim(
        "note: traffic on this login outside dense is scaled by the same ratio.",
    ));
    out.push('\n');
    out
}

fn windows(plan: &Value) -> Vec<Window> {
    let mut out: Vec<Window> = WINDOWS
        .iter()
        .filter_map(|&(key, title, secs, model)| {
            let w = plan.get(key)?;
            Some(Window {
                key: key.into(),
                model: model.into(),
                resets_at: w.get("resets_at")?.as_str()?.into(),
                secs,
                title: title.into(),
                utilization: w.get("utilization")?.as_f64()?,
            })
        })
        .collect();
    // Unscoped `limits[]` entries repeat the fixed windows; the model-scoped ones are new.
    for l in plan
        .get("limits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(name) = l
            .pointer("/scope/model/display_name")
            .and_then(Value::as_str)
        else {
            continue;
        };
        let session = l
            .get("kind")
            .and_then(Value::as_str)
            .is_some_and(|k| k.starts_with("session"));
        let (prefix, span, secs) = if session {
            ("five_hour", "session", SESSION_SECS)
        } else {
            ("seven_day", "week", WEEK_SECS)
        };
        let model = name.to_lowercase();
        let key = format!("{prefix}_{model}");
        let (Some(utilization), Some(resets_at)) = (
            l.get("percent").and_then(Value::as_f64),
            l.get("resets_at").and_then(Value::as_str),
        ) else {
            continue;
        };
        if out.iter().any(|w| w.key == key) {
            continue;
        }
        out.push(Window {
            key,
            model,
            resets_at: resets_at.into(),
            secs,
            title: format!("Current {span} ({name} only)"),
            utilization,
        });
    }
    out
}

/// Plan % the same traffic would have used uncompressed: the attributed part
/// scales by pre/post cost, the unattributed (raw) part counts as-is.
pub(crate) fn without_pct(n: f64, pre_attr: f64, post_attr: f64, raw: f64) -> f64 {
    let d = post_attr + raw;
    if d <= 0.0 {
        n
    } else {
        n * (pre_attr + raw) / d
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_pct_scales_by_pre_over_post() {
        assert!((without_pct(40.0, 3.0, 1.0, 1.0) - 80.0).abs() < 1e-9);
    }

    #[test]
    fn without_pct_is_identity_with_nothing_attributed() {
        assert!((without_pct(40.0, 0.0, 0.0, 5.0) - 40.0).abs() < 1e-9);
        assert!((without_pct(40.0, 0.0, 0.0, 0.0) - 40.0).abs() < 1e-9);
    }

    #[test]
    fn windows_skip_nulls_and_unscoped_limits() {
        let body = serde_json::json!({
            "five_hour": {"utilization": 10.0, "resets_at": "2026-09-18T12:30:00.238559+00:00"},
            "seven_day": {"utilization": 42.5, "resets_at": "2026-09-21T00:00:00.1+00:00"},
            "seven_day_sonnet": {"utilization": 5.0, "resets_at": "2026-09-21T00:00:00+00:00"},
            "seven_day_opus": null,
            "seven_day_breakdown": {"x": 1},
            "spend": null,
            "limits": [
                {"kind": "session", "percent": 10, "resets_at": "a", "scope": null},
                {"kind": "weekly_scoped", "percent": 5, "resets_at": "b",
                 "scope": {"model": {"display_name": "Sonnet"}}},
                {"kind": "weekly_scoped", "group": "weekly", "percent": 96,
                 "resets_at": "2026-09-21T00:00:00+00:00",
                 "scope": {"model": {"id": null, "display_name": "Fable"}, "surface": null},
                 "is_active": true}
            ]
        });
        let ws = windows(&body);
        let got: Vec<(&str, f64, u64, &str)> = ws
            .iter()
            .map(|w| {
                (
                    w.resets_at.as_str(),
                    w.utilization,
                    w.secs,
                    w.model.as_str(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("2026-09-18T12:30:00.238559+00:00", 10.0, 18000, ""),
                ("2026-09-21T00:00:00.1+00:00", 42.5, 604_800, ""),
                ("2026-09-21T00:00:00+00:00", 5.0, 604_800, "sonnet"),
                ("2026-09-21T00:00:00+00:00", 96.0, 604_800, "fable"),
            ]
        );
    }

    #[test]
    fn bar_splits_used_saved_and_free() {
        assert_eq!(
            bar(10.0, 25.0),
            format!(
                "{}{}{}",
                MOON_SPENT.repeat(2),
                MOON_WOULD.repeat(3),
                MOON_SAVED.repeat(15)
            )
        );
        assert_eq!(
            bar(90.0, 150.0),
            format!("{}{}", MOON_SPENT.repeat(18), MOON_WOULD.repeat(2))
        );
    }
}
