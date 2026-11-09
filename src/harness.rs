//! The interceptor: launch an agent CLI with its traffic routed through
//! condense. `launch` owns the universal lifecycle (ensure auth, open a
//! session, spawn, heartbeat, end) and resolves one [`Target`] per dialect the
//! tool declares — one for Claude/Codex, several for OpenCode. The
//! `x-condense-*` headers ride here too; they're the same for every tool.

pub mod claude;
pub mod codex;
pub mod commands;
pub mod opencode;

use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;

use crate::api::Api;
use crate::api::auth::{self, Creds};
use crate::api::dialect::Dialect;
use crate::api::session::Session;
use crate::config::Config;
use crate::daemon::routed::{Guard, Pin};
use crate::error::{Context, Error};
use crate::{Result, hosts, info, selfupdate, tool, ui};

pub const SESSION_ENV: &str = "CONDENSE_SESSION_ID";
const BANNER_TIMEOUT: Duration = Duration::from_millis(2500);
const FOOTER_BAR: usize = 12;
const SESSION_END_TIMEOUT: Duration = Duration::from_secs(2);

/// An agent CLI routed through condense. A tool declares which dialects it
/// wires — one (Claude/Codex) or several (OpenCode) — and configures the child
/// command from the resolved [`Target`]s.
pub trait Tool {
    /// Point `cmd` at the resolved targets — set the tool's base-URL/header
    /// env. The one tool-specific step.
    fn apply(&self, cmd: &mut tokio::process::Command, targets: &[Target]);

    fn binary(&self) -> &str;

    /// The dialects this tool speaks; `apply` gets one [`Target`] per entry.
    fn dialects(&self) -> &'static [Dialect];

    /// Hand `savings` to the tool's own UI; `false` leaves it to the banner.
    fn footer(
        &self,
        _cfg: &Config,
        _cmd: &mut tokio::process::Command,
        _savings: &Savings,
    ) -> bool {
        false
    }

    /// The `x-condense-kind` value; the server takes it verbatim.
    fn kind(&self) -> &'static str;

    fn label(&self) -> &str;
}

/// Last 30 days from the same endpoint helm reads.
pub enum Savings {
    /// Money off API traffic; `share` is the fraction of the bill saved.
    Api { saved_usd: f64, share: f64 },
    /// How much further the tool's own subscription went.
    Plan { stretch: f64 },
}

/// One resolved dialect a tool wires itself to: its route, the condense base
/// URL, and the shared `x-condense-*` headers.
pub struct Target {
    pub base_url: String,
    pub headers: Vec<(String, String)>,
    pub route: &'static str,
    /// `$CONDENSE_UPSTREAM_URL` — set when the proxy forwards to a
    /// non-default upstream (also sent as `x-condense-upstream-url`).
    pub upstream: Option<String>,
}

impl Savings {
    /// Footer row: a bar filled to the share condense took off, then the text.
    pub fn footer(&self, name: &str) -> String {
        let share = match self {
            Self::Api { share, .. } => *share,
            Self::Plan { stretch } => 1.0 - 1.0 / stretch,
        };
        let filled = ((share.clamp(0.0, 1.0) * FOOTER_BAR as f64).round() as usize).min(FOOTER_BAR);
        format!(
            "{} {}{} {}",
            ui::bold(&ui::green(name)),
            ui::green(&"█".repeat(filled)),
            ui::dim(&"░".repeat(FOOTER_BAR - filled)),
            ui::green(&format!("{} in last 30 days", self.text())),
        )
    }

    /// Banner line for when the footer is taken: names condense in the text.
    pub fn line(&self) -> String {
        format!("{} with condense in last 30 days", self.text())
    }

    /// The stretch on this tool's own plan, else money saved on API traffic.
    fn of(value: &Value, kind: &str) -> Option<Self> {
        let on_plan = matches!(kind, "claude_code" | "codex");
        let stretch = value["subscription"]["families"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|f| f["family"] == kind)
            .and_then(|f| f["stretch"].as_f64())
            .filter(|&s| stretch_label(s).is_some());
        if let Some(stretch) = stretch.filter(|_| on_plan) {
            return Some(Self::Plan { stretch });
        }
        let saved_usd = info::usd(&value["api"]["saved_usd"]);
        let share = value["api"]["saved_pct"].as_f64().unwrap_or(0.0);
        (saved_usd > 0.0).then_some(Self::Api { saved_usd, share })
    }

    fn text(&self) -> String {
        match self {
            Self::Api { saved_usd, .. } => {
                format!("{} saved on your API bill", usd_short(*saved_usd))
            }
            Self::Plan { stretch } => format!(
                "{} more usage estimated",
                stretch_label(*stretch)
                    .unwrap_or_default()
                    .trim_start_matches('+')
            ),
        }
    }
}

/// Append the `/v1` API segment to a dialect base, tolerating a trailing slash.
pub(crate) fn with_v1(base: &str) -> String {
    format!("{}/v1", base.trim_end_matches('/'))
}

/// Run `tool` through condense, resolving one [`Target`] per declared dialect.
/// Exits with the child's status; only a launch failure returns.
pub async fn launch<T: Tool>(cfg: &Config, tool: T, args: &[String]) -> Result<()> {
    let bin = tool::resolve_real(cfg, tool.binary())?;
    if crate::daemon::managed::switched_off(cfg) {
        eprintln!(
            "dense is turned off by your organization; running {} directly",
            tool.label()
        );
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        return run_direct(&bin, &args);
    }
    let creds = auth::ensure_auth(cfg).await?;
    crate::daemon::managed::enroll_once(cfg).await;
    let api = Api::authed(cfg, &creds)?;
    let session = Session::new();

    let mut cmd = routed_command(cfg, &tool, &creds, &session, &bin);
    if args.is_empty() {
        announce(&api, cfg, &tool, &mut cmd).await;
    }
    cmd.args(args);

    let repo = cfg!(feature = "session-repo");
    let (kind, pin) = (tool.kind(), Pin::of(args));
    let span = |child| routed_span(cfg, kind, pin, child);
    spawn_and_wait(&api, &session, repo, &bin, cmd, span).await
}

/// Replace this process with `real` (Unix) or run it and exit with its status.
#[cfg(unix)]
pub fn run_direct(real: &Path, args: &[OsString]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    Err(std::process::Command::new(real).args(args).exec().into())
}

#[cfg(not(unix))]
pub fn run_direct(real: &Path, args: &[OsString]) -> Result<()> {
    let status = std::process::Command::new(real).args(args).status()?;
    std::process::exit(status.code().unwrap_or(1))
}

/// Run Desktop's real engine through condense. Unlike [`launch`] this never
/// logs in or prints, and forwards stop signals, since Desktop owns the
/// process and stdout carries its stream. Returns only if the spawn failed,
/// so the caller can still run the engine directly.
pub async fn launch_engine<T: Tool>(
    cfg: &Config,
    tool: T,
    creds: &Creds,
    bin: &Path,
    args: &[OsString],
) -> Result<()> {
    let api = Api::authed(cfg, creds)?;
    let session = Session::new();
    let mut cmd = routed_command(cfg, &tool, creds, &session, bin);
    cmd.args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let lossy: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let pin = Pin::of(&lossy);

    let stops = stop_signals();
    let mut child = cmd.spawn().ctx(format!("launching {}", bin.display()))?;
    let routed = routed_span(cfg, tool.kind(), pin, child.id());
    crate::desktop::engine::log(
        cfg,
        &format!("routed launch: session {} via {}", session.id, cfg.api_host),
    );
    let repo = cfg!(feature = "session-repo");
    let heartbeat = session.start_heartbeat(&api, repo);
    let signals = forward_signals(stops, child.id());
    let status = child.wait().await;
    signals.abort();
    heartbeat.abort();
    if let Some(g) = routed {
        g.close().await;
    }
    let _ = tokio::time::timeout(SESSION_END_TIMEOUT, session.end(&api)).await;
    std::process::exit(status.map(|s| exit_code(&s)).unwrap_or(1))
}

fn routed_command<T: Tool>(
    cfg: &Config,
    tool: &T,
    creds: &Creds,
    session: &Session,
    bin: &Path,
) -> tokio::process::Command {
    let device_id = crate::device::id(cfg).ok();
    let headers = condense_headers(cfg, creds, device_id.as_deref(), &session.id, tool.kind());
    let targets: Vec<Target> = tool
        .dialects()
        .iter()
        .map(|&d| Target {
            route: d.route(),
            base_url: d.base_url(cfg),
            headers: headers.clone(),
            upstream: cfg.upstream().map(str::to_string),
        })
        .collect();

    let mut cmd = tokio::process::Command::new(bin);
    tool.apply(&mut cmd, &targets);
    cmd.env(SESSION_ENV, &session.id);
    // The child's `dense usage` must land on the same api as this session.
    cmd.env("CONDENSE_URL", &cfg.api_base_url);
    cmd
}

/// The proxy reports a routed Claude Code child's usage; record its span so the
/// daemon's disk reader skips it.
/// Opened whether or not the device is managed yet: management can start
/// mid-session, and a session without a span would then be counted twice.
fn routed_span(cfg: &Config, kind: &str, pin: Pin, child: Option<u32>) -> Option<Guard> {
    if kind != "claude_code" || !cfg!(feature = "offline-usage") {
        return None;
    }
    crate::daemon::sessions::remember_root(cfg);
    let session_id = match pin {
        Pin::Known(id) => Some(id),
        Pin::Unknown => None,
    };
    Guard::open(cfg, session_id, child)
}

/// Run an already-configured child to completion under a condense session.
async fn spawn_and_wait(
    api: &Api,
    session: &Session,
    repo: bool,
    bin: &Path,
    mut cmd: tokio::process::Command,
    span: impl FnOnce(Option<u32>) -> Option<Guard>,
) -> Result<()> {
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let heartbeat = session.start_heartbeat(api, repo);
    let interrupts = swallow_interrupts();
    let (status, routed) = match cmd.spawn() {
        Ok(mut child) => {
            let routed = span(child.id());
            (child.wait().await, routed)
        }
        Err(e) => (Err(e), None),
    };
    interrupts.abort();
    heartbeat.abort();
    if let Some(g) = routed {
        g.close().await;
    }
    session.end(api).await;

    match status {
        Ok(s) => std::process::exit(exit_code(&s)),
        Err(e) => Err(Error::msg(format!(
            "failed to launch {}: {e}",
            bin.display()
        ))),
    }
}

async fn announce<T: Tool>(api: &Api, cfg: &Config, tool: &T, cmd: &mut tokio::process::Command) {
    let scheme = hosts::default_scheme_for(&cfg.api_host);
    eprintln!(
        "● condense activated — {} is routing through {}",
        tool.label(),
        cfg.api_host
    );
    eprintln!(
        "  observe usage at {}",
        hosts::sibling(&cfg.api_host, "helm", scheme)
    );
    let (value, latest) = tokio::join!(
        tokio::time::timeout(
            BANNER_TIMEOUT,
            api.get_json::<Value>("/v1/me/analytics/value")
        ),
        tokio::time::timeout(BANNER_TIMEOUT, selfupdate::newer(cfg)),
    );
    let savings = value
        .ok()
        .and_then(Result::ok)
        .and_then(|v| Savings::of(&v, tool.kind()));
    if let Some(savings) = savings
        && !tool.footer(cfg, cmd, &savings)
    {
        eprintln!("  {}", ui::bold(&ui::green(&savings.line())));
    }
    if let Ok(Some(latest)) = latest {
        eprintln!(
            "  {}",
            ui::yellow(&format!("dense {latest} is out, run `dense self update`"))
        );
    }
    eprintln!();
}

/// The `x-condense-*` headers on every request — auth/user/device/session,
/// the harness kind, plus the optional upstream override.
fn condense_headers(
    cfg: &Config,
    creds: &Creds,
    device_id: Option<&str>,
    session_id: &str,
    kind: &str,
) -> Vec<(String, String)> {
    let mut h = vec![("x-condense-kind".to_string(), kind.to_string())];
    if let Some(token) = &creds.token {
        h.push(("x-condense-auth-token".to_string(), token.clone()));
    }
    if let Some(user) = &creds.user_id {
        h.push(("x-condense-user-id".to_string(), user.clone()));
    }
    if let Some(device) = device_id {
        h.push((crate::device::HEADER.to_string(), device.to_string()));
    }
    h.push(("x-condense-session-id".to_string(), session_id.to_string()));
    if let Some(upstream) = cfg.upstream() {
        h.push(("x-condense-upstream-url".to_string(), upstream.to_string()));
    }
    h
}

/// Child exit status → our exit code, keeping the unix `128 + signal`
/// convention for signal deaths.
#[cfg(unix)]
fn exit_code(status: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

#[cfg(not(unix))]
fn exit_code(status: &std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

/// "+58%", or "3×" from 3x up; None under 5%. Matches helm's `stretchLabel`.
fn stretch_label(stretch: f64) -> Option<String> {
    let more = ((stretch - 1.0) * 100.0).round();
    if !more.is_finite() || more < 5.0 {
        return None;
    }
    Some(if stretch >= 3.0 {
        format!("{}×", stretch.round())
    } else {
        format!("+{more}%")
    })
}

/// While the child runs, Ctrl-C belongs to it — the tool decides whether an
/// interrupt cancels a step or exits. Without this, the same SIGINT kills
/// dense too: the heartbeat dies and the session never ends cleanly.
fn swallow_interrupts() -> tokio::task::JoinHandle<()> {
    tokio::spawn(async {
        loop {
            let _ = tokio::signal::ctrl_c().await;
        }
    })
}

/// Registered before the engine spawns: until a handler exists, a stop signal
/// kills dense outright and orphans the engine.
#[cfg(unix)]
fn stop_signals() -> Option<[tokio::signal::unix::Signal; 3]> {
    use tokio::signal::unix::{SignalKind, signal};

    Some([
        signal(SignalKind::terminate()).ok()?,
        signal(SignalKind::interrupt()).ok()?,
        signal(SignalKind::hangup()).ok()?,
    ])
}

#[cfg(not(unix))]
fn stop_signals() -> Option<()> {
    None
}

/// Desktop stops the engine by signalling the process it spawned, which is us.
#[cfg(unix)]
fn forward_signals(
    signals: Option<[tokio::signal::unix::Signal; 3]>,
    child: Option<u32>,
) -> tokio::task::JoinHandle<()> {
    use rustix::process::{Pid, Signal, kill_process};

    tokio::spawn(async move {
        let pid = child
            .and_then(|id| i32::try_from(id).ok())
            .and_then(Pid::from_raw);
        let (Some(pid), Some([mut term, mut int, mut hup])) = (pid, signals) else {
            return;
        };
        loop {
            let sig = tokio::select! {
                _ = term.recv() => Signal::TERM,
                _ = int.recv() => Signal::INT,
                _ = hup.recv() => Signal::HUP,
            };
            let _ = kill_process(pid, sig);
        }
    })
}

#[cfg(not(unix))]
fn forward_signals(_signals: Option<()>, _child: Option<u32>) -> tokio::task::JoinHandle<()> {
    swallow_interrupts()
}

/// Helm's `usdShort`: "$1.2M", "$4.5K", "$412", "$12.34".
fn usd_short(n: f64) -> String {
    if n >= 1e6 {
        format!("${:.1}M", n / 1e6)
    } else if n >= 1e3 {
        format!("${:.1}K", n / 1e3)
    } else if n >= 100.0 {
        format!("${}", n.round())
    } else {
        format!("${n:.2}")
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn text(v: &Value, kind: &str) -> Option<String> {
        Savings::of(v, kind).map(|s| s.line())
    }

    #[test]
    fn savings_prefers_the_tools_own_plan() {
        let v = json!({
            "subscription": {"families": [
                {"family": "codex", "stretch": 2.4},
                {"family": "claude_code", "stretch": 1.58},
            ]},
            "api": {"saved_usd": "412.30", "saved_pct": 0.42},
        });
        assert_eq!(
            text(&v, "claude_code").as_deref(),
            Some("58% more usage estimated with condense in last 30 days")
        );
        assert_eq!(
            text(&v, "codex").as_deref(),
            Some("140% more usage estimated with condense in last 30 days")
        );
        assert_eq!(
            text(&v, "opencode").as_deref(),
            Some("$412 saved on your API bill with condense in last 30 days")
        );
    }

    #[test]
    fn savings_falls_back_to_api_money_then_nothing() {
        let api = json!({"subscription": null, "api": {"saved_usd": "1520.10"}});
        assert_eq!(
            text(&api, "claude_code").as_deref(),
            Some("$1.5K saved on your API bill with condense in last 30 days")
        );
        let flat = json!({
            "subscription": {"families": [{"family": "claude_code", "stretch": 1.02}]},
            "api": null,
        });
        assert_eq!(text(&flat, "claude_code"), None);
        assert_eq!(text(&json!({}), "claude_code"), None);
    }

    #[test]
    fn footer_fills_to_the_share_condense_took_off() {
        owo_colors::set_override(false);
        let plan = Savings::Plan { stretch: 2.0 };
        assert_eq!(
            plan.footer("Dense Claude"),
            "Dense Claude ██████░░░░░░ 100% more usage estimated in last 30 days"
        );
        assert_eq!(
            Savings::Plan { stretch: 3.4 }.footer("Dense Claude"),
            "Dense Claude ████████░░░░ 3× more usage estimated in last 30 days"
        );
        let api = Savings::Api {
            saved_usd: 412.3,
            share: 0.25,
        };
        assert_eq!(
            api.footer("Dense Claude"),
            "Dense Claude ███░░░░░░░░░ $412 saved on your API bill in last 30 days"
        );
    }

    #[test]
    fn stretch_label_matches_helm() {
        assert_eq!(stretch_label(1.04), None);
        assert_eq!(stretch_label(1.05).as_deref(), Some("+5%"));
        assert_eq!(stretch_label(2.99).as_deref(), Some("+199%"));
        assert_eq!(stretch_label(3.4).as_deref(), Some("3×"));
        assert_eq!(stretch_label(f64::NAN), None);
    }

    #[test]
    fn usd_short_matches_helm() {
        assert_eq!(usd_short(12.345), "$12.35");
        assert_eq!(usd_short(412.3), "$412");
        assert_eq!(usd_short(4_512.0), "$4.5K");
        assert_eq!(usd_short(1_260_000.0), "$1.3M");
    }
}
