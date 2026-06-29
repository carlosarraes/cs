mod claude;
mod commands;
mod state;

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
        /// Snapshot the already-logged-in account instead of running `claude auth login`
        #[arg(long)]
        current: bool,
        /// Overwrite the alias if it already exists
        #[arg(short, long)]
        force: bool,
    },
    /// Switch to an alias (use `-` for the previous account)
    Switch {
        /// Alias to switch to, or `-` for the previous account
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
    }
}
