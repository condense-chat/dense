//! `dense setup` — the first-run wizard the installer hands off to. Asks
//! whether to route claude through condense and whether to wire PATH, then
//! tells the user how to start. Run via `curl … | sh`, the installer
//! reconnects stdin to the tty so the prompts work; with no tty it explains
//! and uses defaults.

use std::io::IsTerminal;
use std::path::Path;

use crate::config::Config;
use crate::{Result, env_file, persist, ui};

pub async fn run(cfg: &Config) -> Result<()> {
    let interactive = std::io::stdin().is_terminal();
    cfg.remember_profile()?;

    if interactive {
        let _ = cliclack::intro(ui::cyan("condense setup"));
    }

    let do_persist = ask(
        interactive,
        "Use condense for all claude sessions?",
        "the bare `claude` command will point at the dense claude wrapper.",
        true,
    );

    let modify_path = ask(
        interactive,
        "Add dense to your PATH?",
        &format!("{}.", env_file::path_change_summary(cfg)),
        true,
    );

    env_file::ensure_env(cfg, modify_path)?;
    if do_persist {
        persist::install_shims(cfg, &["claude".to_string()])?;
    }

    if interactive {
        let _ = cliclack::outro(start_hint(do_persist));
    } else {
        println!("\n{}", start_hint(do_persist));
    }
    path_warnings(cfg, do_persist);
    Ok(())
}

/// Ask a yes/no question with a dim one-line explainer. Interactive: a
/// cliclack confirm on the tty; otherwise echo the default taken.
fn ask(interactive: bool, question: &str, explain: &str, default_yes: bool) -> bool {
    if !interactive {
        let default = if default_yes { "yes" } else { "no" };
        println!("{question}");
        println!("{}", ui::dim(explain));
        println!("{}", ui::dim(&format!("[no tty — default: {default}]")));
        println!();
        return default_yes;
    }
    let _ = cliclack::log::remark(ui::dim(explain));
    cliclack::confirm(question)
        .initial_value(default_yes)
        .interact()
        .unwrap_or(default_yes)
}

fn on_path(dir: &Path) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d == dir))
        .unwrap_or(false)
}

/// Warn when the wired dirs aren't visible to this shell yet.
fn path_warnings(cfg: &Config, persisted: bool) {
    let reload = env_file::reload_hint(cfg);
    if !on_path(&cfg.bin_dir()) {
        println!(
            "{}",
            ui::yellow(&format!(
                "{} isn't on your PATH yet; {reload}.",
                cfg.bin_dir().display()
            ))
        );
    } else if persisted && !on_path(&cfg.shim_dir()) {
        println!(
            "{}",
            ui::yellow(&format!("{reload} so `claude` routes through dense."))
        );
    }
}

fn start_hint(persisted: bool) -> String {
    let start = if persisted { "claude" } else { "dense claude" };
    format!(
        "Run `{}` to start saving, or `{}` for help.",
        ui::cyan(start),
        ui::cyan("dense -h")
    )
}
