//! `dense doctor` — verify the install is wired correctly and report a
//! status table. Reports only; it never mutates state.

use std::path::Path;

use crate::api::auth;
use crate::config::Config;
use crate::{Result, persist, tool, ui};

pub async fn run(cfg: &Config) -> Result<()> {
    println!(
        "{} {}\n",
        ui::bold("dense doctor —"),
        ui::dim(&cfg.api_base_url)
    );

    line(on_path("dense"), "dense on PATH", "");
    line(
        path_contains(&cfg.shim_dir()),
        "override dir on PATH",
        &cfg.shim_dir().display().to_string(),
    );
    line(
        cfg.env_file().exists(),
        "env file present",
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
        "authenticated",
        &cfg.cred_dir().display().to_string(),
    );
    if let Some(token) = creds.token.as_deref() {
        let status = auth::probe_token(cfg, token).await;
        line(
            status == 200,
            "token valid (/v1/me)",
            &format!("status {status}"),
        );
    }

    for name in persist::load_record(cfg).tools.keys() {
        let path = persist::shim_path(cfg, name);
        line(
            path.exists(),
            &format!("shim: {name}"),
            &path.display().to_string(),
        );
    }
    Ok(())
}

fn line(ok: bool, label: &str, detail: &str) {
    let mark = if ok {
        ui::green("\u{2713}")
    } else {
        ui::red("\u{2717}")
    };
    print_row(&mark, label, detail);
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

fn warn(label: &str, detail: &str) {
    print_row(&ui::yellow("!"), label, detail);
}
