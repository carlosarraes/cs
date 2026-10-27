mod accounts;
mod claude;
mod commands;
mod config;
mod conversation;
mod daemon;
mod launch;
mod native;
mod policy;
mod proxy;
mod routing;
mod session_control;
mod snapshot;
mod state;
mod usage;
mod util;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "cs",
    version,
    about = "Route Claude sessions across accounts and manage account logins",
    arg_required_else_help = true,
    args_conflicts_with_subcommands = true,
    after_help = "Agent guide: cs --llm
Examples: cs run -- --resume pm
          cs switch work --from personal
          cs switch work --session <UUID> --interrupt"
)]
struct Cli {
    /// Print a compact agent guide with command examples and routing rules
    #[arg(long)]
    llm: bool,
    #[command(subcommand)]
    command: Option<Commands>,
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
    /// Change the shared login, or move selected conversations to a destination account
    #[command(after_help = "Targeted: cs switch DEST --from SOURCE
          cs switch DEST --session UUID [--interrupt]
Get UUIDs with cs sessions in another terminal; no restart needed.
Without --interrupt, queue a restart after Claude's idle notification, typically
about 60 seconds after its response with no background agents. Use --interrupt
for sessions waiting for a quota reset. A restart resumes the same conversation.
Only updated cs run launchers support targeted switching. Check cs sessions
for pending moves or failures. Without a target, switch changes the shared login.")]
    Switch {
        /// Destination alias or, for targeted moves, email; plain switch also accepts - and next
        alias: String,
        /// Move live managed conversations from this account to the destination
        #[arg(long, conflicts_with = "session", group = "target")]
        from: Option<String>,
        /// Move only this live conversation (UUID from cs sessions)
        #[arg(long, conflicts_with = "from", group = "target")]
        session: Option<String>,
        /// Interrupt selected conversations instead of waiting for Claude to become idle
        #[arg(long, requires = "target")]
        interrupt: bool,
        /// Switch even if Claude Code is running, without prompting
        #[arg(short, long, conflicts_with_all = ["from", "session"])]
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
    /// Update a saved alias with the live credentials (after a `/login` outside cs)
    Refresh,
    /// Show account session counts and cached 5h/7d usage; --live fetches now
    Usage {
        /// Poll the API right now instead of reading the router's last observation
        #[arg(long)]
        live: bool,
    },
    /// Poll/report account usage and serve existing proxy sessions
    Start,
    /// Launch Claude with an automatically selected native account
    Run {
        /// Pin to an alias or unique email; fail if unavailable, never fall back
        #[arg(long)]
        account: Option<String>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<std::ffi::OsString>,
    },
    #[command(hide = true)]
    NativeEvent,
    #[command(hide = true)]
    NativeMaintain { alias: String, email: String },
    /// Show live conversation UUIDs, accounts, and pending switches or failures
    Sessions,
    /// Print the session router log
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
    if cli.llm {
        print!("{}", include_str!("llm.md"));
        return Ok(());
    }
    let Some(command) = cli.command else {
        unreachable!("clap requires a command or --llm");
    };
    match command {
        Commands::Add {
            alias,
            current,
            force,
        } => commands::add(&alias, current, force),
        Commands::Switch {
            alias,
            from,
            session,
            interrupt,
            yes,
        } => {
            if from.is_some() || session.is_some() {
                launch::switch_targeted(&alias, from.as_deref(), session.as_deref(), interrupt)
            } else {
                commands::switch(&alias, yes)
            }
        }
        Commands::Del { alias } => commands::del(&alias),
        Commands::List => commands::list(),
        Commands::Whoami => commands::whoami(),
        Commands::Refresh => commands::refresh(),
        Commands::Usage { live } => commands::usage(live),
        Commands::Start => proxy::run(),
        Commands::Run { account, args } => launch::run(args, account.as_deref()),
        Commands::Sessions => launch::sessions(),
        Commands::NativeEvent => launch::native_event(),
        Commands::NativeMaintain { alias, email } => native::maintain_account(&alias, &email),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_guide_is_standalone_and_preserves_claude_flag_passthrough() {
        let cli = Cli::try_parse_from(["cs", "--llm"]).unwrap();
        assert!(cli.llm && cli.command.is_none());
        assert!(Cli::try_parse_from(["cs"]).is_err());
        assert!(Cli::try_parse_from(["cs", "--llm", "switch", "work"]).is_err());
        assert!(Cli::try_parse_from(["cs", "switch", "work", "--llm"]).is_err());
        let cli = Cli::try_parse_from(["cs", "run", "--", "--llm"]).unwrap();
        let Some(Commands::Run { args, .. }) = cli.command else {
            panic!("expected run");
        };
        assert_eq!(args, [std::ffi::OsString::from("--llm")]);
    }

    #[test]
    fn targeted_switch_selectors_do_not_change_plain_switch_parsing() {
        let cli = Cli::try_parse_from(["cs", "switch", "b", "--from", "a"]).unwrap();
        let Some(Commands::Switch {
            alias,
            from,
            session,
            interrupt,
            yes,
        }) = cli.command
        else {
            panic!("expected switch")
        };
        assert_eq!(alias, "b");
        assert_eq!(from.as_deref(), Some("a"));
        assert!(session.is_none() && !yes && !interrupt);
        assert!(Cli::try_parse_from(["cs", "switch", "b", "--interrupt"]).is_err());
        let cli = Cli::try_parse_from(["cs", "switch", "b", "--yes"]).unwrap();
        let Some(Commands::Switch {
            from, session, yes, ..
        }) = cli.command
        else {
            panic!("expected switch")
        };
        assert!(from.is_none() && session.is_none() && yes);
        assert!(
            Cli::try_parse_from(["cs", "switch", "b", "--from", "a", "--session", "root"]).is_err()
        );
    }

    #[test]
    fn run_parses_account_before_claude_arguments() {
        let cli = Cli::try_parse_from([
            "cs",
            "run",
            "--account",
            "carlos@mondrio.io",
            "--",
            "claude",
            "-p",
            "say hi",
        ])
        .unwrap();
        let Some(Commands::Run { account, args }) = cli.command else {
            panic!("expected run")
        };
        assert_eq!(account.as_deref(), Some("carlos@mondrio.io"));
        assert_eq!(
            args,
            ["claude", "-p", "say hi"].map(std::ffi::OsString::from)
        );
        let cli = Cli::try_parse_from(["cs", "run", "--", "--chrome"]).unwrap();
        let Some(Commands::Run { account, args }) = cli.command else {
            panic!("expected run")
        };
        assert!(account.is_none());
        assert_eq!(args, [std::ffi::OsString::from("--chrome")]);
    }
}
