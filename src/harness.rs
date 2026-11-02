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

use crate::api::Api;
use crate::api::auth::{self, Creds};
use crate::api::dialect::Dialect;
use crate::api::session::Session;
use crate::config::Config;
use crate::daemon::routed::{Guard, Pin};
use crate::error::{Context, Error};
use crate::{Result, hosts, tool};

pub const SESSION_ENV: &str = "CONDENSE_SESSION_ID";
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

    /// The `x-condense-kind` value; the server takes it verbatim.
    fn kind(&self) -> &'static str;

    fn label(&self) -> &str;
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

/// Append the `/v1` API segment to a dialect base, tolerating a trailing slash.
pub(crate) fn with_v1(base: &str) -> String {
    format!("{}/v1", base.trim_end_matches('/'))
}

/// Run `tool` through condense, resolving one [`Target`] per declared dialect.
/// Exits with the child's status; only a launch failure returns.
pub async fn launch<T: Tool>(cfg: &Config, tool: T, args: &[String]) -> Result<()> {
    let creds = auth::ensure_auth(cfg).await?;
    let api = Api::authed(cfg, &creds)?;
    let session = Session::new();
    let bin = tool::resolve_real(cfg, tool.binary())?;

    if args.is_empty() {
        announce(cfg, tool.label());
    }

    let mut cmd = routed_command(cfg, &tool, &creds, &session, &bin);
    let routed = routed_span(cfg, tool.kind(), Pin::of(args), &mut cmd);
    cmd.args(args);

    spawn_and_wait(&api, &session, &bin, cmd, routed).await
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
    // Desktop's argv is its own; a session it did not name is bound by cwd.
    let pin = match Pin::of(&lossy) {
        Pin::Inject(_) => Pin::Unknown,
        pin => pin,
    };
    let routed = routed_span(cfg, tool.kind(), pin, &mut cmd);

    let stops = stop_signals();
    let mut child = cmd.spawn().ctx(format!("launching {}", bin.display()))?;
    crate::desktop::engine::log(
        cfg,
        &format!("routed launch: session {} via {}", session.id, cfg.api_host),
    );
    let heartbeat = session.start_heartbeat(&api);
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
fn routed_span(
    cfg: &Config,
    kind: &str,
    pin: Pin,
    cmd: &mut tokio::process::Command,
) -> Option<Guard> {
    if kind != "claude_code" {
        return None;
    }
    crate::daemon::sessions::remember_root(cfg);
    let session_id = match pin {
        Pin::Inject(id) => {
            cmd.args(["--session-id", &id]);
            Some(id)
        }
        Pin::Known(id) => Some(id),
        Pin::Unknown => None,
    };
    Guard::open(cfg, session_id)
}

/// Run an already-configured child to completion under a condense session.
async fn spawn_and_wait(
    api: &Api,
    session: &Session,
    bin: &Path,
    mut cmd: tokio::process::Command,
    routed: Option<Guard>,
) -> Result<()> {
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let heartbeat = session.start_heartbeat(api);
    let interrupts = swallow_interrupts();
    let status = cmd.status().await;
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

fn announce(cfg: &Config, label: &str) {
    let scheme = hosts::default_scheme_for(&cfg.api_host);
    eprintln!(
        "● condense activated — {label} is routing through {}",
        cfg.api_host
    );
    eprintln!(
        "  observe usage at {}",
        hosts::sibling(&cfg.api_host, "helm", scheme)
    );
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
