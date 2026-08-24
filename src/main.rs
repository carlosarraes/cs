mod claude;
mod commands;
mod config;
mod daemon;
mod policy;
mod snapshot;
mod state;
mod usage;
mod util;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "cs", version, about = "Switch between Claude Code accounts")]
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
        /// Snapshot the already-logged-in account instead of running a login
        #[arg(long)]
        current: bool,
        /// Overwrite the alias if it already exists
        #[arg(short, long)]
        force: bool,
    },
    /// Switch to an alias (`-` = previous account, `next` = least-used 5h account)
    Switch {
        /// Alias to switch to, `-` for the previous account, or `next` for the
        /// least-used (5h) account
        alias: String,
        /// Switch even if Claude Code is running, without prompting
        #[arg(short, long)]
        yes: bool,
    },
    /// Delete a saved alias (does not log out)
    Del {
        /// Alias to delete
        alias: String,
    },
    /// List saved aliases
    List,
    /// Show the live signed-in account
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
    /// Save the currently-open Claude sessions so they can be reopened after a
    /// reboot (subcommands manage saved snapshots)
    Snapshot {
        #[command(subcommand)]
        command: Option<SnapshotCommands>,
    },
}

#[derive(Subcommand)]
enum SnapshotCommands {
    /// List snapshots, or the sessions inside one
    List {
        /// Snapshot id to show in detail
        id: Option<String>,
    },
    /// Recreate a snapshot's sessions in tmux (newest snapshot by default)
    Restore {
        /// Snapshot id (defaults to the newest)
        id: Option<String>,
    },
    /// Delete a snapshot
    Del {
        /// Snapshot id
        id: String,
    },
}

#[derive(Subcommand)]
enum DaemonCommands {
    /// Write a systemd user unit (Linux) / launchd agent (macOS) that runs `cs start` at login
    Install,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Add {
            alias,
            current,
            force,
        } => commands::add(&alias, current, force),
        Commands::Switch { alias, yes } => commands::switch(&alias, yes),
        Commands::Del { alias } => commands::del(&alias),
        Commands::List => commands::list(),
        Commands::Whoami => commands::whoami(),
        Commands::Usage { live } => commands::usage(live),
        Commands::Start => daemon::run(),
        Commands::Logs { follow } => daemon::logs(follow),
        Commands::Daemon {
            command: DaemonCommands::Install,
        } => daemon::install(),
        Commands::Snapshot { command } => match command {
            None => snapshot::take(),
            Some(SnapshotCommands::List { id }) => snapshot::list(id.as_deref()),
            Some(SnapshotCommands::Restore { id }) => snapshot::restore(id.as_deref()),
            Some(SnapshotCommands::Del { id }) => snapshot::del(&id),
        },
    }
}
