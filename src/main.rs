mod api;
mod atomic;
mod cli;
mod config;
mod daemon;
mod desktop;
mod device;
mod doctor;
mod env_file;
mod error;
mod harness;
mod hosts;
mod info;
mod migrate;
mod persist;
mod profile;
mod selfupdate;
mod setup;
mod status;
mod tool;
mod ui;
mod usage;

pub(crate) use error::Result;

use std::process::ExitCode;

use clap::{CommandFactory, FromArgMatches};
use tracing_subscriber::EnvFilter;

use api::auth;
use cli::{Cli, Command, DaemonCommand, DesktopCommand, SelfCommand};
use config::Config;

fn init_tracing(verbose: u8) {
    let level = match verbose {
        0 => "warn",
        1 => "debug",
        _ => "trace",
    };
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("dense={level}")));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(ui::color_enabled(true))
        .with_writer(std::io::stderr)
        .try_init();
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    ui::init();
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            ui::error(&error);
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    if let Some((real, args)) = desktop::engine::launch_target() {
        return desktop::engine::run(&real, &args).await;
    }
    let matches = Cli::command()
        .color(if ui::color_enabled(false) || ui::color_enabled(true) {
            clap::ColorChoice::Auto
        } else {
            clap::ColorChoice::Never
        })
        .get_matches();
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    if let Command::Statusline { line } = &cli.command {
        harness::claude::statusline(line);
        return Ok(());
    }
    init_tracing(cli.verbose);

    let url = Config::cli_url(cli.url.clone(), cli.environment.as_deref());
    let cfg = Config::resolve(url, cli.environment.clone())?;
    if let Err(e) = migrate::run(&cfg) {
        ui::warning(format!(
            "Could not finish moving Dense files to their new folders: {e}"
        ));
    }

    let result: Result<()> = match cli.command {
        Command::Status { details } => {
            status::run(&cfg, details);
            Ok(())
        }
        Command::Statusline { .. } => Ok(()),
        Command::Daemon(DaemonCommand::Install) => {
            daemon::install(&cfg).and_then(|()| desktop::keep_daemon(&cfg))
        }
        Command::Daemon(DaemonCommand::Run { once }) => daemon::run(&cfg, once).await,
        Command::Daemon(DaemonCommand::Uninstall) => daemon::uninstall(&cfg),
        Command::Codex { args } => harness::codex::run(&cfg, &args).await,
        Command::Login => match auth::login(&cfg).await {
            Ok(()) => {
                let _ = daemon::managed::after_login(&cfg).await;
                Ok(())
            }
            Err(e) => Err(e),
        },
        Command::Logout => auth::logout(&cfg).inspect(|()| daemon::managed::forget_switch(&cfg)),
        Command::Claude { args } => harness::claude::run(&cfg, &args).await,
        Command::Opencode { args } => harness::opencode::run(&cfg, &args).await,
        Command::Persist {
            targets,
            no_modify_path,
        } => persist::persist(&cfg, &targets, !no_modify_path)
            .inspect(|()| daemon::managed::note_forced(&cfg)),
        Command::Unpersist { targets } => {
            daemon::managed::refuse_if_managed(&cfg, "dense unpersist")
                .and_then(|()| persist::unpersist(&cfg, &targets))
        }
        Command::Usage {
            attributed_only,
            json,
            sub,
        } => usage::run(&cfg, &sub, json, attributed_only).await,
        Command::ClaudeDesktop(command) => match command {
            DesktopCommand::Install { data_dir, no_watch } => {
                desktop::install(&cfg, data_dir, !no_watch)
                    .inspect(|()| daemon::managed::note_forced(&cfg))
            }
            DesktopCommand::Status { data_dir } => desktop::status(&cfg, data_dir),
            DesktopCommand::Uninstall { data_dir } => {
                daemon::managed::refuse_if_managed(&cfg, "dense claude-desktop uninstall")
                    .and_then(|()| desktop::uninstall(&cfg, data_dir))
            }
        },
        Command::Doctor => doctor::run(&cfg).await,
        Command::Setup => setup::run(&cfg).await,
        Command::Profile { name, url, list } => {
            if list {
                profile::list(&cfg)
            } else if let Some(name) = name {
                profile::switch(&cfg, &name, url.as_deref()).await
            } else {
                profile::current(&cfg);
                Ok(())
            }
        }
        Command::SelfCmd(SelfCommand::Update) => selfupdate::update(&cfg).await,
        Command::SelfCmd(SelfCommand::Uninstall) => selfupdate::uninstall(&cfg).await,
        Command::Info {
            bar,
            details,
            json,
            matrix,
            session,
        } => {
            let layout = match (bar, matrix) {
                (true, _) => Some(info::Style::Bar),
                (_, true) => Some(info::Style::Matrix),
                _ => None,
            };
            info::run(&cfg, details, json, layout, session.as_deref()).await
        }
    };
    result
}
