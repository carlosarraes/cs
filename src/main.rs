mod claude;
mod codex;
mod commands;
mod provider;
mod state;
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
    },
    /// Switch to an alias (use `-` for the previous account)
    Switch {
        /// Alias to switch to, or `-` for the previous account
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
        } => commands::add(provider_of(codex), &alias, current, force),
        Commands::Switch { alias, codex, yes } => commands::switch(provider_of(codex), &alias, yes),
        Commands::Del { alias, codex } => commands::del(provider_of(codex), &alias),
        Commands::List { codex } => commands::list(provider_of(codex)),
        Commands::Whoami => commands::whoami(),
    }
}
