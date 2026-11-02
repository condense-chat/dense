//! The `dense` CLI. Parses the command line, dispatches to the modules that do
//! the work, and renders any error via color-eyre.

mod api;
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
mod tool;
mod ui;
mod usage;

pub(crate) use error::Result;

use clap::Parser;
use color_eyre::Result as EyreResult;
use tracing_subscriber::EnvFilter;

use api::auth;
use cli::{Cli, Command, DaemonCommand, DesktopCommand, SelfCommand};
use config::Config;

fn init_tracing(verbose: u8) {
    let level = match verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("dense={level}")));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> EyreResult<()> {
    if let Some(real) = desktop::engine::sibling_engine() {
        return desktop::engine::run(&real).await.map_err(Into::into);
    }
    color_eyre::config::HookBuilder::default()
        .display_env_section(false)
        .display_location_section(false)
        .install()?;
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let url = Config::cli_url(cli.url.clone(), cli.environment.as_deref());
    let cfg = Config::resolve(url, cli.environment.clone())?;
    if let Err(e) = migrate::run(&cfg) {
        eprintln!("warning: could not finish moving dense files to their new dirs: {e}");
    }

    let result: Result<()> = match cli.command {
        Command::Status => {
            status(&cfg);
            Ok(())
        }
        Command::Daemon(DaemonCommand::Install) => {
            daemon::install(&cfg).and_then(|()| desktop::keep_daemon(&cfg))
        }
        Command::Daemon(DaemonCommand::Run { once }) => daemon::run(&cfg, once).await,
        Command::Daemon(DaemonCommand::Uninstall) => daemon::uninstall(&cfg).await,
        Command::Codex { args } => harness::codex::run(&cfg, &args).await,
        Command::Login => match auth::login(&cfg).await {
            Ok(()) => {
                daemon::managed::after_login(&cfg).await;
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
        } => persist::persist(&cfg, &targets, !no_modify_path),
        Command::Unpersist { targets } => persist::unpersist(&cfg, &targets),
        Command::Usage {
            attributed_only,
            json,
            sub,
        } => usage::run(&cfg, &sub, json, attributed_only).await,
        Command::ClaudeDesktop(command) => match command {
            DesktopCommand::Install { data_dir, no_watch } => {
                desktop::install(&cfg, data_dir, !no_watch)
            }
            DesktopCommand::Status { data_dir } => desktop::status(&cfg, data_dir),
            DesktopCommand::Uninstall { data_dir } => desktop::uninstall(&cfg, data_dir),
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
            json,
            matrix,
            session,
        } => {
            let layout = match (bar, matrix) {
                (true, _) => Some(info::Style::Bar),
                (_, true) => Some(info::Style::Matrix),
                _ => None,
            };
            info::run(&cfg, json, layout, session.as_deref()).await
        }
    };
    result.map_err(Into::into)
}

fn presence(found: bool) -> &'static str {
    if found { "present" } else { "missing" }
}

fn status(cfg: &Config) {
    let creds = auth::load_creds(cfg);
    let mode = auth::resolve_mode(&cfg.api_host, cfg.auth_required);
    println!("profile: {}", cfg.profile());
    println!("api:     {}", cfg.api_base_url);
    println!("host:    {} ({mode:?})", cfg.api_host);
    println!("creds:   {}", cfg.cred_dir().display());
    println!("token:   {}", presence(creds.token.is_some()));
    println!("user:    {}", presence(creds.user_id.is_some()));
    for (label, value) in daemon::status_lines(cfg) {
        println!("{:<9}{value}", format!("{label}:"));
    }
}
