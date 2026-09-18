//! `dense usage`: how much of a subscription's limits condense saved.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::Result;
use crate::api::{Api, auth};
use crate::config::Config;
use crate::error::{Context, Error};
use crate::harness::claude;
use crate::info::{self, BAR};
use crate::ui;

const BAR_USED: &str = "█";
const BAR_FREE: &str = "░";
const BAR_OVER: &str = "▓";
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
        if w.resets_at.is_empty() {
            rows.push(row(&w, &Value::Null, &Value::Null));
            continue;
        }
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
        print!("{}", summary(&rows, terminal_width()));
    }
    Ok(())
}

fn bar(used: f64, scale: f64, width: usize) -> String {
    let base = ((100.0 / scale * width as f64).round() as usize).min(width);
    let spent = ((used.clamp(0.0, 100.0) / 100.0 * base as f64).round() as usize).min(base);
    let overflow = if scale > 100.0 {
        (((used - 100.0).max(0.0) / (scale - 100.0) * (width - base) as f64).round() as usize)
            .min(width - base)
    } else {
        0
    };
    format!(
        "{}{}{}{}",
        BAR_USED.repeat(spent),
        BAR_FREE.repeat(base - spent),
        if scale > 100.0 { "│" } else { "" },
        BAR_OVER.repeat(overflow),
    )
}

fn comparison(used: f64, without: f64, width: usize, indent: &str) -> String {
    let scale = without.max(used).max(100.0);
    let entries = [
        (
            "Regular Claude",
            without,
            format!("{without:.0}% of limit (estimated)"),
        ),
        ("With condense", used, format!("{used:.0}% of limit")),
    ];
    let available = width.saturating_sub(indent.len());
    let marker_width = usize::from(scale > 100.0);
    let cells = available
        .saturating_sub(marker_width + 2)
        .min((BAR as f64 * scale / 100.0).ceil() as usize);
    let mut out = String::new();
    for (index, (label, value, suffix)) in entries.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(&wrapped(&format!("{label:<14}  {suffix}"), width, indent));
        out.push('\n');
        if cells > 0 {
            out.push_str(&format!("{indent}[{}]\n", bar(*value, scale, cells)));
        }
    }
    out
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
        "resets_at": if w.resets_at.is_empty() { None } else { Some(&w.resets_at) },
        "requests": count("requests"),
        "inferred_requests": inferred_requests,
        "reconciled_requests": count("reconciled_requests"),
        "pre_usd": pre,
        "sent_usd": sent,
        "raw_usd": raw,
        "output_usd": output,
    })
}

fn summary(rows: &[Value], width: usize) -> String {
    if rows.is_empty() {
        return format!("{}\n", wrapped("No active Claude limits", width, ""));
    }
    let num = |r: &Value, k: &str| r.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    let text = |r: &Value, k: &str| r.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let indent = if width >= 4 { "  " } else { "" };
    let mut out = String::new();
    for r in rows {
        let title = text(r, "title")
            .replace("Current session", "Session")
            .replace("Current week", "Week")
            .replace(" (all models)", "")
            .replace(" only)", ")");
        if r.get("resets_at").is_some_and(Value::is_null) {
            out.push_str(&wrapped(
                &format!("{title} · inactive (0% used)"),
                width,
                "",
            ));
            out.push_str("\n\n");
            continue;
        }
        let reset = format!("resets {}", resets(&text(r, "resets_at")));
        if console::measure_text_width(&title) + console::measure_text_width(&reset) + 2 <= width {
            out.push_str(&format!("{}  {}\n", ui::bold(&title), ui::dim(&reset)));
        } else {
            out.push_str(&format!(
                "{}\n{}\n",
                ui::bold(&wrapped(&title, width, "")),
                ui::dim(&wrapped(&reset, width, indent)),
            ));
        }
        let used = num(r, "utilization");
        if let Some(without) = r.get("without").and_then(Value::as_f64) {
            let gain = (num(r, "usage_multiplier") - 1.0) * 100.0;
            let direction = if gain >= 0.0 { "more" } else { "less" };
            out.push_str(&ui::bold(&wrapped(
                &format!(
                    "Estimated benefit: {:.0}% {direction} usage on your plan",
                    gain.abs()
                ),
                width,
                indent,
            )));
            out.push_str("\n\n");
            out.push_str(&comparison(used, without, width, indent));
        } else {
            out.push_str(&wrapped(
                &format!("{used:.0}% used · estimate unavailable"),
                width,
                indent,
            ));
        }
        if !out.ends_with("\n") {
            out.push('\n');
        }
        out.push('\n');
    }
    if rows
        .iter()
        .any(|r| r.get("without").and_then(Value::as_f64).is_some())
    {
        out.push_str(&ui::dim(&wrapped(
            &format!("{BAR_USED} used · {BAR_FREE} left · │ 100% · {BAR_OVER} over limit"),
            width,
            "",
        )));
        out.push('\n');
    }
    out
}

fn terminal_width() -> usize {
    console::Term::stdout()
        .size_checked()
        .map(|(_, cols)| usize::from(cols))
        .filter(|&cols| cols > 0)
        .or_else(|| {
            std::env::var("COLUMNS")
                .ok()?
                .parse::<usize>()
                .ok()
                .filter(|&cols| cols > 0)
        })
        .unwrap_or(80)
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
            let utilization = w.get("utilization")?.as_f64()?;
            let resets_at = match w.get("resets_at")? {
                Value::String(at) => at.clone(),
                Value::Null if key == "five_hour" && utilization == 0.0 => String::new(),
                _ => return None,
            };
            Some(Window {
                key: key.into(),
                model: model.into(),
                resets_at,
                secs,
                title: title.into(),
                utilization,
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

fn wrapped(text: &str, width: usize, indent: &str) -> String {
    textwrap::fill(
        text,
        textwrap::Options::new(width.max(1))
            .initial_indent(indent)
            .subsequent_indent(indent),
    )
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
        let display = summary(&[r], 80);
        assert!(display.contains("Regular Claude"));
        assert!(display.contains("200% of limit (estimated)"));
        assert!(display.contains("Estimated benefit: 100% more usage on your plan"));

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
        let display = summary(&[r], 80);
        assert!(display.contains("100% used · estimate unavailable"));
        assert!(!display.contains("saved 0"));
        assert!(!display.contains("-0.00"));
    }

    #[test]
    fn summary_fits_narrow_and_wide_terminals() {
        let rows = [json!({
            "title": "Current week (Long model name 模型 only)",
            "utilization": 100.0,
            "without": 201.0,
            "usage_multiplier": 2.01,
            "resets_at": "2026-09-22T04:59:00+00:00",
        })];
        for width in [20, 32, 40, 60, 80, 120] {
            let display = summary(&rows, width);
            for line in display.lines() {
                assert!(
                    console::measure_text_width(line) <= width,
                    "{width}: {line}"
                );
            }
            let words = display.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(words.contains("Regular Claude"));
            assert!(words.contains("With condense"));
            assert!(words.contains("201% of limit (estimated)"));
            assert!(words.contains("100%"));
            assert!(words.contains("Estimated benefit: 101% more usage on your plan"));
            assert!(words.contains("resets 2026-09-22 04:59 UTC"));
        }
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
    fn inactive_session_remains_visible_without_an_estimate() {
        let plan = json!({"five_hour": {"utilization": 0, "resets_at": null}});
        let ws = windows(&plan);
        assert_eq!(ws.len(), 1);
        assert!(ws[0].resets_at.is_empty());
        let r = row(&ws[0], &Value::Null, &Value::Null);
        assert!(r["resets_at"].is_null());
        assert!(r["without"].is_null());
        assert_eq!(summary(&[r], 80), "Session · inactive (0% used)\n\n");
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
    fn bars_share_scale_and_extend_beyond_the_limit() {
        assert_eq!(
            bar(10.0, 100.0, BAR),
            format!("{}{}", BAR_USED.repeat(2), BAR_FREE.repeat(18))
        );
        assert_eq!(bar(100.0, 200.0, BAR), format!("{}│", BAR_USED.repeat(10)));
        assert_eq!(
            bar(200.0, 200.0, BAR),
            format!("{}│{}", BAR_USED.repeat(10), BAR_OVER.repeat(10))
        );
        assert_eq!(
            bar(50.0, 200.0, BAR),
            format!("{}{}│", BAR_USED.repeat(5), BAR_FREE.repeat(5))
        );
    }
}
