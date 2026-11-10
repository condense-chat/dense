//! Command-line surface.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "dense",
    version,
    about = "Use your coding tools through Condense",
    after_help = "Start here:\n  dense setup                   Set up login and your tools\n  dense claude                  Try one Claude Code session\n  dense claude-desktop install   Connect Claude Desktop\n  dense status                  See what's configured\n\nLearn more: dense <command> --help",
    styles = help_styles()
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Target a known environment: prod, stage, dev, or a bare zone
    /// (e.g. dev-foo.condense.localhost). Lower precedence than --url.
    #[arg(short = 'e', long = "env", global = true, value_name = "ENV")]
    pub environment: Option<String>,

    /// Condense api base URL (overrides --env and the default). $CONDENSE_URL
    /// sets it too, unless --env is given.
    #[arg(long, global = true)]
    pub url: Option<String>,

    /// Increase log verbosity (-v debug, -vv trace).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run Claude Code through Condense. All arguments pass through.
    #[command(disable_help_flag = true)]
    Claude {
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "ARGS"
        )]
        args: Vec<String>,
    },
    /// Set up or check Condense for Claude Desktop's local Code sessions.
    #[command(subcommand)]
    ClaudeDesktop(DesktopCommand),
    /// Run Codex through Condense. All arguments pass through.
    #[command(disable_help_flag = true)]
    Codex {
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "ARGS"
        )]
        args: Vec<String>,
    },
    /// Background service: runs dense's jobs on this machine.
    #[command(subcommand, hide = true)]
    Daemon(DaemonCommand),
    /// Check your installation and see how to fix any issues.
    Doctor,
    /// See your account, savings, and optional session details.
    Info {
        /// Show savings as emoji bars.
        #[arg(long, conflicts_with_all = ["json", "matrix"])]
        bar: bool,
        /// Include exact token counts and account and session IDs.
        #[arg(long, conflicts_with_all = ["bar", "json", "matrix"])]
        details: bool,
        /// Print JSON instead of the summary.
        #[arg(long)]
        json: bool,
        /// Show savings as a detailed emoji grid.
        #[arg(long, conflicts_with = "json")]
        matrix: bool,
        /// Defaults to $CONDENSE_SESSION_ID, which `dense <tool>` sets for its child.
        #[arg(value_name = "SESSION_ID")]
        session: Option<String>,
    },
    /// Log in to Condense on this computer.
    Login,
    /// Log out of Condense on this computer.
    Logout,

    /// Run OpenCode through Condense. All arguments pass through.
    #[command(disable_help_flag = true)]
    Opencode {
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "ARGS"
        )]
        args: Vec<String>,
    },
    /// Use Condense whenever you run a tool (default: all supported tools).
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
    /// Set up login and choose which tools use Condense.
    Setup,
    /// See your login and how each tool uses Condense.
    Status {
        /// Include server, file paths, and background service diagnostics.
        #[arg(long)]
        details: bool,
    },
    /// Claude Code status line for the launch footer (hidden).
    #[command(hide = true)]
    Statusline {
        #[arg(allow_hyphen_values = true)]
        line: String,
    },
    /// Stop automatic routing for tools (default: all supported tools).
    Unpersist {
        #[arg(value_name = "TOOL")]
        targets: Vec<String>,
    },
    /// Compare Claude plan usage with and without Condense.
    Usage {
        /// Exclude untagged Claude-harness usage from the estimate.
        #[arg(long)]
        attributed_only: bool,
        /// Print JSON instead of the summary.
        #[arg(long)]
        json: bool,
        #[arg(value_name = "SUB", default_value = "claude", value_parser = ["claude", "codex"])]
        sub: String,
    },
}

#[derive(Subcommand)]
pub enum DaemonCommand {
    /// Start the daemon at login (launchd / systemd user unit / Startup folder).
    Install,
    /// Run the daemon loop in the foreground.
    Run {
        /// Run every job once and exit.
        #[arg(long)]
        once: bool,
    },
    /// Remove the autostart entry.
    Uninstall,
}

#[derive(Subcommand)]
pub enum DesktopCommand {
    /// Connect local Code sessions and automatically follow Desktop updates.
    Install {
        #[arg(long, hide = true, value_name = "DIR")]
        data_dir: Option<PathBuf>,
        /// Connect installed engines only; don't enable automatic updates.
        #[arg(long)]
        no_watch: bool,
    },
    /// See connection status, automatic updates, and supported sessions.
    Status {
        #[arg(long, hide = true, value_name = "DIR")]
        data_dir: Option<PathBuf>,
    },
    /// Disconnect Condense and restore Desktop's original engines.
    Uninstall {
        #[arg(long, hide = true, value_name = "DIR")]
        data_dir: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
pub enum SelfCommand {
    /// Remove dense, its shims, and PATH entries.
    Uninstall,
    /// Update dense to the latest release.
    Update,
}

fn help_styles() -> clap::builder::Styles {
    use clap::builder::styling::AnsiColor;
    clap::builder::Styles::styled()
        .header(AnsiColor::Cyan.on_default().bold())
        .usage(AnsiColor::Cyan.on_default().bold())
        .literal(AnsiColor::Green.on_default())
        .placeholder(AnsiColor::Cyan.on_default())
}
