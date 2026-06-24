//! `dense doctor` — verify the install is wired correctly and report a
//! status table. Reports only; it never mutates state.

use crate::api::auth;
use crate::config::Config;
use crate::{Result, persist, tool, ui};

/// Lowest Claude Code that honors `_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL`
/// (the harness's lever for the 1M context window behind the proxy). Older
/// builds silently fall back to the 200K window.
const MIN_CLAUDE_VERSION: (u32, u32, u32) = (2, 1, 169);

/// A check's outcome. `Fail` is a critical wiring problem an agent could repair;
/// `Warn` is a transient/environmental note (PATH not reloaded, not logged in)
/// that shouldn't block. Only the rendered mark differs.
pub enum Health {
    Fail,
    Ok,
    Warn,
}

/// One row of the doctor report, kept structured so callers other than the
/// printed table can see what failed.
pub struct Check {
    pub detail: String,
    pub health: Health,
    pub label: String,
}

impl Check {
    pub fn is_critical(&self) -> bool {
        matches!(self.health, Health::Fail)
    }
}

/// Run every check and return the results without printing — the structured
/// view [`run`] and setup share.
pub async fn diagnose(cfg: &Config) -> Vec<Check> {
    let mut checks = vec![check(
        cfg.env_file().exists(),
        "env file present",
        &cfg.env_file().display().to_string(),
    )];
    checks.extend(new_shell_checks(cfg));

    match tool::resolve_real(cfg, "claude") {
        Ok(p) => {
            checks.push(check(true, "claude detected", &p.display().to_string()));
            if let Some(v) = claude_version_check(&p) {
                checks.push(v);
            }
        }
        Err(_) => checks.push(warn(
            "claude detected",
            "not found — https://docs.claude.com/en/docs/claude-code",
        )),
    }

    let creds = auth::load_creds(cfg);
    checks.push(soft(
        creds.is_authenticated(),
        "authenticated",
        &cfg.cred_dir().display().to_string(),
    ));
    if let Some(token) = creds.token.as_deref() {
        let status = auth::probe_token(cfg, token).await;
        checks.push(soft(
            status == 200,
            "token valid (/v1/me)",
            &format!("status {status}"),
        ));
    }

    for name in persist::load_record(cfg).tools.keys() {
        let path = persist::shim_path(cfg, name);
        checks.push(check(
            path.exists(),
            &format!("shim: {name}"),
            &path.display().to_string(),
        ));
    }
    checks
}

/// Render a diagnosed report as the familiar status table.
pub fn print_report(cfg: &Config, checks: &[Check]) {
    println!(
        "{} {}\n",
        ui::bold("dense doctor —"),
        ui::dim(&cfg.api_base_url)
    );
    for c in checks {
        let mark = match c.health {
            Health::Ok => ui::green("\u{2713}"),
            Health::Warn => ui::yellow("!"),
            Health::Fail => ui::red("\u{2717}"),
        };
        print_row(&mark, &c.label, &c.detail);
    }
}

pub async fn run(cfg: &Config) -> Result<()> {
    print_report(cfg, &diagnose(cfg).await);
    Ok(())
}

fn check(ok: bool, label: &str, detail: &str) -> Check {
    Check {
        detail: detail.to_string(),
        health: if ok { Health::Ok } else { Health::Fail },
        label: label.to_string(),
    }
}

/// Whether a freshly-started shell actually has the dense dirs on PATH — the
/// failure users miss most: the install looks fine, but a new terminal still
/// runs the system `claude`, silently bypassing condense. We start the user's
/// `$SHELL` as a login+interactive session (sourcing the same profiles a new
/// terminal would) and inspect the PATH it ends up with.
fn new_shell_checks(cfg: &Config) -> Vec<Check> {
    let Some(paths) = new_session_path() else {
        return vec![warn(
            "new-shell PATH",
            "couldn't start your shell to verify a new session",
        )];
    };
    let mut out = vec![check(
        paths.iter().any(|d| d == &cfg.bin_dir()),
        "dense on PATH in a new shell",
        &cfg.bin_dir().display().to_string(),
    )];
    if shim_installed(cfg, "claude") {
        out.push(check(
            paths.iter().any(|d| d == &cfg.shim_dir()),
            "claude routes via dense in a new shell",
            &cfg.shim_dir().display().to_string(),
        ));
    }
    out
}

/// The PATH a brand-new login+interactive shell sees, or `None` if we couldn't
/// capture it. The output is bracketed by sentinels so profile/MOTD noise on
/// stdout doesn't corrupt the parse.
#[cfg(not(windows))]
fn new_session_path() -> Option<Vec<std::path::PathBuf>> {
    const OPEN: &str = "__DENSE_PATH__";
    const CLOSE: &str = "__DENSE_END__";
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let probe = format!("printf '{OPEN}%s{CLOSE}' \"$PATH\"");
    let out = std::process::Command::new(&shell)
        .args(["-l", "-i", "-c", &probe])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let start = stdout.find(OPEN)? + OPEN.len();
    let rest = &stdout[start..];
    let end = rest.find(CLOSE)?;
    let path = &rest[..end];
    (!path.is_empty()).then(|| std::env::split_paths(path).collect())
}

#[cfg(windows)]
fn new_session_path() -> Option<Vec<std::path::PathBuf>> {
    std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect())
}

fn print_row(mark: &str, label: &str, detail: &str) {
    if detail.is_empty() {
        println!("  {mark} {label}");
    } else {
        println!("  {mark} {label}  {}", ui::dim(&format!("({detail})")));
    }
}

/// Warn when the installed Claude Code predates [`MIN_CLAUDE_VERSION`]: the
/// harness can't force the 1M window on it, so the user silently gets the 200K
/// one. `None` (no row) if `claude --version` can't be read or parsed.
fn claude_version_check(claude: &std::path::Path) -> Option<Check> {
    let out = std::process::Command::new(claude)
        .arg("--version")
        .output()
        .ok()?;
    let ver = parse_version(&String::from_utf8_lossy(&out.stdout))?;
    let shown = format!("{}.{}.{}", ver.0, ver.1, ver.2);
    if ver >= MIN_CLAUDE_VERSION {
        Some(check(true, "claude gets condense's 1M context", &shown))
    } else {
        let (a, b, c) = MIN_CLAUDE_VERSION;
        Some(warn(
            "claude gets condense's 1M context",
            &format!("{shown} < {a}.{b}.{c} — likely the 200K window; upgrade Claude Code"),
        ))
    }
}

/// First `MAJOR.MINOR.PATCH` in `claude --version` output (e.g.
/// "2.1.177 (Claude Code)"), tolerant of surrounding text.
fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    text.split(|c: char| !c.is_ascii_digit() && c != '.')
        .find_map(|tok| {
            let mut parts = tok.split('.');
            let major = parts.next()?.parse().ok()?;
            let minor = parts.next()?.parse().ok()?;
            let patch = parts.next()?.parse().ok()?;
            Some((major, minor, patch))
        })
}

fn shim_installed(cfg: &Config, name: &str) -> bool {
    persist::load_record(cfg).tools.contains_key(name)
}

/// A non-critical check: green when satisfied, a yellow warn otherwise.
fn soft(ok: bool, label: &str, detail: &str) -> Check {
    Check {
        detail: detail.to_string(),
        health: if ok { Health::Ok } else { Health::Warn },
        label: label.to_string(),
    }
}

fn warn(label: &str, detail: &str) -> Check {
    Check {
        detail: detail.to_string(),
        health: Health::Warn,
        label: label.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_tolerates_trailing_text() {
        assert_eq!(parse_version("2.1.177 (Claude Code)"), Some((2, 1, 177)));
        assert_eq!(parse_version("v2.1.169"), Some((2, 1, 169)));
        assert_eq!(parse_version("Claude Code 2.0.5"), Some((2, 0, 5)));
        assert_eq!(parse_version("no version here"), None);
    }

    #[test]
    fn min_version_is_an_inclusive_floor() {
        assert!((2, 1, 169) >= MIN_CLAUDE_VERSION);
        assert!((2, 1, 177) >= MIN_CLAUDE_VERSION);
        assert!((2, 2, 0) >= MIN_CLAUDE_VERSION);
        assert!((2, 1, 168) < MIN_CLAUDE_VERSION);
        assert!((2, 0, 999) < MIN_CLAUDE_VERSION);
    }
}
