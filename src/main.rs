//! The `dense` CLI. Parses the command line, dispatches to the modules that do
//! the work, and renders any error via color-eyre.

mod api;
mod cli;
mod config;
mod doctor;
mod env_file;
mod error;
mod harness;
mod hosts;
mod persist;
mod profile;
mod selfupdate;
mod setup;
mod tool;
mod ui;

pub(crate) use error::Result;

use clap::Parser;
use color_eyre::Result as EyreResult;
use tracing_subscriber::EnvFilter;

use api::auth;
use cli::{Cli, Command, SelfCommand};
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
    color_eyre::install()?;
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let cfg = Config::resolve(cli.url.clone(), cli.environment.clone())?;

    let result: Result<()> = match cli.command {
        Command::Status => {
            status(&cfg);
            Ok(())
        }
        Command::Codex { .. } => {
            println!("Codex support is coming soon.");
            Ok(())
        }
        Command::Login => auth::login(&cfg).await,
        Command::Logout => auth::logout(&cfg),
        Command::Claude { args } => harness::claude::run(&cfg, &args).await,
        Command::Persist {
            targets,
            no_modify_path,
        } => persist::persist(&cfg, &targets, !no_modify_path),
        Command::Unpersist { targets } => persist::unpersist(&cfg, &targets),
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
        Command::SelfCmd(SelfCommand::Uninstall) => selfupdate::uninstall(&cfg),
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
}
