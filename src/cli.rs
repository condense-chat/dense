//! Command-line surface.

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "dense", version, about = "Durable condense CLI")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Target a known environment: prod, stage, dev, or a bare zone
    /// (e.g. dev-foo.condense.localhost). Lower precedence than --url.
    #[arg(short = 'e', long = "env", global = true, value_name = "ENV")]
    pub environment: Option<String>,

    /// Condense api base URL (overrides --env, $CONDENSE_URL, and the default).
    #[arg(long, global = true, env = "CONDENSE_URL")]
    pub url: Option<String>,

    /// Increase log verbosity (-v debug, -vv trace).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run Claude Code through the condense proxy (args pass straight through).
    #[command(disable_help_flag = true)]
    Claude {
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "ARGS"
        )]
        args: Vec<String>,
    },
    /// Launch Codex routed through condense.
    #[command(disable_help_flag = true)]
    Codex {
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "ARGS"
        )]
        args: Vec<String>,
    },
    /// Verify the install is wired correctly.
    Doctor,
    /// Authenticate this machine (device-flow or register).
    Login,
    /// Clear stored credentials.
    Logout,

    #[command(disable_help_flag = true)]
    Opencode {
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "ARGS"
        )]
        args: Vec<String>,
    },
    /// Install shims so the named tools route through dense (no args: all).
    Persist {
        /// Write the env file but don't edit shell profiles.
        #[arg(long)]
        no_modify_path: bool,
        #[arg(value_name = "TOOL")]
        targets: Vec<String>,
    },
    /// Manage environment profiles (hidden; `prod` resets to default).
    #[command(hide = true)]
    Profile {
        /// List registered profiles.
        #[arg(short = 'l', long = "list")]
        list: bool,
        #[arg(value_name = "NAME")]
        name: Option<String>,
        /// Register the profile from a zone (stage.condense.chat) or full api URL.
        #[arg(long, value_name = "ZONE_OR_URL")]
        url: Option<String>,
    },
    /// Manage the dense binary itself.
    #[command(subcommand, name = "self")]
    SelfCmd(SelfCommand),
    /// First-run setup wizard (invoked by the installer).
    Setup,
    /// Show identity, api host, and persisted tools.
    Status,
    /// Remove shims for the named tools (no args: all).
    Unpersist {
        #[arg(value_name = "TOOL")]
        targets: Vec<String>,
    },
}

#[derive(Subcommand)]
pub enum SelfCommand {
    /// Remove dense, its shims, and PATH entries.
    Uninstall,
    /// Update dense to the latest release.
    Update,
}
