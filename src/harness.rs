//! The interceptor: launch an agent CLI with its traffic routed through
//! condense. `launch` is generic over the tool and the dialect it speaks
//! (static dispatch, no trait objects) — invalid tool×dialect pairings don't
//! type-check — and owns the universal lifecycle: ensure auth, open a session,
//! spawn, heartbeat, end. The `x-condense-*` headers ride here too; they're the
//! same for every tool and dialect.

pub mod claude;
pub mod codex;
pub mod opencode;

use std::path::Path;
use std::process::Stdio;

use crate::api::Api;
use crate::api::auth::{self, Creds};
use crate::api::dialect::{Anthropic, Dialect, OpenAi};
use crate::api::session::Session;
use crate::config::Config;
use crate::error::Error;
use crate::{Result, hosts, tool};

/// Like [`Tool`], but fans out to several dialects in one launch — OpenCode
/// declares one provider per dialect in a single config.
pub trait MultiTool {
    fn apply(&self, cmd: &mut tokio::process::Command, targets: &[DialectTarget]);

    fn binary(&self) -> &str;
    fn label(&self) -> &str;
}

/// An agent CLI, parameterised by a dialect it speaks. A tool implements
/// `Tool<D>` once per dialect it supports (Claude only Anthropic).
pub trait Tool<D: Dialect> {
    /// Point `cmd` at `target` — set the tool's base-URL/header env. The one
    /// tool-and-dialect-specific step.
    fn apply(&self, cmd: &mut tokio::process::Command, target: &ProxyTarget);

    fn binary(&self) -> &str;
    fn label(&self) -> &str;
}

/// A [`ProxyTarget`] tagged with the dialect route that produced it, so a
/// multi-provider tool can map each provider to its condense route.
pub struct DialectTarget {
    pub route: &'static str,
    pub target: ProxyTarget,
}

/// A resolved proxy target a tool wires itself to.
pub struct ProxyTarget {
    pub base_url: String,
    pub headers: Vec<(String, String)>,
}

/// Run `tool` through condense speaking `dialect`. Exits with the child's
/// status; only a launch failure returns.
pub async fn launch<D, T>(cfg: &Config, tool: T, dialect: D, args: &[String]) -> Result<()>
where
    D: Dialect,
    T: Tool<D>,
{
    let creds = auth::ensure_auth(cfg).await?;
    let api = Api::authed(cfg, &creds)?;
    let session = Session::new();
    let bin = tool::resolve_real(cfg, tool.binary())?;

    if args.is_empty() {
        announce(cfg, tool.label());
    }

    let target = ProxyTarget {
        base_url: dialect.base_url(cfg),
        headers: condense_headers(cfg, &creds, &session.id),
    };

    let mut cmd = tokio::process::Command::new(&bin);
    tool.apply(&mut cmd, &target);
    cmd.args(args);

    spawn_and_wait(&api, &session, &bin, cmd).await
}

/// Run a multi-provider `tool`, wiring one [`DialectTarget`] per dialect (the
/// full set condense speaks). Same lifecycle as [`launch`].
pub async fn launch_multi<T: MultiTool>(cfg: &Config, tool: T, args: &[String]) -> Result<()> {
    let creds = auth::ensure_auth(cfg).await?;
    let api = Api::authed(cfg, &creds)?;
    let session = Session::new();
    let bin = tool::resolve_real(cfg, tool.binary())?;

    if args.is_empty() {
        announce(cfg, tool.label());
    }

    let headers = condense_headers(cfg, &creds, &session.id);
    let targets = vec![
        DialectTarget {
            route: Anthropic.route(),
            target: ProxyTarget {
                base_url: Anthropic.base_url(cfg),
                headers: headers.clone(),
            },
        },
        DialectTarget {
            route: OpenAi.route(),
            target: ProxyTarget {
                base_url: OpenAi.base_url(cfg),
                headers: headers.clone(),
            },
        },
    ];

    let mut cmd = tokio::process::Command::new(&bin);
    tool.apply(&mut cmd, &targets);
    cmd.args(args);

    spawn_and_wait(&api, &session, &bin, cmd).await
}

/// Run an already-configured child to completion under a condense.
pub(crate) async fn spawn_and_wait(
    api: &Api,
    session: &Session,
    bin: &Path,
    mut cmd: tokio::process::Command,
) -> Result<()> {
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let heartbeat = session.start_heartbeat(api);
    let interrupts = swallow_interrupts();
    let status = cmd.status().await;
    interrupts.abort();
    heartbeat.abort();
    session.end(api).await;

    match status {
        Ok(s) => std::process::exit(exit_code(&s)),
        Err(e) => Err(Error::msg(format!(
            "failed to launch {}: {e}",
            bin.display()
        ))),
    }
}

pub(crate) fn announce(cfg: &Config, label: &str) {
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

/// The `x-condense-*` headers on every request — auth/user/session, plus the
/// optional upstream override. Universal; the upstream comes from [`Config`].
pub(crate) fn condense_headers(
    cfg: &Config,
    creds: &Creds,
    session_id: &str,
) -> Vec<(String, String)> {
    let mut h = Vec::new();
    if let Some(token) = &creds.token {
        h.push(("x-condense-auth-token".to_string(), token.clone()));
    }
    if let Some(user) = &creds.user_id {
        h.push(("x-condense-user-id".to_string(), user.clone()));
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
