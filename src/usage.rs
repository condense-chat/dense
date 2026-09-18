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

pub async fn run(cfg: &Config, sub: &str, json: bool, attributed_only: bool) -> Result<()> {
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
        let got = info::get(&api, usage_url(&cfg.api_base_url, &w, false)?.as_str()).await?;
        let inferred = if attributed_only {
            Value::Null
        } else {
            info::get(&api, usage_url(&cfg.api_base_url, &w, true)?.as_str()).await?
        };
        rows.push(row(&w, &got, &inferred));
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
    let whole = without.max(used).max(100.0);
    let bp = |p: f64| (p / whole * 10_000.0).round().clamp(0.0, 10_000.0) as i64;
    let saved = (without - used).max(0.0);
    let cells = info::allocate(&[bp(used), bp(saved)], 10_000, BAR);
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

/// One window's row: the `/v1/me/usage/models` body summed across models.
/// Output counts like raw in the ratio, since compression never touches it.
fn row(w: &Window, got: &Value, inferred: &Value) -> Value {
    let inferred_models = inferred
        .get("models")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let inferred_requests: u64 = inferred_models
        .iter()
        .filter_map(|m| m.get("requests")?.as_u64())
        .sum();
    let models: Vec<&Value> = got
        .get("models")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .chain(inferred_models)
        .collect();
    let count = |k: &str| {
        models
            .iter()
            .filter_map(|m| m.get(k)?.as_u64())
            .sum::<u64>()
    };
    let usd = |keys: &[&str]| {
        let sum: f64 = models
            .iter()
            .flat_map(|m| keys.iter().map(|k| m.get(*k).map_or(0.0, info::usd)))
            .sum();
        if sum == 0.0 {
            0.0
        } else {
            (sum * 1e6).round() / 1e6
        }
    };
    let (pre, sent, raw, output) = (
        usd(&["pre_usd"]),
        usd(&["sent_usd"]),
        usd(&["raw_usd"]),
        usd(&["output_usd"]),
    );
    let multiplier = (sent + raw + output > 0.0).then(|| without_pct(1.0, pre, sent, raw + output));
    let without = multiplier.map(|m| w.utilization * m);
    json!({
        "key": w.key,
        "title": w.title,
        "utilization": w.utilization,
        "without": without,
        "saved": without.map(|n| n - w.utilization),
        "usage_multiplier": multiplier,
        "resets_at": w.resets_at,
        "requests": count("requests"),
        "inferred_requests": inferred_requests,
        "reconciled_requests": count("reconciled_requests"),
        "pre_usd": pre,
        "sent_usd": sent,
        "raw_usd": raw,
        "output_usd": output,
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
        let used = num(r, "utilization");
        let Some(without) = r.get("without").and_then(Value::as_f64) else {
            out.push_str(&format!(
                "{}  {}\n  with condense {used:.0}% · savings estimate unavailable (no matching spend)\n\n",
                ui::bold(&text(r, "title")),
                ui::dim(&format!("resets {}", resets(&text(r, "resets_at")))),
            ));
            continue;
        };
        out.push_str(&format!(
            "{}  {}\n  {}  with condense {used:.0}% · estimated without {without:.0}% · saved {:.0} pts\n  estimated {:.2}× usage ({:.0}% more)\n  {}\n",
            ui::bold(&text(r, "title")),
            ui::dim(&format!("resets {}", resets(&text(r, "resets_at")))),
            bar(used, without),
            without - used,
            num(r, "usage_multiplier"),
            (num(r, "usage_multiplier") - 1.0) * 100.0,
            ui::dim(&format!(
                "reconciled {}/{} requests · pre ${} → sent ${} · raw ${} · output ${}",
                num(r, "reconciled_requests"),
                num(r, "requests"),
                info::dollars(r.get("pre_usd")),
                info::dollars(r.get("sent_usd")),
                info::dollars(r.get("raw_usd")),
                info::dollars(r.get("output_usd")),
            )),
        ));
        if num(r, "inferred_requests") > 0.0 {
            out.push_str(&format!(
                "  includes {} untagged Claude-harness requests; may include API-key traffic\n",
                num(r, "inferred_requests"),
            ));
        }
        out.push('\n');
    }
    out.push_str(&ui::dim(
        "note: traffic on this login outside dense is scaled by the same ratio.",
    ));
    out.push('\n');
    out
}

fn usage_url(base: &str, w: &Window, inferred: bool) -> Result<reqwest::Url> {
    let secs = w.secs.to_string();
    let mut params = vec![
        ("until", w.resets_at.as_str()),
        ("window", &secs),
        (
            "upstream_sub",
            if inferred { "none" } else { "claude_code" },
        ),
        ("provider", "anthropic"),
    ];
    if inferred {
        params.extend([("kind", "claude_code"), ("kind", "compact_condense")]);
    }
    if !w.model.is_empty() {
        params.push(("model", &w.model));
    }
    reqwest::Url::parse_with_params(
        &format!("{}/v1/me/usage/models", base.trim_end_matches('/')),
        params,
    )
    .ctx("building the usage URL")
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

/// Plan % the same traffic would have used uncompressed: the reconciled part
/// scales by pre/sent cost, the rest (raw, plus all output) counts as-is.
pub(crate) fn without_pct(n: f64, pre: f64, sent: f64, raw: f64) -> f64 {
    let d = sent + raw;
    if d <= 0.0 { n } else { n * (pre + raw) / d }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_pct_scales_by_pre_over_post() {
        assert!((without_pct(40.0, 3.0, 1.0, 1.0) - 80.0).abs() < 1e-9);
    }

    #[test]
    fn without_pct_is_identity_with_nothing_reconciled() {
        assert!((without_pct(40.0, 0.0, 0.0, 5.0) - 40.0).abs() < 1e-9);
        assert!((without_pct(40.0, 0.0, 0.0, 0.0) - 40.0).abs() < 1e-9);
    }

    #[test]
    fn row_sums_models_and_counts_output_like_raw() {
        let w = Window {
            key: "seven_day".into(),
            model: String::new(),
            resets_at: "2026-09-21T00:00:00+00:00".into(),
            secs: WEEK_SECS,
            title: "Current week (all models)".into(),
            utilization: 40.0,
        };
        let got = serde_json::json!({"since": "a", "until": "b", "models": [
            {"model": "claude-opus-4-6", "provider": "anthropic",
             "requests": 120, "reconciled_requests": 110,
             "pre_usd": "3.10", "sent_usd": "1.20", "saved_usd": "1.90",
             "raw_usd": "0.40", "output_usd": "2.00"},
            {"model": "claude-sonnet-4-6", "provider": "anthropic",
             "requests": 5, "reconciled_requests": 0,
             "pre_usd": "0", "sent_usd": "0", "saved_usd": "0",
             "raw_usd": "0.10", "output_usd": "0.30"}
        ]});
        let r = row(&w, &got, &Value::Null);
        assert_eq!(r["requests"], 125);
        assert_eq!(r["reconciled_requests"], 110);
        assert_eq!(r["pre_usd"], 3.1);
        assert_eq!(r["sent_usd"], 1.2);
        assert_eq!(r["raw_usd"], 0.5);
        assert_eq!(r["output_usd"], 2.3);
        // 40 × (3.10 + 2.80) / (1.20 + 2.80)
        assert!((r["without"].as_f64().unwrap() - 59.0).abs() < 1e-9);
    }

    #[test]
    fn historical_rows_contribute_to_weekly_estimate() {
        let w = weekly_window();
        let inferred = json!({"models": [{
            "model": "claude-fable-5-1", "provider": "anthropic",
            "requests": 2, "reconciled_requests": 2,
            "pre_usd": "3", "sent_usd": "1", "raw_usd": "0", "output_usd": "1"
        }]});
        let r = row(&w, &json!({"models": []}), &inferred);
        assert_eq!(r["without"], 200.0);
        assert_eq!(r["usage_multiplier"], 2.0);
        assert_eq!(r["inferred_requests"], 2);
        let display = summary(&[r]);
        assert!(display.contains("with condense 100% · estimated without 200%"));
        assert!(display.contains("2.00× usage (100% more)"));
        assert!(display.contains("includes 2 untagged"));

        let attributed = json!({"models": [{
            "model": "claude-fable-5-1", "provider": "anthropic",
            "requests": 1, "reconciled_requests": 1,
            "pre_usd": "1", "sent_usd": "1", "raw_usd": "0", "output_usd": "0"
        }]});
        let combined = row(&w, &attributed, &inferred);
        assert_eq!(combined["requests"], 3);
        assert_eq!(combined["inferred_requests"], 2);
        assert_eq!(combined["pre_usd"], 4.0);
        assert_eq!(combined["sent_usd"], 2.0);
        assert_eq!(combined["output_usd"], 1.0);
        assert!((combined["without"].as_f64().unwrap() - 100.0 * 5.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn empty_usage_does_not_claim_zero_savings() {
        let r = row(&weekly_window(), &json!({"models": []}), &Value::Null);
        assert!(r["without"].is_null());
        assert!(r["saved"].is_null());
        assert!(r["usage_multiplier"].is_null());
        let display = summary(&[r]);
        assert!(display.contains("with condense 100% · savings estimate unavailable"));
        assert!(!display.contains("saved 0"));
        assert!(!display.contains("-0.00"));
    }

    #[test]
    fn usage_queries_keep_attributed_and_inferred_rows_disjoint() {
        let mut w = weekly_window();
        w.model = "fable".into();
        for inferred in [false, true] {
            let url = usage_url("https://api.example/", &w, inferred).unwrap();
            let pairs: Vec<_> = url.query_pairs().collect();
            let values = |key| {
                pairs
                    .iter()
                    .filter(|(k, _)| k == key)
                    .map(|(_, v)| v.as_ref())
                    .collect::<Vec<_>>()
            };
            assert_eq!(values("until"), [w.resets_at.as_str()]);
            assert_eq!(values("window"), ["604800"]);
            assert_eq!(values("provider"), ["anthropic"]);
            assert_eq!(values("model"), ["fable"]);
            if inferred {
                assert_eq!(values("upstream_sub"), ["none"]);
                assert_eq!(values("kind"), ["claude_code", "compact_condense"]);
            } else {
                assert_eq!(values("upstream_sub"), ["claude_code"]);
                assert!(values("kind").is_empty());
            }
        }
    }

    fn weekly_window() -> Window {
        Window {
            key: "seven_day".into(),
            model: String::new(),
            resets_at: "2026-09-22T04:59:00+00:00".into(),
            secs: WEEK_SECS,
            title: "Current week (all models)".into(),
            utilization: 100.0,
        }
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
            format!("{}{}", MOON_SPENT.repeat(12), MOON_WOULD.repeat(8))
        );
        assert_eq!(
            bar(100.0, 200.0),
            format!("{}{}", MOON_SPENT.repeat(10), MOON_WOULD.repeat(10))
        );
    }
}
