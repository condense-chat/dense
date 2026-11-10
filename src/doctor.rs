//! `dense doctor` — verify the install is wired correctly and report a
//! status table. Reports only; it never mutates state.

use std::path::Path;

use crate::api::auth;
use crate::config::Config;
use crate::{Result, daemon, desktop, persist, tool, ui};

pub async fn run(cfg: &Config) -> Result<()> {
    ui::title("Installation check");
    ui::field("Server", &cfg.api_base_url);
    ui::section("Checks");

    line(on_path("dense"), "dense on PATH", "");
    line(
        path_contains(&cfg.shim_dir()),
        "Automatic routing available in this shell",
        &cfg.shim_dir().display().to_string(),
    );
    line(
        cfg.env_file().exists(),
        "Shell setup file",
        &cfg.env_file().display().to_string(),
    );

    match tool::resolve_real(cfg, "claude") {
        Ok(p) => line(true, "claude detected", &p.display().to_string()),
        Err(_) => warn(
            "claude detected",
            "not found — https://docs.claude.com/en/docs/claude-code",
        ),
    }

    let creds = auth::load_creds(cfg);
    line(
        creds.is_authenticated(),
        "Saved login",
        &cfg.cred_dir().display().to_string(),
    );
    if let Some(token) = creds.token.as_deref() {
        let progress = ui::progress("Checking your login with the server...");
        let status = auth::probe_token(cfg, token).await;
        drop(progress);
        line(
            status == 200,
            "Login accepted by server",
            &if status == 0 {
                "Could not reach the server; check your connection".into()
            } else {
                format!("HTTP {status}")
            },
        );
    }

    if desktop::in_use(cfg) {
        for version in desktop::unwrapped_versions(cfg) {
            warn("Claude Desktop engine not routed", &version);
        }
        if !daemon::running(cfg) {
            warn(
                "dense daemon not running",
                "new Claude Desktop engines won't route — run `dense claude-desktop install`",
            );
        }
    } else if persist::watching(cfg) && !daemon::running(cfg) {
        warn(
            "dense daemon not running",
            "editors' Claude Code settings won't be re-applied — run `dense persist claude`",
        );
    }

    for name in persist::load_record(cfg).tools.keys() {
        let path = persist::shim_path(cfg, name);
        line(
            path.exists(),
            &format!("Automatic routing: {name}"),
            &path.display().to_string(),
        );
    }
    ui::hint("Configure tools", "dense setup");
    Ok(())
}

fn line(ok: bool, label: &str, detail: &str) {
    ui::check(ok, label, detail);
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

fn warn(label: &str, detail: &str) {
    ui::check(false, label, detail);
}
