//! `dense setup` — the first-run wizard the installer hands off to. Asks
//! whether to route claude through condense and whether to wire PATH, then
//! tells the user how to start. Run via `curl … | sh`, the installer
//! reconnects stdin to the tty so the prompts work; with no tty it explains
//! and uses defaults.

use std::io::IsTerminal;
use std::path::Path;

use crate::config::Config;
use crate::error::Error;
use crate::{Result, doctor, env_file, harness, persist, tool, ui};

/// Coding agents `dense` can launch through its harness. Only those with a
/// harness run path belong here; setup offers the ones actually installed.
const LAUNCH_AGENTS: &[&str] = &["claude"];

/// Why we're offering to launch an agent at the end of setup: to repair
/// critical wiring issues, or just to start using the tool now.
#[derive(Clone, Copy)]
enum LaunchKind {
    Fix,
    Now,
}

pub async fn run(cfg: &Config) -> Result<()> {
    let interactive = std::io::stdin().is_terminal();
    let res = wizard(cfg, interactive);
    if res.is_err() {
        if interactive {
            let _ = cliclack::outro_cancel(ui::yellow("setup did not finish."));
        }
        return res;
    }
    offer_launch(cfg, interactive).await
}

fn available_agents(cfg: &Config) -> Vec<&'static str> {
    LAUNCH_AGENTS
        .iter()
        .copied()
        .filter(|name| tool::resolve_real(cfg, name).is_ok())
        .collect()
}

/// Ask which installed agent (if any) to launch. One agent is a yes/no; several
/// is a pick-or-skip select. The wording follows `kind`. `None` = leave it.
fn choose_agent(agents: &[&'static str], kind: LaunchKind) -> Option<&'static str> {
    match agents {
        [] => None,
        [only] => {
            let question = match kind {
                LaunchKind::Fix => format!("Launch {only} to fix the issues above?"),
                LaunchKind::Now => format!("Launch {only} now?"),
            };
            cliclack::confirm(question)
                .initial_value(true)
                .interact()
                .ok()
                .filter(|yes| *yes)
                .map(|_| *only)
        }
        many => {
            let prompt = match kind {
                LaunchKind::Fix => "Launch an agent to fix the issues above?",
                LaunchKind::Now => "Launch an agent now?",
            };
            let mut select = cliclack::select(prompt).item(None, "skip", "");
            for name in many {
                select = select.item(Some(*name), *name, "");
            }
            select.interact().ok().flatten()
        }
    }
}

/// Seed an agent with the critical checks and the commands that repair them, so
/// it can pick up the fix without the user restating anything.
fn fix_prompt(issues: &[&doctor::Check]) -> String {
    let lines: Vec<String> = issues
        .iter()
        .map(|c| {
            if c.detail.is_empty() {
                format!("- {}", c.label)
            } else {
                format!("- {} ({})", c.label, c.detail)
            }
        })
        .collect();
    format!(
        "My `dense` CLI install isn't fully wired. `dense doctor` reports these \
         issues:\n{}\n\ndense routes Claude Code through the condense proxy. \
         Relevant commands: `dense doctor` (re-check), `dense persist` (install \
         PATH shims), `dense login` (authenticate). Please diagnose and fix the \
         wiring, then run `dense doctor` to confirm everything passes.",
        lines.join("\n")
    )
}

async fn launch_agent(cfg: &Config, name: &str, args: &[String]) -> Result<()> {
    match name {
        "claude" => harness::claude::run(cfg, args).await,
        other => Err(Error::msg(format!("don't know how to launch {other}"))),
    }
}

/// Close setup by running `dense doctor`. With critical wiring issues, offer to
/// launch an installed agent (seeded with them) to fix; otherwise just offer to
/// launch one now. Non-interactive runs only print the report.
async fn offer_launch(cfg: &Config, interactive: bool) -> Result<()> {
    println!();
    let checks = doctor::diagnose(cfg).await;
    doctor::print_report(cfg, &checks);
    if !interactive {
        return Ok(());
    }
    let agents = available_agents(cfg);
    if agents.is_empty() {
        return Ok(());
    }

    let critical: Vec<&doctor::Check> = checks.iter().filter(|c| c.is_critical()).collect();
    let kind = if critical.is_empty() {
        LaunchKind::Now
    } else {
        LaunchKind::Fix
    };
    let Some(agent) = choose_agent(&agents, kind) else {
        return Ok(());
    };
    let args = match kind {
        LaunchKind::Fix => vec![fix_prompt(&critical)],
        LaunchKind::Now => Vec::new(),
    };
    launch_agent(cfg, agent, &args).await
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
fn path_warnings(cfg: &Config, persisted: bool, wiring: &env_file::PathWiring) -> Vec<String> {
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
    } else if persisted && !on_path(&cfg.shim_dir()) {
        out.push(format!("{hint} so `claude` routes through dense."));
    }
    out
}

fn start_hint(persisted: bool) -> String {
    let start = if persisted { "claude" } else { "dense claude" };
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

    let Some(do_persist) = ask(
        interactive,
        "Use condense for all claude sessions?",
        "the bare `claude` command will point at the dense claude wrapper.",
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
    if do_persist {
        let report = persist::install_shims(cfg, &["claude".to_string()])?;
        for warning in &report.warnings {
            warn(interactive, warning);
        }
    }
    for warning in path_warnings(cfg, do_persist, &wiring) {
        warn(interactive, &warning);
    }

    if interactive {
        let _ = cliclack::outro(start_hint(do_persist));
    } else {
        println!("\n{}", start_hint(do_persist));
    }
    Ok(())
}
