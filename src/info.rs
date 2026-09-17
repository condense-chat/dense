//! `dense info` — account, lifetime savings, and (given a session id) one
//! session as condense saw it.

use std::io::IsTerminal;

use serde_json::{Map, Value, json};

use crate::Result;
use crate::api::Api;
use crate::api::auth;
use crate::config::Config;
use crate::error::{Context, Error};
use crate::ui;

const BAR: usize = 20;
const COLS: usize = 20;
const ROWS: usize = 10;
const CELLS: usize = COLS * ROWS;
// 20 cells + 19 spaces, then the gap before the legend.
const LEGEND_INDENT: usize = COLS * 2 + 3;
const MOON_SAVED: &str = "🌕";
const MOON_SPENT: &str = "🌑";

type Paint = fn(&str) -> String;

/// `--matrix`: the /context-style glyph grid with colour (default on a
/// terminal). `--bar`: moon-phase emoji bars, which survive a slash command
/// echoing them as markdown (default when piped).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Bar,
    Matrix,
}

struct Tier {
    moon: &'static str,
    n: i64,
    name: &'static str,
    paint: Paint,
}

struct Split {
    cache_read: i64,
    cache_write: i64,
    input: i64,
}

impl Split {
    fn of(side: &Value) -> Self {
        Self {
            cache_read: int_of(side, "cache_read"),
            cache_write: int_of(side, "cache_write"),
            input: int_of(side, "input"),
        }
    }

    fn sent(&self) -> i64 {
        self.input + self.cache_read + self.cache_write
    }

    // Costliest tier first; the moon fills as the tier gets cheaper, so the
    // grid still reads where colour is stripped.
    fn tiers(&self) -> [Tier; 3] {
        [
            Tier {
                name: "Cache write",
                n: self.cache_write,
                paint: ui::orange,
                moon: "🌑",
            },
            Tier {
                name: "Input",
                n: self.input,
                paint: ui::blue,
                moon: "🌘",
            },
            Tier {
                name: "Cache read",
                n: self.cache_read,
                paint: ui::green,
                moon: "🌗",
            },
        ]
    }
}

pub async fn run(
    cfg: &Config,
    json: bool,
    layout: Option<Style>,
    session: Option<&str>,
) -> Result<()> {
    let creds = auth::load_creds(cfg);
    if !creds.is_authenticated() {
        return Err(Error::Auth("not logged in — run `dense login`".into()));
    }
    let api = Api::authed(cfg, &creds)?;
    let session_id = resolve_session(session, cfg.session_id());
    let (me, billing, session) = tokio::join!(
        get(&api, "/v1/me"),
        get(&api, "/v1/me/billing"),
        fetch_session(&api, session_id.as_deref()),
    );
    let report = report(&me?, &billing?, session_id.as_deref(), session);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).ctx("rendering info JSON")?
        );
    } else {
        let style = layout.unwrap_or(if std::io::stdout().is_terminal() {
            Style::Matrix
        } else {
            Style::Bar
        });
        print!("{}", summary(&report, style));
    }
    Ok(())
}

fn commas(n: i64) -> String {
    let digits = n.abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    if n < 0 { format!("-{out}") } else { out }
}

async fn detail_of(resp: reqwest::Response) -> Option<String> {
    let body: Value = resp.json().await.ok()?;
    body.get("detail")?.as_str().map(str::to_owned)
}

fn dollars(v: Option<&Value>) -> String {
    let raw = match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => "0".into(),
    };
    raw.parse::<f64>().map_or(raw, |f| format!("{f:.2}"))
}

async fn fetch_session(api: &Api, id: Option<&str>) -> Option<Result<Value>> {
    let id = id?;
    let path = format!("/v1/me/sessions/{id}");
    Some(match get(api, &path).await {
        Ok(v) => Ok(v),
        Err(Error::Message { msg, .. }) if msg.starts_with("400 ") => {
            Err(Error::msg(format!("invalid session id `{id}`")))
        }
        Err(Error::Message { msg, .. }) if msg.starts_with("404 ") => {
            Err(Error::msg(msg.trim_start_matches("404 ").to_owned()))
        }
        Err(e) => Err(e),
    })
}

/// Failures carry `<status> <detail-or-reason>` so callers can specialise.
async fn get(api: &Api, path: &str) -> Result<Value> {
    let resp = api.get_response(path).await?;
    let status = resp.status();
    if status.is_success() {
        return resp
            .json()
            .await
            .ctx(format!("{path} returned malformed JSON"));
    }
    if matches!(status.as_u16(), 401 | 403) {
        return Err(Error::Auth("not authorized — run `dense login`".into()));
    }
    let reason = detail_of(resp)
        .await
        .unwrap_or_else(|| format!("GET {path} failed: {status}"));
    Err(Error::msg(format!("{} {reason}", status.as_u16())))
}

fn human(n: i64) -> String {
    let f = n as f64;
    if n.abs() >= 1_000_000_000 {
        format!("{:.1}B", f / 1e9)
    } else if n.abs() >= 1_000_000 {
        format!("{:.1}M", f / 1e6)
    } else if n.abs() >= 10_000 {
        format!("{:.1}K", f / 1e3)
    } else {
        commas(n)
    }
}

fn int_of(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn report(
    me: &Value,
    billing: &Value,
    session_id: Option<&str>,
    session: Option<Result<Value>>,
) -> Value {
    let org = me
        .get("organizations")
        .and_then(Value::as_array)
        .and_then(|orgs| orgs.first())
        .and_then(|o| o.get("name"));
    let mut out = Map::new();
    out.insert(
        "account".into(),
        json!({
            "email": me.get("email"),
            "user_id": me.get("user_id"),
            "org": org,
            "tier": billing.get("tier"),
            "status": billing.get("status"),
            "credit_balance": me.get("credit_balance"),
            "compressing": billing.get("compressing"),
            "zdr": me.pointer("/toggles/zdr"),
        }),
    );
    out.insert(
        "totals".into(),
        me.get("usage").cloned().unwrap_or(Value::Null),
    );
    out.insert("session_id".into(), session_id.into());
    match session {
        Some(Ok(s)) => {
            out.insert("session".into(), s);
        }
        Some(Err(e)) => {
            out.insert("session".into(), Value::Null);
            out.insert("session_error".into(), e.to_string().into());
        }
        None => {
            out.insert("session".into(), Value::Null);
        }
    }
    Value::Object(out)
}

fn resolve_session(arg: Option<&str>, env: Option<&str>) -> Option<String> {
    [arg, env]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(str::to_owned)
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn summary(report: &Value, style: Style) -> String {
    let null = Value::Null;
    let account = report.get("account").unwrap_or(&null);
    let totals = report.get("totals").unwrap_or(&null);
    let mut out = String::new();

    let who = str_of(account, "email").unwrap_or("?");
    let org = str_of(account, "org")
        .map(|o| format!(" ({o})"))
        .unwrap_or_default();
    out.push_str(&format!("{} {who}{org}\n", ui::bold("dense —")));
    let mut plan = vec![];
    if let Some(tier) = str_of(account, "tier") {
        plan.push(match str_of(account, "status").filter(|s| !s.is_empty()) {
            Some(status) => format!("{tier} {status}"),
            None => tier.to_string(),
        });
    }
    match account.get("compressing").and_then(Value::as_bool) {
        Some(true) => plan.push("compressing on".into()),
        Some(false) => plan.push(ui::yellow("compressing off")),
        None => {}
    }
    if account.get("zdr").and_then(Value::as_bool) == Some(true) {
        plan.push("zdr".into());
    }
    if account.get("credit_balance").is_some_and(|c| !c.is_null()) {
        plan.push(format!(
            "credits ${}",
            dollars(account.get("credit_balance"))
        ));
    }
    if !plan.is_empty() {
        out.push_str(&format!("        {}\n", plan.join(" · ")));
    }

    let post = int_of(totals, "post_tokens");
    let read = int_of(totals, "cache_read");
    let write = int_of(totals, "cache_write");
    let split = Split {
        cache_read: read,
        cache_write: write,
        input: post - read - write,
    };
    let pre = int_of(totals, "pre_tokens");
    out.push('\n');
    let requests = format!("{} requests", commas(int_of(totals, "request_count")));
    match style {
        Style::Matrix => out.push_str(&grid(
            &split,
            pre,
            "Saved",
            &[
                ui::bold("Lifetime"),
                requests,
                tokens_line(pre, split.sent()),
                money_line(totals),
            ],
        )),
        Style::Bar => out.push_str(&moon_block(
            "Lifetime",
            &[requests],
            &split,
            pre,
            totals,
            "saved",
        )),
    }

    let Some(id) = str_of(report, "session_id") else {
        return out;
    };
    out.push_str(&format!("\n{} {}\n", ui::bold("session"), ui::dim(id)));
    if let Some(err) = str_of(report, "session_error") {
        out.push_str(&format!("        {}\n", ui::red(err)));
        return out;
    }
    let session = report.get("session").unwrap_or(&null);
    let mut head = vec![ui::bold("Session")];
    if let Some(meta) = session.get("session").filter(|s| s.is_object()) {
        let cwd = str_of(meta, "cwd").unwrap_or("?");
        let branch = str_of(meta, "branch")
            .map(|b| format!(" ({b})"))
            .unwrap_or_default();
        let state = if meta.get("ended_at").is_some_and(|e| !e.is_null()) {
            "ended"
        } else {
            "active"
        };
        head.push(format!("{cwd}{branch} — {state}"));
    }
    let requests = int_of(session, "requests");
    let settled = int_of(session, "settled_requests");
    let spend = session.get("spend").unwrap_or(&null);
    let mut counts = format!(
        "{requests} requests ({settled} settled) · {} output tokens",
        commas(int_of(spend, "output_tokens"))
    );
    if settled < requests {
        counts.push_str(&format!(
            " · {}",
            ui::yellow("pricing pending, re-run in a few minutes")
        ));
    }
    head.push(counts);
    let split = Split::of(spend.get("sent").unwrap_or(&null));
    let pre = int_of(spend.get("pre").unwrap_or(&null), "total");
    out.push('\n');
    match style {
        Style::Matrix => {
            head.push(tokens_line(pre, split.sent()));
            head.push(money_line(spend));
            out.push_str(&grid(&split, pre, "Saved", &head));
        }
        Style::Bar => {
            let mut head = head.into_iter();
            let title = head.next().unwrap_or_default();
            let rest: Vec<String> = head.collect();
            out.push_str(&moon_block(&title, &rest, &split, pre, spend, "saved"));
        }
    }

    out.push('\n');
    match session.get("context").filter(|c| c.is_object()) {
        Some(ctx) => {
            let split = Split::of(ctx.get("sent").unwrap_or(&null));
            let pre = int_of(ctx.get("pre").unwrap_or(&null), "total");
            let sent = split.sent();
            let model = str_of(ctx, "model").unwrap_or("?").to_string();
            let figures = format!(
                "{} of {} tokens · {}",
                human(sent),
                human(pre),
                ui::green(&format!("{}% smaller", pct(pre - sent, pre)))
            );
            match style {
                Style::Matrix => out.push_str(&grid(
                    &split,
                    pre,
                    "Optimized away",
                    &[ui::bold("Context"), model, figures],
                )),
                Style::Bar => {
                    out.push_str(&format!("{}   {model}\n", ui::bold("Context")));
                    out.push_str(&format!("  tokens  {}  {figures}\n", moon_bar(&split, pre)));
                    out.push_str(&format!(
                        "          {}\n",
                        moon_legend(&split, pre, "optimized away")
                    ));
                }
            }
        }
        None => out.push_str(&format!(
            "{}  {}\n",
            ui::bold("Context"),
            ui::dim("no settled request yet")
        )),
    }
    out
}

fn tokens_line(pre: i64, sent: i64) -> String {
    format!(
        "{} → {} tokens ({}% smaller)",
        human(pre),
        human(sent),
        pct(pre - sent, pre)
    )
}

fn money_line(money: &Value) -> String {
    let saved = money.get("saved_usd").or_else(|| money.get("money_saved"));
    match money.get("pre_usd") {
        Some(pre) => format!(
            "${} → ${} ({})",
            dollars(Some(pre)),
            dollars(money.get("sent_usd")),
            ui::green(&format!("saved ${}", dollars(saved)))
        ),
        None => ui::green(&format!("saved ${}", dollars(saved))),
    }
}

// A 20×10 grid like Claude Code's /context: the whole grid is what would
// have been sent (`whole`), 🌑/🌘/🌗 cells are cache write / input / cache read, 🌕 cells
// are what compression removed. A tier too small for half a cell shows dimmed.
fn grid(split: &Split, whole: i64, free_label: &str, head: &[String]) -> String {
    let tiers = split.tiers();
    let whole = whole.max(split.sent());
    let cells = allocate(&tiers.each_ref().map(|t| t.n), whole, CELLS);
    let mut glyphs: Vec<String> = Vec::with_capacity(CELLS);
    for (t, (n, partial)) in tiers.iter().zip(&cells) {
        for i in 0..*n {
            let g = (t.paint)(t.moon);
            glyphs.push(if *partial && i == 0 { ui::dim(&g) } else { g });
        }
    }
    let free = CELLS - glyphs.len();
    glyphs.extend(std::iter::repeat_n(ui::dim(MOON_SAVED), free));

    let mut legend: Vec<String> = head.to_vec();
    legend.push(String::new());
    legend.push(ui::bold("Sent by tier"));
    for t in &tiers {
        legend.push(format!(
            "{} {}: {} tokens ({})",
            (t.paint)(t.moon),
            t.name,
            human(t.n),
            pct_str(t.n, whole)
        ));
    }
    let saved = whole - split.sent();
    legend.push(format!(
        "{} {free_label}: {} tokens ({})",
        ui::dim(MOON_SAVED),
        human(saved),
        pct_str(saved, whole)
    ));

    let mut out = String::new();
    let rows = ROWS.max(legend.len());
    for r in 0..rows {
        if r < ROWS {
            out.push_str(&glyphs[r * COLS..(r + 1) * COLS].concat());
            out.push_str("   ");
        } else {
            out.push_str(&" ".repeat(LEGEND_INDENT));
        }
        if let Some(line) = legend.get(r) {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

// One block of the piped layout: a token bar split by tier, a spend bar
// split spent / saved, then a legend line.
fn moon_block(
    title: &str,
    head: &[String],
    split: &Split,
    pre: i64,
    money: &Value,
    free_label: &str,
) -> String {
    let mut out = String::new();
    let mut head = head.iter();
    out.push_str(&format!(
        "{}  {}\n",
        ui::bold(title),
        head.next().map(String::as_str).unwrap_or("")
    ));
    for line in head {
        out.push_str(&format!("          {line}\n"));
    }
    out.push_str(&format!(
        "  tokens  {}  {}\n",
        moon_bar(split, pre),
        tokens_line(pre, split.sent())
    ));
    if let Some(bar) = spend_bar(money) {
        out.push_str(&format!("  spend   {bar}  {}\n", money_line(money)));
    } else {
        out.push_str(&format!("  spend   {}\n", money_line(money)));
    }
    out.push_str(&format!(
        "          {}\n",
        moon_legend(split, pre, free_label)
    ));
    out
}

fn moon_bar(split: &Split, whole: i64) -> String {
    let tiers = split.tiers();
    let whole = whole.max(split.sent());
    let cells = allocate(&tiers.each_ref().map(|t| t.n), whole, BAR);
    let mut bar = String::new();
    let mut used = 0;
    for (t, (n, _)) in tiers.iter().zip(&cells) {
        bar.push_str(&t.moon.repeat(*n));
        used += n;
    }
    bar.push_str(&MOON_SAVED.repeat(BAR - used));
    bar
}

fn moon_legend(split: &Split, whole: i64, free_label: &str) -> String {
    let whole = whole.max(split.sent());
    let mut parts: Vec<String> = split
        .tiers()
        .iter()
        .map(|t| {
            format!(
                "{} {} {}",
                t.moon,
                t.name.to_lowercase(),
                pct_str(t.n, whole)
            )
        })
        .collect();
    parts.push(format!(
        "{MOON_SAVED} {free_label} {}",
        pct_str(whole - split.sent(), whole)
    ));
    parts.join(" · ")
}

// Spent vs saved dollars, or None on a legacy body with no pre/sent split.
fn spend_bar(money: &Value) -> Option<String> {
    let pre = usd(money.get("pre_usd")?);
    let sent = usd(money.get("sent_usd")?);
    let cents = |d: f64| (d * 100.0).round().max(0.0) as i64;
    let cells = allocate(&[cents(sent)], cents(pre), BAR);
    let spent = cells.first().map_or(0, |c| c.0).min(BAR);
    Some(format!(
        "{}{}",
        MOON_SPENT.repeat(spent),
        MOON_SAVED.repeat(BAR - spent)
    ))
}

fn usd(v: &Value) -> f64 {
    match v {
        Value::String(s) => s.parse().unwrap_or(0.0),
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        _ => 0.0,
    }
}

// Cells per part, largest remainder so they sum to the rounded total; a
// part with tokens but no cell borrows one and is flagged partial.
fn allocate(parts: &[i64], whole: i64, total: usize) -> Vec<(usize, bool)> {
    let n = parts.len();
    if whole <= 0 {
        return vec![(0, false); n];
    }
    let exact: Vec<f64> = parts
        .iter()
        .map(|p| (*p).max(0) as f64 / whole as f64 * total as f64)
        .collect();
    let mut cells: Vec<usize> = exact.iter().map(|e| e.floor() as usize).collect();
    let target = (exact.iter().sum::<f64>().round() as usize).min(total);
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|a, b| {
        (exact[*b] - exact[*b].floor()).total_cmp(&(exact[*a] - exact[*a].floor()))
    });
    for i in order.into_iter().cycle().take(n) {
        if cells.iter().sum::<usize>() >= target {
            break;
        }
        cells[i] += 1;
    }
    let mut partial = vec![false; n];
    for i in 0..n {
        if parts[i] > 0 && cells[i] == 0 {
            let used: usize = cells.iter().sum();
            if used >= total {
                let Some(big) = (0..n).filter(|j| cells[*j] > 1).max_by_key(|j| cells[*j]) else {
                    continue;
                };
                cells[big] -= 1;
            }
            cells[i] = 1;
            partial[i] = true;
        }
    }
    cells.into_iter().zip(partial).collect()
}

fn pct(part: i64, whole: i64) -> i64 {
    if whole <= 0 {
        return 0;
    }
    (part as f64 / whole as f64 * 100.0).round() as i64
}

// One decimal under 10% so small tiers don't read as 0%.
fn pct_str(part: i64, whole: i64) -> String {
    if whole <= 0 || part <= 0 {
        return "0%".into();
    }
    let p = part as f64 / whole as f64 * 100.0;
    if p < 10.0 {
        format!("{p:.1}%")
    } else {
        format!("{}%", p.round() as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary_grid(report: &Value) -> String {
        summary(report, Style::Matrix)
    }

    fn me() -> Value {
        json!({
            "email": "a@b.c",
            "user_id": "u1",
            "credit_balance": "73.1200266000",
            "toggles": {"zdr": false},
            "organizations": [{"name": "povilabs"}],
            "usage": {
                "request_count": 62992,
                "pre_tokens": 18401797122i64,
                "post_tokens": 6679319764i64,
                "tokens_saved": 11724339836i64,
                "money_saved": "6842.5286850500",
                "pre_usd": "9500.0000000000",
                "sent_usd": "2657.4713149500",
                "saved_usd": "6842.5286850500",
                "compression_pct": 64
            }
        })
    }

    fn billing() -> Value {
        json!({"tier": "t20", "status": "active", "compressing": true})
    }

    fn session(requests: i64, settled: i64, saved: &str) -> Value {
        json!({
            "session_id": "s",
            "session": {"cwd": "~/w", "branch": "main", "ended_at": null},
            "requests": requests,
            "settled_requests": settled,
            "context": null,
            "spend": {
                "pre": {"total": 30000},
                "sent": {"input": 500, "cache_read": 8000, "cache_write": 500, "total": 9000},
                "pre_usd": "0.90",
                "sent_usd": "0.27",
                "saved_usd": saved,
                "output_tokens": 4
            }
        })
    }

    #[test]
    fn commas_groups_thousands() {
        assert_eq!(commas(0), "0");
        assert_eq!(commas(999), "999");
        assert_eq!(commas(1000), "1,000");
        assert_eq!(commas(62992), "62,992");
        assert_eq!(commas(18401797122), "18,401,797,122");
        assert_eq!(commas(-1234), "-1,234");
    }

    #[test]
    fn human_scales_units() {
        assert_eq!(human(999), "999");
        assert_eq!(human(9999), "9,999");
        assert_eq!(human(10_000), "10.0K");
        assert_eq!(human(1_500_000), "1.5M");
        assert_eq!(human(18401797122), "18.4B");
    }

    #[test]
    fn dollars_rounds_to_cents() {
        assert_eq!(dollars(Some(&json!("6842.5286850500"))), "6842.53");
        assert_eq!(dollars(Some(&json!(0.5))), "0.50");
        assert_eq!(dollars(None), "0.00");
        assert_eq!(dollars(Some(&json!("n/a"))), "n/a");
    }

    #[test]
    fn report_shapes_account_and_totals() {
        let r = report(&me(), &billing(), None, None);
        let keys: Vec<&str> = r
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["account", "totals", "session_id", "session"]);
        let account = &r["account"];
        assert_eq!(account["email"], "a@b.c");
        assert_eq!(account["org"], "povilabs");
        assert_eq!(account["tier"], "t20");
        assert_eq!(account["status"], "active");
        assert_eq!(account["compressing"], true);
        assert_eq!(account["zdr"], false);
        assert_eq!(account["credit_balance"], "73.1200266000");
        assert_eq!(r["totals"]["request_count"], 62992);
        assert!(r["session_id"].is_null());
        assert!(r["session"].is_null());
        assert!(r.get("session_error").is_none());
    }

    #[test]
    fn report_tolerates_missing_org_and_usage() {
        let r = report(&json!({"email": "a@b.c"}), &json!({}), None, None);
        assert!(r["account"]["org"].is_null());
        assert!(r["account"]["tier"].is_null());
        assert!(r["totals"].is_null());
    }

    #[test]
    fn report_embeds_session_verbatim() {
        let r = report(
            &me(),
            &billing(),
            Some("s"),
            Some(Ok(session(3, 3, "0.63"))),
        );
        assert_eq!(r["session_id"], "s");
        assert_eq!(r["session"]["requests"], 3);
        assert!(r.get("session_error").is_none());
    }

    #[test]
    fn report_keeps_account_when_session_fails() {
        let r = report(
            &me(),
            &billing(),
            Some("s"),
            Some(Err(Error::msg("session `s` not found"))),
        );
        assert_eq!(r["account"]["email"], "a@b.c");
        assert_eq!(r["session_id"], "s");
        assert!(r["session"].is_null());
        assert_eq!(r["session_error"], "session `s` not found");
    }

    #[test]
    fn summary_general_only() {
        let out = summary_grid(&report(&me(), &billing(), None, None));
        assert!(out.contains("a@b.c (povilabs)"), "got: {out}");
        assert!(
            out.contains("t20 active · compressing on · credits $73.12"),
            "got: {out}"
        );
        assert!(out.contains("62,992 requests"), "got: {out}");
        assert!(
            out.contains("18.4B → 6.7B tokens (64% smaller)"),
            "got: {out}"
        );
        assert!(
            out.contains("$9500.00 → $2657.47 (saved $6842.53)"),
            "got: {out}"
        );
        assert!(out.contains("Sent by tier"), "got: {out}");
        assert!(out.contains("Saved: 11.7B tokens (64%)"), "got: {out}");
        assert!(!out.contains("session"), "got: {out}");
    }

    #[test]
    fn grid_is_ten_rows_of_twenty_cells() {
        let split = Split {
            input: 10,
            cache_read: 60,
            cache_write: 10,
        };
        let out = grid(&split, 100, "Saved", &["t".into()]);
        let rows: Vec<&str> = out.lines().collect();
        assert_eq!(rows.len(), 10, "got: {out}");
        let cells_of = |r: &str| r.split("   ").next().unwrap_or("").to_string();
        for row in &rows {
            let cells = cells_of(row)
                .chars()
                .filter(|c| "🌑🌘🌗🌕".contains(*c))
                .count();
            assert_eq!(cells, 20, "got: {row}");
        }
        let all: String = rows.iter().map(|r| cells_of(r)).collect();
        assert_eq!(all.matches('🌑').count(), 20, "got: {out}");
        assert_eq!(all.matches('🌘').count(), 20, "got: {out}");
        assert_eq!(all.matches('🌗').count(), 120, "got: {out}");
        assert_eq!(all.matches('🌕').count(), 40, "got: {out}");
        assert!(rows[0].ends_with("   t"), "got: {}", rows[0]);
    }

    #[test]
    fn allocate_uses_largest_remainder_and_marks_tiny_tiers() {
        assert_eq!(
            allocate(&[50, 50, 0], 100, CELLS),
            [(100, false), (100, false), (0, false)]
        );
        let a = allocate(&[1, 199, 0], 200, CELLS);
        assert_eq!(a, [(1, false), (199, false), (0, false)]);
        let b = allocate(&[1, 999_999, 0], 1_000_000, CELLS);
        assert_eq!(b, [(1, true), (199, false), (0, false)]);
        assert_eq!(allocate(&[1, 1], 0, CELLS), [(0, false), (0, false)]);
        assert_eq!(allocate(&[30], 100, BAR), [(6, false)]);
    }

    #[test]
    fn legend_overflow_indents_past_the_grid() {
        let split = Split {
            input: 1,
            cache_read: 1,
            cache_write: 1,
        };
        let head: Vec<String> = (0..8).map(|i| format!("h{i}")).collect();
        let out = grid(&split, 3, "Saved", &head);
        let rows: Vec<&str> = out.lines().collect();
        assert_eq!(rows.len(), 14, "got: {out}");
        assert!(
            rows[13].starts_with(&" ".repeat(LEGEND_INDENT)),
            "got: {}",
            rows[13]
        );
        assert!(
            rows[13].contains("Saved: 0 tokens (0%)"),
            "got: {}",
            rows[12]
        );
    }

    #[test]
    fn pct_str_keeps_a_decimal_under_ten() {
        assert_eq!(pct_str(4, 1000), "0.4%");
        assert_eq!(pct_str(94, 100), "94%");
        assert_eq!(pct_str(0, 100), "0%");
        assert_eq!(pct_str(5, 0), "0%");
    }

    #[test]
    fn summary_flags_compression_off_and_zdr() {
        let mut m = me();
        m["toggles"]["zdr"] = json!(true);
        m["credit_balance"] = Value::Null;
        let b = json!({"tier": "t20", "status": "active", "compressing": false});
        let out = summary_grid(&report(&m, &b, None, None));
        assert!(out.contains("compressing off · zdr"), "got: {out}");
        assert!(!out.contains("credits"), "got: {out}");
    }

    #[test]
    fn summary_appends_session_block() {
        let out = summary_grid(&report(
            &me(),
            &billing(),
            Some("s"),
            Some(Ok(session(3, 3, "0.63"))),
        ));
        assert!(out.contains("session s\n"), "got: {out}");
        assert!(out.contains("~/w (main) — active"), "got: {out}");
        assert!(
            out.contains("3 requests (3 settled) · 4 output tokens"),
            "got: {out}"
        );
        assert!(
            out.contains("30.0K → 9,000 tokens (70% smaller)"),
            "got: {out}"
        );
        assert!(out.contains("$0.90 → $0.27 (saved $0.63)"), "got: {out}");
        assert!(out.contains("no settled request yet"), "got: {out}");
        assert!(!out.contains("pricing pending"), "got: {out}");
    }

    #[test]
    fn summary_renders_context_grid() {
        let mut sess = session(3, 3, "0.63");
        sess["context"] = json!({
            "model": "claude-opus-5",
            "pre": {"input": 0, "cache_read": 200894, "cache_write": 1822, "total": 202716},
            "sent": {"input": 4, "cache_read": 53202, "cache_write": 17625, "total": 70831},
        });
        let out = summary_grid(&report(&me(), &billing(), Some("s"), Some(Ok(sess))));
        assert!(out.contains("Context"), "got: {out}");
        assert!(out.contains("claude-opus-5"), "got: {out}");
        assert!(
            out.contains("70.8K of 202.7K tokens · 65% smaller"),
            "got: {out}"
        );
        assert!(
            out.contains("Optimized away: 131.9K tokens (65%)"),
            "got: {out}"
        );
        assert!(out.contains("Cache read: 53.2K tokens (26%)"), "got: {out}");
        assert!(out.contains("Input: 4 tokens (0.0%)"), "got: {out}");
    }

    #[test]
    fn summary_flags_pending_settlement() {
        let out = summary_grid(&report(
            &me(),
            &billing(),
            Some("s"),
            Some(Ok(session(5, 2, "0.63"))),
        ));
        assert!(out.contains("5 requests (2 settled)"), "got: {out}");
        assert!(out.contains("pricing pending"), "got: {out}");
    }

    #[test]
    fn summary_shows_session_error_under_general_info() {
        let out = summary_grid(&report(
            &me(),
            &billing(),
            Some("s"),
            Some(Err(Error::msg("session `s` not found"))),
        ));
        assert!(out.contains("$6842.53"), "got: {out}");
        assert!(out.contains("session s\n"), "got: {out}");
        assert!(out.contains("session `s` not found"), "got: {out}");
        assert!(!out.contains("Context"), "got: {out}");
    }

    #[test]
    fn resolve_session_prefers_arg() {
        assert_eq!(
            resolve_session(Some("arg-id"), Some("env-id")).as_deref(),
            Some("arg-id")
        );
    }

    #[test]
    fn resolve_session_falls_back_to_env() {
        assert_eq!(
            resolve_session(None, Some("env-id")).as_deref(),
            Some("env-id")
        );
    }

    #[test]
    fn resolve_session_blank_arg_falls_through_to_env() {
        assert_eq!(
            resolve_session(Some("   "), Some("env-id")).as_deref(),
            Some("env-id")
        );
        assert_eq!(
            resolve_session(Some(""), Some(" env-id ")).as_deref(),
            Some("env-id")
        );
    }

    #[test]
    fn resolve_session_trims_arg() {
        assert_eq!(
            resolve_session(Some("  arg-id \n"), None).as_deref(),
            Some("arg-id")
        );
    }

    #[test]
    fn resolve_session_missing_both_is_none() {
        assert!(resolve_session(None, None).is_none());
        assert!(resolve_session(Some(""), Some("  ")).is_none());
    }

    #[test]
    fn moon_bar_is_twenty_cells_costliest_first() {
        let split = Split {
            input: 10,
            cache_read: 60,
            cache_write: 10,
        };
        let bar = moon_bar(&split, 100);
        assert_eq!(bar.chars().count(), 20, "got: {bar}");
        assert_eq!(bar, "🌑🌑🌘🌘🌗🌗🌗🌗🌗🌗🌗🌗🌗🌗🌗🌗🌕🌕🌕🌕");
        let legend = moon_legend(&split, 100, "saved");
        assert_eq!(
            legend,
            "🌑 cache write 10% · 🌘 input 10% · 🌗 cache read 60% · 🌕 saved 20%"
        );
    }

    #[test]
    fn spend_bar_splits_spent_and_saved_or_skips_legacy() {
        let m = json!({"pre_usd": "10", "sent_usd": "2.5", "saved_usd": "7.5"});
        assert_eq!(
            spend_bar(&m).as_deref(),
            Some("🌑🌑🌑🌑🌑🌕🌕🌕🌕🌕🌕🌕🌕🌕🌕🌕🌕🌕🌕🌕")
        );
        assert!(spend_bar(&json!({"money_saved": "1"})).is_none());
        let over = json!({"pre_usd": "1", "sent_usd": "3"});
        assert_eq!(spend_bar(&over).as_deref(), Some(&"🌑".repeat(20)[..]));
    }

    #[test]
    fn moon_summary_has_bars_for_every_block() {
        let mut sess = session(3, 3, "0.63");
        sess["context"] = json!({
            "model": "claude-opus-5",
            "pre": {"input": 0, "cache_read": 200894, "cache_write": 1822, "total": 202716},
            "sent": {"input": 4, "cache_read": 53202, "cache_write": 17625, "total": 70831},
        });
        let out = summary(
            &report(&me(), &billing(), Some("s"), Some(Ok(sess))),
            Style::Bar,
        );
        assert!(!out.contains("Sent by tier"), "got: {out}");
        assert_eq!(out.matches("  tokens  ").count(), 3, "got: {out}");
        assert_eq!(out.matches("  spend   ").count(), 2, "got: {out}");
        assert!(out.contains("Lifetime  62,992 requests"), "got: {out}");
        assert!(out.contains("Session  ~/w (main) — active"), "got: {out}");
        assert!(
            out.contains("          3 requests (3 settled)"),
            "got: {out}"
        );
        assert!(out.contains("Context   claude-opus-5"), "got: {out}");
        assert!(out.contains("🌕 optimized away 65%"), "got: {out}");
        assert!(
            out.contains("$9500.00 → $2657.47 (saved $6842.53)"),
            "got: {out}"
        );
    }
}
