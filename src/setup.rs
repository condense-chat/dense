//! `dense setup` — the first-run wizard the installer hands off to. Asks
//! which tools to route through condense and whether to wire PATH, then
//! tells the user how to start. Run via `curl … | sh`, the installer
//! reconnects stdin to the tty so the prompts work; with no tty it explains
//! and uses defaults.

use std::io::IsTerminal;
use std::path::Path;

use crate::config::Config;
use crate::{Result, env_file, persist, tool, ui};

pub async fn run(cfg: &Config) -> Result<()> {
    let interactive = std::io::stdin().is_terminal();
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
fn path_warnings(cfg: &Config, tools: &[String], wiring: &env_file::PathWiring) -> Vec<String> {
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
    } else if let Some(first) = tools.first().filter(|_| !on_path(&cfg.shim_dir())) {
        out.push(format!("{hint} so `{first}` routes through dense."));
    }
    out
}

/// Pick which supported tools get a shim. Interactive: a multiselect with
/// the installed ones pre-checked (`None` = cancelled); otherwise echo the
/// installed set.
fn pick_tools(cfg: &Config, interactive: bool) -> Option<Vec<String>> {
    let question = "Route which tools through condense?";
    let explain = "the bare command (e.g. `claude`) will point at the dense wrapper.";
    let installed: Vec<String> = persist::names()
        .filter(|name| tool::resolve_real(cfg, name).is_ok())
        .map(str::to_string)
        .collect();
    if !interactive {
        let default = if installed.is_empty() {
            "none".to_string()
        } else {
            installed.join(", ")
        };
        println!("{question}");
        println!("{}", ui::dim(explain));
        println!("{}", ui::dim(&format!("[no tty — default: {default}]")));
        println!();
        return Some(installed);
    }
    let _ = cliclack::log::remark(ui::dim(explain));
    let mut prompt = cliclack::multiselect(question)
        .initial_values(installed.clone())
        .required(false);
    for name in persist::names() {
        let hint = if installed.iter().any(|i| i == name) {
            "installed"
        } else {
            "not installed"
        };
        prompt = prompt.item(name.to_string(), name, hint);
    }
    prompt.interact().ok()
}

fn start_hint(tools: &[String]) -> String {
    let start = tools.first().map_or("dense claude", String::as_str);
    format!(
        "Run `{}` to start saving, or `{}` for help.",
        ui::cyan(start),
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

    let Some(tools) = pick_tools(cfg, interactive) else {
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
    if !tools.is_empty() {
        let report = persist::install_shims(cfg, &tools)?;
        for warning in &report.warnings {
            warn(interactive, warning);
        }
    }
    for warning in path_warnings(cfg, &tools, &wiring) {
        warn(interactive, &warning);
    }

    let later = format!(
        "Change this anytime: `{}` / `{}`.",
        ui::cyan("dense persist <tool>"),
        ui::cyan("dense unpersist <tool>")
    );
    if interactive {
        let _ = cliclack::log::remark(ui::dim(&later));
        let _ = cliclack::outro(start_hint(&tools));
    } else {
        println!("\n{}\n{}", ui::dim(&later), start_hint(&tools));
    }
    Ok(())
}
