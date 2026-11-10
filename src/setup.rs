//! `dense setup` — the first-run wizard the installer hands off to. Signs in
//! when no creds exist, asks which tools and whether Claude Desktop to route
//! through condense and whether to wire PATH, then tells the user how to
//! start. Run via `curl … | sh`, the installer
//! reconnects stdin to the tty so the prompts work; with no tty it explains
//! and uses defaults.

use std::path::Path;

use crate::api::auth;
use crate::config::Config;
use crate::{Result, daemon, desktop, env_file, persist, tool, ui};

pub async fn run(cfg: &Config) -> Result<()> {
    let interactive = ui::interactive();
    if !auth::load_creds(cfg).is_authenticated() {
        auth::login(cfg).await?;
    }
    cfg.remember_profile()?;
    if daemon::managed::after_login(cfg).await && cfg!(feature = "managed-setup") {
        let wiring = env_file::ensure_env(cfg, persist::may_modify_path(cfg))?;
        persist::report_wiring(cfg, &wiring);
        ui::success("Your organization manages this device. There is nothing else to set up");
        return Ok(());
    }
    let res = wizard(cfg, interactive);
    if res.is_err() && interactive {
        ui::warning("Setup did not finish. Run `dense setup` to try again.");
    }
    res
}

/// Ask a yes/no question with a dim one-line explainer. Interactive: a
/// cliclack confirm on the tty (`None` = cancelled); otherwise echo the
/// default taken.
fn ask(interactive: bool, question: &str, explain: &str, default_yes: bool) -> Option<bool> {
    if !interactive {
        let default = if default_yes { "yes" } else { "no" };
        ui::note(question);
        ui::detail(explain);
        ui::detail(format!("No interactive terminal. Using default: {default}"));
        return Some(default_yes);
    }
    ui::stderr_detail(explain);
    ui::stderr_detail("y / n to choose; Enter keeps the default");
    cliclack::confirm(question)
        .initial_value(default_yes)
        .interact()
        .ok()
}

/// Desktop is its own choice: routing it wraps Desktop's engine and keeps a
/// background service following Desktop, even before it's installed.
fn ask_desktop(interactive: bool) -> Option<bool> {
    let explain = if desktop::detected() {
        "Covers Desktop's Code sessions on this computer, including engines Desktop \
         downloads later. Undo with `dense claude-desktop uninstall`."
    } else {
        "Desktop hasn't run on this computer yet; a background service routes its Code \
         sessions from the first one. Undo with `dense claude-desktop uninstall`."
    };
    ask(
        interactive,
        "Route Claude Desktop through condense?",
        explain,
        false,
    )
}

fn cancelled(interactive: bool) -> Result<()> {
    if interactive {
        ui::hint("Try again", "dense setup");
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
    let question = "Which tools should always go through condense?";
    let explain = "Selected tools use Condense whenever you run them (for example, `claude`). Unchecked tools still work with `dense <tool>`.";
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
        ui::note(question);
        ui::detail(explain);
        ui::detail(format!("No interactive terminal. Using default: {default}"));
        return Some(installed);
    }
    ui::stderr_detail(explain);
    ui::stderr_detail("Arrow keys to move; Space to select; Enter to confirm");
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

/// What got persisted and how to flip it later.
fn persist_summary(tools: &[String]) -> String {
    let persist = ui::cyan("dense persist <tool>");
    let unpersist = ui::cyan("dense unpersist <tool>");
    if tools.is_empty() {
        return format!(
            "Automatic routing is off. Tools use Condense only when run as `{}`.\n\
             Turn it on later with `{persist}`.",
            ui::cyan("dense <tool>")
        );
    }
    format!(
        "Automatic routing: {}. These commands now use Condense.\n\
         Add more with `{persist}`; turn it off with `{unpersist}`.",
        ui::bold(&tools.join(", "))
    )
}

fn start_hint(tools: &[String]) -> String {
    let start = tools.first().map_or("dense <tool>", String::as_str);
    format!(
        "Run `{}` to start saving, or `{}` for help.",
        ui::cyan(start),
        ui::cyan("dense -h")
    )
}

fn wizard(cfg: &Config, interactive: bool) -> Result<()> {
    ui::title("Setup");
    ui::field("Source", env!("CARGO_PKG_REPOSITORY"));

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

    let Some(route_desktop) = ask_desktop(interactive) else {
        return cancelled(interactive);
    };

    let wiring = env_file::ensure_env(cfg, modify_path)?;
    persist::remember_path_choice(cfg, modify_path)?;
    if let env_file::PathWiring::Manual(notes) = &wiring {
        ui::warning(notes.join("\n"));
    }
    if !tools.is_empty() {
        let mut report = persist::install_shims(cfg, &tools)?;
        persist::wire_claude(cfg, &mut report)?;
        for settings in &report.editors {
            ui::note("Claude Code in your editor is set up to use Condense");
            ui::detail(format!("Settings: {}", settings.display()));
        }
        if let Some(entry) = &report.daemon {
            ui::field(
                "Editor updates",
                "Enabled; settings are repaired automatically",
            );
            ui::detail(format!("Service file: {}", entry.display()));
        }
        for warning in &report.warnings {
            ui::warning(warning);
        }
    }
    for warning in path_warnings(cfg, &tools, &wiring) {
        ui::warning(warning);
    }
    if route_desktop {
        match desktop::route(cfg, None, true) {
            Ok(report) => {
                for warning in &report.warnings {
                    ui::warning(warning);
                }
                ui::note(&report.summary);
                for (label, value) in &report.fields {
                    ui::field(label, value);
                }
                desktop::coverage(0);
            }
            Err(e) => ui::warning(format!("Claude Desktop not routed: {e}")),
        }
    }

    ui::success("Setup complete");
    ui::note(persist_summary(&tools));
    ui::note(start_hint(&tools));
    Ok(())
}
