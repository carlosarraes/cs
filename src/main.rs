mod claude;
mod codex;
mod commands;
mod config;
mod daemon;
mod policy;
mod provider;
mod state;
mod usage;
mod util;

use anyhow::Result;
use clap::{Parser, Subcommand};
use provider::Provider;

#[derive(Parser)]
#[command(
    name = "cs",
    version,
    about = "Switch between Claude Code and Codex accounts"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Authenticate a new account and save it under an alias
    Add {
        /// Name to save the account as
        alias: String,
        /// Target Codex instead of Claude Code
        #[arg(long)]
        codex: bool,
        /// Snapshot the already-logged-in account instead of running a login
        #[arg(long)]
        current: bool,
        /// Overwrite the alias if it already exists
        #[arg(short, long)]
        force: bool,
        /// Use Codex's device-code login (for SSH / headless machines)
        #[arg(long)]
        device_auth: bool,
    },
    /// Switch to an alias (`-` = previous account, `next` = least-used 5h account)
    Switch {
        /// Alias to switch to, `-` for the previous account, or `next` for the
        /// least-used (5h) Claude account
        alias: String,
        /// Target Codex instead of Claude Code
        #[arg(long)]
        codex: bool,
        /// Switch even if Claude Code is running, without prompting
        #[arg(short, long)]
        yes: bool,
    },
    /// Delete a saved alias (does not log out)
    Del {
        /// Alias to delete
        alias: String,
        /// Target Codex instead of Claude Code
        #[arg(long)]
        codex: bool,
    },
    /// List saved aliases
    List {
        /// List Codex aliases instead of Claude Code
        #[arg(long)]
        codex: bool,
    },
    /// Show the live account signed in on Claude Code and Codex
    Whoami,
    /// Show every Claude account's 5h/7d usage (from the daemon's last poll)
    Usage {
        /// Poll the API right now instead of reading the daemon's last observation
        #[arg(long)]
        live: bool,
    },
    /// Run the auto-switcher in the foreground (Claude only)
    Start,
    /// Print the auto-switcher log
    Logs {
        /// Keep printing as new lines arrive
        #[arg(short, long)]
        follow: bool,
    },
    /// Manage the background service
    Daemon {
        #[command(subcommand)]
        command: DaemonCommands,
    },
}

#[derive(Subcommand)]
enum DaemonCommands {
    /// Write a systemd user unit (Linux) / launchd agent (macOS) that runs `cs start` at login
    Install,
}

fn provider_of(codex: bool) -> Provider {
    if codex {
        Provider::Codex
    } else {
        Provider::Claude
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Add {
            alias,
            codex,
            current,
            force,
            device_auth,
        } => commands::add(provider_of(codex), &alias, current, force, device_auth),
        Commands::Switch { alias, codex, yes } => commands::switch(provider_of(codex), &alias, yes),
        Commands::Del { alias, codex } => commands::del(provider_of(codex), &alias),
        Commands::List { codex } => commands::list(provider_of(codex)),
        Commands::Whoami => commands::whoami(),
        Commands::Usage { live } => commands::usage(live),
        Commands::Start => daemon::run(),
        Commands::Logs { follow } => daemon::logs(follow),
        Commands::Daemon {
            command: DaemonCommands::Install,
        } => daemon::install(),
    }
}
