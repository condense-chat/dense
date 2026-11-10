use serde_json::Value;

use super::{commas, human, str_of};
use crate::ui;

pub(super) fn summary(report: &Value, details: bool, width: usize) -> String {
    let mut out = ui::Report::new("Account & savings", width);
    let totals = &report["totals"];
    out.section("Lifetime savings");
    if totals.is_object() {
        savings(&mut out, totals);
        tokens(
            &mut out,
            totals["pre_tokens"].as_i64(),
            totals["post_tokens"].as_i64(),
        );
        if let Some(n) = totals["request_count"].as_i64() {
            out.field("Requests", commas(n));
        }
        out.section("Lifetime token split");
        let input = totals["post_tokens"]
            .as_i64()
            .zip(totals["cache_read"].as_i64())
            .zip(totals["cache_write"].as_i64())
            .and_then(|((post, read), write)| post.checked_sub(read)?.checked_sub(write))
            .filter(|&n| n >= 0);
        breakdown(
            &mut out,
            totals["pre_tokens"].as_i64(),
            totals["post_tokens"].as_i64(),
            input,
            totals["cache_read"].as_i64(),
            totals["cache_write"].as_i64(),
            details,
        );
    } else {
        out.note("Lifetime usage is not available yet.");
    }

    let account = &report["account"];
    out.section("Account");
    out.field("Email", str_of(account, "email").unwrap_or("Unavailable"));
    if let Some(org) = str_of(account, "org") {
        out.field("Workspace", org);
    }
    if let Some(tier) = str_of(account, "tier") {
        let tier = tier.to_uppercase();
        out.field(
            "Plan",
            match str_of(account, "status").filter(|s| !s.is_empty()) {
                Some(status) => format!("{tier} ({status})"),
                None => tier,
            },
        );
    } else {
        out.field("Plan", "Unavailable");
    }
    out.field(
        "Compression",
        match account["compressing"].as_bool() {
            Some(true) => "On",
            Some(false) => "Off",
            None => "Unknown",
        },
    );
    out.field(
        "Data retention",
        match account["zdr"].as_bool() {
            Some(true) => "Zero data retention enabled",
            Some(false) => "Zero data retention off",
            None => "Unknown",
        },
    );
    if let Some(balance) = number(&account["credit_balance"]) {
        out.field("Credit balance", money(balance));
    }
    if details && let Some(id) = str_of(account, "user_id") {
        out.field("Account ID", id);
    }
    if report["session_id"].is_string() {
        session(&mut out, report, details);
    }
    if !details {
        out.section("More detail");
        if report["session_id"].is_string() {
            out.note("Exact counts and IDs: add `--details` to this command.");
        } else {
            out.note("Exact counts and IDs: `dense info --details`");
        }
    }
    out.finish()
}

fn breakdown(
    out: &mut ui::Report,
    before: Option<i64>,
    after: Option<i64>,
    input: Option<i64>,
    read: Option<i64>,
    write: Option<i64>,
    details: bool,
) {
    let total = before
        .zip(after)
        .map(|(a, b)| a.max(b))
        .or(before)
        .or(after);
    let saved = before
        .zip(after)
        .and_then(|(a, b)| a.checked_sub(b))
        .map(|n| n.max(0));
    out.note(if before.is_some() && before >= after {
        "Each bar shows a share of the input before compression."
    } else {
        "Each bar shows a share of the input sent to the model."
    });
    let counts = [write, input, read, saved];
    let values: Vec<String> = counts
        .iter()
        .map(|n| match n {
            Some(n) => {
                if details {
                    commas(*n)
                } else {
                    human(*n)
                }
            }
            None => "Unavailable".into(),
        })
        .collect();
    let count_width = values.iter().map(String::len).max().unwrap_or(0);
    let rows: Vec<ui::Bar<'_>> = [
        ("Cache writes", ui::orange as fn(&str) -> String),
        ("New input", ui::blue),
        ("Cache reads", ui::cyan),
        ("Saved", ui::green),
    ]
    .into_iter()
    .zip(counts)
    .zip(values)
    .map(|(((label, paint), count), value)| {
        let fraction = count.zip(total).map(|(n, total)| {
            if total > 0 {
                n as f64 / total as f64
            } else {
                0.0
            }
        });
        let value = match fraction {
            Some(f) => format!("{value:>count_width$}  {:>5.1}%", f * 100.0),
            None => value,
        };
        ui::Bar {
            fraction,
            label,
            paint,
            value,
        }
    })
    .collect();
    out.bars(&rows);
}

fn money(n: f64) -> String {
    let fixed = format!("{:.2}", n.abs());
    let (whole, cents) = fixed.split_once('.').unwrap_or((&fixed, "00"));
    let mut grouped = String::new();
    for (i, ch) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    format!("{}${grouped}.{cents}", if n < 0.0 { "-" } else { "" })
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|n| n.is_finite())
}

fn savings(out: &mut ui::Report, spend: &Value) {
    let before = number(&spend["pre_usd"]);
    let after = number(&spend["sent_usd"]);
    let saved = number(&spend["saved_usd"])
        .or_else(|| number(&spend["money_saved"]))
        .or_else(|| before.zip(after).map(|(a, b)| a - b));
    match saved {
        Some(n) if n >= 0.0 => out.field(
            "Cost saved",
            ui::green(&format!("{} (estimated model cost)", money(n))),
        ),
        Some(n) => out.field(
            "Added cost",
            ui::yellow(&format!("{} (estimated model cost)", money(-n))),
        ),
        None => out.field("Cost saved", "Unavailable"),
    }
    match (before, after) {
        (Some(a), Some(b)) => out.field(
            "Model cost",
            format!("{} before -> {} after", money(a), money(b)),
        ),
        (_, Some(b)) => out.field("Model cost", format!("{} after compression", money(b))),
        _ => {}
    }
}

fn session(out: &mut ui::Report, report: &Value, details: bool) {
    out.section("Session");
    if details {
        out.field(
            "Session ID",
            report["session_id"].as_str().unwrap_or("Unknown"),
        );
    }
    if let Some(error) = str_of(report, "session_error") {
        out.note(&format!("Could not load this session: {error}"));
        return;
    }
    let data = &report["session"];
    if !data.is_object() {
        out.note("Session details are not available yet.");
        return;
    }
    let meta = &data["session"];
    if meta.is_object() {
        out.field(
            "State",
            if meta["ended_at"].is_null() {
                "Active"
            } else {
                "Ended"
            },
        );
        if let Some(branch) = str_of(meta, "branch") {
            out.field("Branch", branch);
        }
        if details && let Some(cwd) = str_of(meta, "cwd") {
            out.field("Folder", cwd);
        }
    }
    if let Some(requests) = data["requests"].as_i64() {
        out.field("Requests", commas(requests));
        if let Some(settled) = data["settled_requests"].as_i64().filter(|&n| n < requests) {
            out.field(
                "Pricing",
                format!(
                    "{} of {} requests priced; totals are partial",
                    commas(settled),
                    commas(requests)
                ),
            );
            out.note("Pricing pending. Check again in a few minutes.");
        }
    }
    let spend = &data["spend"];
    savings(out, spend);
    tokens(
        out,
        spend.pointer("/pre/total").and_then(Value::as_i64),
        spend.pointer("/sent/total").and_then(Value::as_i64),
    );
    if let Some(n) = spend["output_tokens"].as_i64() {
        out.field("Output tokens", human(n));
    }
    out.section("Session token split");
    breakdown(
        out,
        spend.pointer("/pre/total").and_then(Value::as_i64),
        spend.pointer("/sent/total").and_then(Value::as_i64),
        spend.pointer("/sent/input").and_then(Value::as_i64),
        spend.pointer("/sent/cache_read").and_then(Value::as_i64),
        spend.pointer("/sent/cache_write").and_then(Value::as_i64),
        details,
    );
    out.section("Latest request context");
    let context = &data["context"];
    if context.is_object() {
        if let Some(model) = str_of(context, "model") {
            out.field("Model", model);
        }
        tokens(
            out,
            context.pointer("/pre/total").and_then(Value::as_i64),
            context.pointer("/sent/total").and_then(Value::as_i64),
        );
        breakdown(
            out,
            context.pointer("/pre/total").and_then(Value::as_i64),
            context.pointer("/sent/total").and_then(Value::as_i64),
            context.pointer("/sent/input").and_then(Value::as_i64),
            context.pointer("/sent/cache_read").and_then(Value::as_i64),
            context.pointer("/sent/cache_write").and_then(Value::as_i64),
            details,
        );
    } else {
        out.note("No completed request yet.");
    }
}

fn tokens(out: &mut ui::Report, before: Option<i64>, after: Option<i64>) {
    let value = match (before, after) {
        (Some(a), Some(b)) => {
            let reduction = if a > 0 {
                let pct = ((a as f64 - b as f64).abs() / a as f64 * 100.0).round();
                if a == b {
                    " (unchanged)".into()
                } else {
                    format!(" ({pct:.0}% {})", if b < a { "fewer" } else { "more" })
                }
            } else {
                String::new()
            };
            format!("{} before -> {} after{reduction}", human(a), human(b))
        }
        (_, Some(b)) => format!("{} after compression", human(b)),
        _ => "Unavailable".into(),
    };
    out.field("Input tokens", value);
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn fixture() -> Value {
        json!({
            "account": {"email": "a@b.c", "tier": "t50", "status": "active", "compressing": true, "zdr": true},
            "totals": {"request_count": 22945, "pre_tokens": 9500000000_i64, "post_tokens": 3300000000_i64,
                "cache_read": 3200000000_i64, "cache_write": 73000000,
                "pre_usd": "2919.84", "sent_usd": "1494.37", "saved_usd": "1425.47"}
        })
    }

    #[test]
    fn default_is_readable_savings_without_the_grid() {
        let out = summary(&fixture(), false, 80);
        for value in [
            "$1,425.47",
            "$2,919.84 before -> $1,494.37 after",
            "65% fewer",
            "22,945",
            "T50 (active)",
            "Zero data retention enabled",
        ] {
            assert!(out.contains(value), "{out}");
        }
        assert!(!out.contains("🌕"));
        for label in ["Cache reads", "Cache writes", "New input", "Saved"] {
            assert!(out.contains(label), "{out}");
        }
        assert!(out.contains('░'), "{out}");
        assert!(
            out.contains('▏'),
            "small categories should stay visible: {out}"
        );
        assert!(summary(&fixture(), true, 80).contains("3,200,000,000"));
    }

    #[test]
    fn missing_data_is_not_presented_as_zero_savings() {
        let out = summary(&json!({"totals": {"request_count": 2}}), false, 80);
        assert!(out.contains("Unavailable"));
        assert!(!out.contains("$0.00"));
        assert!(!out.contains("0%"));
        assert!(!out.contains("Zero data retention off"));
        let out = summary(&json!({}), false, 80);
        assert!(out.contains("Lifetime usage is not available yet"));
        for value in [json!("NaN"), json!("infinity"), json!(null), json!("n/a")] {
            assert_eq!(number(&value), None);
        }
    }

    #[test]
    fn increased_cost_and_tokens_do_not_claim_savings() {
        let mut report = fixture();
        report["totals"]["saved_usd"] = json!("-2.50");
        report["totals"]["post_tokens"] = json!(10450000000_i64);
        let out = summary(&report, false, 80);
        assert!(out.contains("Added cost"));
        assert!(out.contains("$2.50"));
        assert!(out.contains("10% more"));
        assert!(!out.contains("Cost saved"));
    }

    #[test]
    fn pending_and_failed_sessions_keep_the_account_summary() {
        let mut report = fixture();
        report["session_id"] = json!("session-123");
        report["session"] =
            json!({"requests": 3, "settled_requests": 1, "spend": {"saved_usd": "0.63"}});
        let out = summary(&report, false, 80);
        assert!(out.contains("1 of 3 requests priced; totals are partial"));
        assert!(out.contains("Pricing pending"));
        assert!(out.contains("$0.63"));
        assert!(out.contains("No completed request yet"));
        assert!(!out.contains("session-123"));
        assert!(summary(&report, true, 80).contains("session-123"));
        report["session_error"] = json!("Session not found");
        let out = summary(&report, false, 80);
        assert!(out.contains("Could not load this session: Session not found"));
        assert!(out.contains("$1,425.47"));
        assert!(!out.contains("$0.63"));
    }

    #[test]
    fn session_and_context_keep_their_own_token_splits() {
        let mut report = fixture();
        report["session_id"] = json!("session-123");
        report["session"] = json!({
            "requests": 3, "settled_requests": 3,
            "spend": {"pre": {"total": 30000}, "sent": {"input": 3000, "cache_read": 6000, "cache_write": 3000, "total": 12000}},
            "context": {"model": "test-model", "pre": {"total": 1000}, "sent": {"input": 100, "cache_read": 200, "cache_write": 200, "total": 500}}
        });
        let out = summary(&report, false, 80);
        let (_, session) = out
            .split_once("Session token split")
            .expect("session chart");
        let (session, context) = session
            .split_once("Latest request context")
            .expect("context chart");
        for (part, read, saved) in [(session, "6,000", "60.0%"), (context, "200", "50.0%")] {
            assert!(part.contains("Cache reads"));
            assert!(part.contains("Cache writes"));
            assert!(part.contains("New input"));
            assert!(part.contains(read), "{part}");
            assert!(part.contains(saved), "{part}");
            assert!(part.contains('█'), "{part}");
        }
        assert!(!out.contains("Pricing pending"));
    }

    #[test]
    fn compact_summary_fits_narrow_terminals() {
        for width in [32, 40, 50, 80, 120] {
            for details in [false, true] {
                let out = summary(&fixture(), details, width);
                assert!(
                    out.lines()
                        .all(|line| console::measure_text_width(line) <= width),
                    "width {width}: {out}"
                );
            }
        }
    }
}
