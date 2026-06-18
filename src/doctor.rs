//! `dense doctor` — verify the install is wired correctly and report a
//! status table. Reports only; it never mutates state.

use std::path::Path;

use crate::api::auth;
use crate::config::Config;
use crate::{Result, persist, tool, ui};

/// A check's outcome. `Warn`/`Fail` both count as issues a caller (e.g. setup's
/// offer-to-fix) may act on; only the rendered mark differs.
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
    pub fn is_issue(&self) -> bool {
        !matches!(self.health, Health::Ok)
    }
}

/// Run every check and return the results without printing — the structured
/// view [`run`] and setup share.
pub async fn diagnose(cfg: &Config) -> Vec<Check> {
    let mut checks = vec![
        check(on_path("dense"), "dense on PATH", ""),
        check(
            path_contains(&cfg.shim_dir()),
            "override dir on PATH",
            &cfg.shim_dir().display().to_string(),
        ),
        check(
            cfg.env_file().exists(),
            "env file present",
            &cfg.env_file().display().to_string(),
        ),
    ];

    match tool::resolve_real(cfg, "claude") {
        Ok(p) => checks.push(check(true, "claude detected", &p.display().to_string())),
        Err(_) => checks.push(warn(
            "claude detected",
            "not found — https://docs.claude.com/en/docs/claude-code",
        )),
    }

    let creds = auth::load_creds(cfg);
    checks.push(check(
        creds.is_authenticated(),
        "authenticated",
        &cfg.cred_dir().display().to_string(),
    ));
    if let Some(token) = creds.token.as_deref() {
        let status = auth::probe_token(cfg, token).await;
        checks.push(check(
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

fn on_path(name: &str) -> bool {
    path_dirs().iter().any(|d| {
        d.join(name).is_file()
            || d.join(format!("{name}.exe")).is_file()
            || d.join(format!("{name}.cmd")).is_file()
    })
}

fn path_contains(dir: &Path) -> bool {
    path_dirs().iter().any(|d| d == dir)
}

fn path_dirs() -> Vec<std::path::PathBuf> {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default()
}

fn print_row(mark: &str, label: &str, detail: &str) {
    if detail.is_empty() {
        println!("  {mark} {label}");
    } else {
        println!("  {mark} {label}  {}", ui::dim(&format!("({detail})")));
    }
}

fn warn(label: &str, detail: &str) -> Check {
    Check {
        detail: detail.to_string(),
        health: Health::Warn,
        label: label.to_string(),
    }
}
