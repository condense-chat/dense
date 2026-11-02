//! Engine mode: this binary was installed as Desktop's `claude`, with the real
//! engine beside it. It never logs in, prints nothing on stdout (Desktop's
//! stream), and runs the engine unproxied on any failure, so a condense
//! problem can't stop a Desktop session.

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use tokio::net::TcpStream;

use crate::Result;
use crate::api::auth;
use crate::config::Config;
use crate::error::Error;
use crate::harness::claude;

use super::{BIN, BUNDLE, REAL, REAL_BUNDLE, bundle};

const CONNECT_TIMEOUT: Duration = Duration::from_millis(800);
const INFO_FLAGS: &[&str] = &["--version", "-v", "--help", "-h"];
const LOG_FILE: &str = "desktop-engine.log";
const LOG_MAX_BYTES: u64 = 1 << 20;
const OTHER_PROVIDERS: &[&str] = &[
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_USE_GATEWAY",
    "CLAUDE_CODE_USE_MANTLE",
    "CLAUDE_CODE_USE_VERTEX",
];
const PROBE_STAMP: &str = ".desktop-probe";
const PROBE_TTL: Duration = Duration::from_secs(300);

/// The real engine, when this process is running as Desktop's `claude`.
pub fn sibling_engine() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    engine_beside(&exe)
}

/// Never returns on success: both paths exit with the engine's status.
pub async fn run(real: &Path) -> Result<()> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if is_info_call(&args) {
        return run_direct(real, &args);
    }
    let Ok(cfg) = Config::resolve(None, None) else {
        return run_direct(real, &args);
    };
    if let Err(why) = route(&cfg, real, &args).await {
        log(&cfg, &format!("direct launch: {why}"));
    }
    run_direct(real, &args)
}

pub(crate) fn log(cfg: &Config, line: &str) {
    let path = cfg.data_dir().join(LOG_FILE);
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > LOG_MAX_BYTES) {
        let _ = std::fs::remove_file(&path);
    }
    let _ = std::fs::create_dir_all(cfg.data_dir());
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path);
    if let Ok(mut file) = file {
        let stamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        // One write, so concurrent sessions can't interleave within a line.
        let _ = file.write_all(format!("{stamp} {line}\n").as_bytes());
    }
}

/// Plain `claude`/`claude.real` siblings, or a bundle wrapper's
/// `claude.real.app` beside `claude.app`.
fn engine_beside(exe: &Path) -> Option<PathBuf> {
    if exe.file_name() != Some(OsStr::new(BIN)) {
        return None;
    }
    let real = exe.with_file_name(REAL);
    if real.is_file() {
        return Some(real);
    }
    let dir = exe.ancestors().nth(4)?;
    let real = bundle::executable(&dir.join(REAL_BUNDLE));
    (bundle::executable(&dir.join(BUNDLE)) == exe && real.is_file()).then_some(real)
}

/// A provider Desktop chose itself (3P mode) or a base URL already off
/// Anthropic stays as configured.
fn env_is_routable(get: impl Fn(&str) -> Option<String>) -> bool {
    if OTHER_PROVIDERS
        .iter()
        .any(|key| get(key).is_some_and(|v| !v.is_empty()))
    {
        return false;
    }
    match get("ANTHROPIC_BASE_URL").filter(|v| !v.is_empty()) {
        None => true,
        Some(url) => reqwest::Url::parse(&url).is_ok_and(|u| {
            u.host_str()
                .is_some_and(|h| h == "anthropic.com" || h.ends_with(".anthropic.com"))
        }),
    }
}

fn is_info_call(args: &[OsString]) -> bool {
    match args {
        [only] => only.to_str().is_some_and(|a| INFO_FLAGS.contains(&a)),
        _ => false,
    }
}

/// Warm respawns are frequent, so a recent success skips the round trip.
async fn probe(cfg: &Config, token: &str) -> Result<()> {
    let stamp = cfg.data_dir().join(PROBE_STAMP);
    let key = probe_key(&cfg.api_base_url, token);
    if stamp_is_fresh(&stamp, &key) {
        return Ok(());
    }
    let status = auth::probe_token(cfg, token).await;
    if status != 200 {
        return Err(Error::msg(format!("token probe returned {status}")));
    }
    let _ = std::fs::create_dir_all(cfg.data_dir());
    let _ = std::fs::write(&stamp, key);
    Ok(())
}

/// Hashed, so the stamp on disk never holds the token.
fn probe_key(api_base_url: &str, token: &str) -> String {
    crate::harness::commands::sha256_hex(format!("{api_base_url}\n{token}").as_bytes())
}

// ponytail: proves the host answers now, not for the session. A running
// engine's env can't change, so an outage after launch still fails in Desktop;
// surviving that needs a local forwarding proxy. A bare connect also ignores
// HTTPS_PROXY, so a proxy-only network runs direct; probe through reqwest if
// such users appear.
async fn reachable(api_url: &str) -> bool {
    let addr = reqwest::Url::parse(api_url)
        .ok()
        .and_then(|u| Some(format!("{}:{}", u.host_str()?, u.port_or_known_default()?)));
    let Some(addr) = addr else {
        return false;
    };
    matches!(
        tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await,
        Ok(Ok(_))
    )
}

/// Runs the engine through condense, or says why it can't. Only ever returns
/// an error: a routed engine's exit ends this process.
async fn route(cfg: &Config, real: &Path, args: &[OsString]) -> Result<()> {
    if !env_is_routable(|key| std::env::var(key).ok()) {
        return Err(Error::msg("Desktop is set up for another provider"));
    }
    if crate::daemon::managed::switched_off(cfg) {
        return Err(Error::msg(
            "dense is turned off for this device by your organization",
        ));
    }
    let creds = auth::load_creds(cfg);
    if !creds.is_authenticated() {
        return Err(Error::msg("not logged in to dense"));
    }
    if !reachable(&cfg.api_base_url).await {
        return Err(Error::msg(format!("{} is unreachable", cfg.api_host)));
    }
    if let Some(token) = creds.token.as_deref() {
        probe(cfg, token).await?;
    }
    claude::run_engine(cfg, &creds, real, args).await
}

#[cfg(unix)]
fn run_direct(real: &Path, args: &[OsString]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    Err(Command::new(real).args(args).exec().into())
}

#[cfg(not(unix))]
fn run_direct(real: &Path, args: &[OsString]) -> Result<()> {
    let status = Command::new(real).args(args).status()?;
    std::process::exit(status.code().unwrap_or(1))
}

fn stamp_is_fresh(stamp: &Path, key: &str) -> bool {
    let age = std::fs::metadata(stamp)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok());
    age.is_some_and(|a| a < PROBE_TTL)
        && std::fs::read_to_string(stamp).is_ok_and(|held| held == key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        let pairs = pairs.to_vec();
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn signed_in_desktop_env_is_routed() {
        assert!(env_is_routable(env(&[])));
        assert!(env_is_routable(env(&[("ANTHROPIC_BASE_URL", "")])));
        assert!(env_is_routable(env(&[
            ("ANTHROPIC_BASE_URL", "https://api.anthropic.com"),
            ("CLAUDE_CODE_USE_GATEWAY", ""),
        ])));
    }

    #[test]
    fn third_party_desktop_env_is_left_alone() {
        assert!(!env_is_routable(env(&[(
            "ANTHROPIC_BASE_URL",
            "https://gateway.example/anthropic"
        )])));
        assert!(!env_is_routable(env(&[("CLAUDE_CODE_USE_BEDROCK", "1")])));
        assert!(!env_is_routable(env(&[("CLAUDE_CODE_USE_GATEWAY", "1")])));
        assert!(!env_is_routable(env(&[(
            "ANTHROPIC_BASE_URL",
            "https://anthropic.com.evil.example"
        )])));
    }

    #[test]
    fn only_a_bare_info_flag_skips_routing() {
        assert!(is_info_call(&["--version".into()]));
        assert!(!is_info_call(&[]));
        assert!(!is_info_call(&["--print".into(), "--version".into()]));
    }

    #[test]
    fn engine_mode_needs_both_the_name_and_the_sibling() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join(BIN);
        assert_eq!(engine_beside(&bin), None);

        std::fs::write(tmp.path().join(REAL), b"x").unwrap();
        assert_eq!(engine_beside(&bin), Some(tmp.path().join(REAL)));
        assert_eq!(engine_beside(&tmp.path().join("dense")), None);
    }

    #[test]
    fn engine_is_found_beside_a_bundle_wrapper() {
        let tmp = tempfile::tempdir().unwrap();
        let wrapper = bundle::executable(&tmp.path().join(BUNDLE));
        let original = bundle::executable(&tmp.path().join(REAL_BUNDLE));
        assert_eq!(engine_beside(&wrapper), None);

        std::fs::create_dir_all(original.parent().unwrap()).unwrap();
        std::fs::write(&original, b"x").unwrap();
        assert_eq!(engine_beside(&wrapper), Some(original.clone()));
        assert_eq!(
            engine_beside(&bundle::executable(&tmp.path().join("other.app"))),
            None
        );
    }

    #[tokio::test]
    async fn only_a_live_port_is_reachable() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        assert!(reachable(&url).await);

        drop(listener);
        assert!(!reachable(&url).await);
        assert!(!reachable("not a url").await);
    }

    #[test]
    fn probe_stamp_is_keyed_to_host_and_token() {
        let tmp = tempfile::tempdir().unwrap();
        let stamp = tmp.path().join(PROBE_STAMP);
        let key = probe_key("https://a", "token");
        assert!(!stamp_is_fresh(&stamp, &key));
        std::fs::write(&stamp, &key).unwrap();
        assert!(stamp_is_fresh(&stamp, &key));
        assert!(!stamp_is_fresh(&stamp, &probe_key("https://b", "token")));
        assert!(!stamp_is_fresh(&stamp, &probe_key("https://a", "other")));
        assert!(!key.contains("token"));
    }
}
