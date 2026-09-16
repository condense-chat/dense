//! `dense setup` — the first-run wizard the installer hands off to. Signs in
//! when no creds exist, asks which agent to route through condense and
//! whether to wire PATH, then tells the user how to start. Run via `curl … | sh`, the installer
//! reconnects stdin to the tty so the prompts work; with no tty it explains
//! and uses defaults.

use std::io::IsTerminal;
use std::path::Path;

use crate::api::auth;
use crate::config::Config;
use crate::{Result, env_file, persist, ui};

const AGENTS: &[(&str, &str)] = &[
    ("claude", "Claude Code"),
    ("codex", "Codex"),
    ("opencode", "opencode"),
];

pub async fn run(cfg: &Config) -> Result<()> {
    let interactive = std::io::stdin().is_terminal();
    // Login runs in its own frame so the code is the first thing on screen.
    if !auth::load_creds(cfg).is_authenticated() {
        auth::login(cfg).await?;
    }
    let res = wizard(cfg, interactive);
    if res.is_err() && interactive {
        let _ = cliclack::outro_cancel(ui::yellow("setup did not finish."));
    }
    res
}

/// Ask a yes/no question with a dim one-line explainer. Interactive: a
/// cliclack confirm on the tty (`None` = cancelled); otherwise echo the
/// default taken.
fn ask(interactive: bool, question: &str, explain: &str, default_yes: bool) -> Option<bool> {
    if !interactive {
        let default = if default_yes { "yes" } else { "no" };
        println!("{question}");
        println!("{}", ui::dim(explain));
        println!("{}", ui::dim(&format!("[no tty — default: {default}]")));
        println!();
        return Some(default_yes);
    }
    let _ = cliclack::log::remark(ui::dim(explain));
    cliclack::confirm(question)
        .initial_value(default_yes)
        .interact()
        .ok()
}

// A cancelled prompt already closed the frame ("Operation cancelled.").
fn cancelled(interactive: bool) -> Result<()> {
    if interactive {
        println!("{}", ui::dim("rerun `dense setup` anytime."));
    }
    Ok(())
}

fn on_path(dir: &Path) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d == dir))
        .unwrap_or(false)
}

/// Warnings for dirs that aren't visible to this shell yet. A restart only
/// helps once the profile wiring exists; otherwise point at the immediate
/// activation instead.
fn path_warnings(
    cfg: &Config,
    persisted: Option<&str>,
    wiring: &env_file::PathWiring,
) -> Vec<String> {
    let hint = match wiring {
        env_file::PathWiring::Wired => env_file::reload_hint(cfg),
        env_file::PathWiring::Manual(_) | env_file::PathWiring::Skipped => {
            env_file::activate_hint(cfg)
        }
    };
    let mut out = Vec::new();
    if !on_path(&cfg.bin_dir()) {
        out.push(format!(
            "{} isn't on your PATH yet; {hint}.",
            cfg.bin_dir().display()
        ));
    } else if let Some(tool) = persisted.filter(|_| !on_path(&cfg.shim_dir())) {
        out.push(format!("{hint} so `{tool}` routes through dense."));
    }
    out
}

fn pick_agent(interactive: bool) -> Option<&'static str> {
    if !interactive {
        println!("Which coding agent do you use?");
        println!("{}\n", ui::dim("[no tty — default: claude]"));
        return Some("claude");
    }
    let mut sel = cliclack::select("Which coding agent do you use?");
    for (name, label) in AGENTS {
        sel = sel.item(*name, *label, "");
    }
    sel.initial_value("claude").interact().ok()
}

fn start_hint(persisted: Option<&str>, tool: &str) -> String {
    let start = match persisted {
        Some(t) => t.to_string(),
        None => format!("dense {tool}"),
    };
    format!(
        "Run `{}` to start saving, or `{}` for help.",
        ui::cyan(&start),
        ui::cyan("dense -h")
    )
}

/// A warning that stays inside the cliclack frame when there is one.
fn warn(interactive: bool, text: &str) {
    if interactive {
        let _ = cliclack::log::warning(text);
    } else {
        eprintln!("{}", ui::yellow(text));
    }
}

fn wizard(cfg: &Config, interactive: bool) -> Result<()> {
    cfg.remember_profile()?;

    let note = format!(
        "dense is open source — read the code: {}",
        env!("CARGO_PKG_REPOSITORY")
    );
    if interactive {
        let _ = cliclack::intro(ui::cyan("condense setup"));
        let _ = cliclack::log::remark(ui::dim(&note));
    } else {
        println!("{}\n", ui::dim(&note));
    }

    let Some(tool) = pick_agent(interactive) else {
        return cancelled(interactive);
    };

    let Some(do_persist) = ask(
        interactive,
        &format!("Use condense for all {tool} sessions?"),
        &format!("the bare `{tool}` command will point at the dense {tool} wrapper."),
        true,
    ) else {
        return cancelled(interactive);
    };

    let Some(modify_path) = ask(
        interactive,
        "Add dense to your PATH?",
        &format!("{}.", env_file::path_change_summary(cfg)),
        true,
    ) else {
        return cancelled(interactive);
    };

    let wiring = env_file::ensure_env(cfg, modify_path)?;
    if let env_file::PathWiring::Manual(notes) = &wiring {
        warn(interactive, &notes.join("\n"));
    }
    let persisted = do_persist.then_some(tool);
    if do_persist {
        let report = persist::install_shims(cfg, &[tool.to_string()])?;
        for warning in &report.warnings {
            warn(interactive, warning);
        }
    }
    for warning in path_warnings(cfg, persisted, &wiring) {
        warn(interactive, &warning);
    }

    if interactive {
        let _ = cliclack::outro(start_hint(persisted, tool));
    } else {
        println!("\n{}", start_hint(persisted, tool));
    }
    Ok(())
}
