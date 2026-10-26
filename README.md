# cs

Route Claude Code conversations across subscription accounts, with quota-aware
placement and one-hour session affinity. Use `cs start` to run the router and
`cs run` to launch a managed conversation.

`cs` snapshots each account's credentials under an alias and restores them
instantly, so you can hop between personal/work/org accounts without
re-authenticating every time. It never implements OAuth itself: it delegates
login to `claude` and only ever swaps credentials on disk.

> Linux and macOS. OAuth subscription accounts only (for now).

## Install

```sh
curl -sSf https://raw.githubusercontent.com/carlosarraes/cs/main/install.sh | sh
```

Pin a version: `curl -sSf .../install.sh | VERSION=v0.0.1 sh`. The installer
verifies the binary against the release `SHA256SUMS` before installing to
`~/.local/bin` (override with `BIN_DIR=`).

From source: `cargo install --path .` or `just build`.

## Usage

```sh
cs add work              # run `claude auth login`, save the account as "work"
cs add main --current    # save the already-logged-in account as "main"
cs switch work           # switch to "work"
cs switch -              # switch back to the previous account (toggles)
cs list                  # list saved accounts (* = current, - = previous)
cs del work              # forget an alias (does not log you out)
cs whoami                # show the live signed-in account
cs refresh               # after a `/login` outside cs, update the saved credentials
cs switch next           # switch to the least-used (5h) account
cs usage [--live]        # every account's 5h/7d usage and live session counts

cs snapshot              # save the set of open Claude sessions (see below)
cs snapshot restore      # after a reboot: reopen them all in tmux
```

| Command | Description |
|---|---|
| `cs add <alias> [--current] [--force]` | Authenticate (or snapshot `--current`) and save under `<alias>` |
| `cs switch <alias>` / `cs switch -` | Activate an alias; `-` is the previous one |
| `cs del <alias>` | Remove a saved alias (live credentials untouched) |
| `cs list` | List saved aliases |
| `cs whoami` | Show the live signed-in account |
| `cs refresh` | Re-capture the live credentials into the alias they belong to |
| `cs switch next` | Activate the least-used (5h) account |
| `cs usage [--live]` | Show every account's 5h/7d usage (router's last poll, or `--live`) |
| `cs snapshot` | Save the currently-open Claude sessions (`list`/`restore`/`del` manage them) |
| `cs run [-- <claude args>]` | Launch a managed conversation |
| `cs sessions` | List live managed conversations and accounts |
| `cs start` | Run the session router in the foreground (see below) |
| `cs logs [-f]` | Print / follow the session router log |
| `cs daemon install` | Install a systemd user unit / launchd agent that runs `cs start` at login |
| `cs --version` | Print version |

If Claude Code is running when you switch, `cs` warns you first (pass `--yes`
to skip the prompt).

Claude forces a fresh login every 7 days, so sooner or later you will run
`/login` inside Claude Code rather than through `cs`. Run `cs refresh`
afterwards and the saved copy catches up; it finds the right alias by email, so
it cannot write one account's tokens into another account's alias, and it fixes
`current` if the live login drifted. The router reconciles newer live credentials while it runs. `cs` also refuses to overwrite an alias whose saved account differs from
the live login, so a mistaken login can never repoint an alias at someone else.

Usage windows are colored in interactive terminals: green below 70%, yellow
from 70% to below 90%, red from 90%, and bold red at 100% or above. Each window's
percentage and reset countdown share its color. Piped output stays plain;
`NO_COLOR=1` or `TERM=dumb` disables colors.

## Session routing

Run the local router in one terminal, then launch each managed conversation through it:

```sh
cs start
# In other terminals:
cs run
cs run -- --resume <conversation-id>
cs run -- --model sonnet
cs sessions
cs usage --live
```

`cs run` passes arguments to `claude` and preserves its settings, project files,
MCP configuration, and conversation history. It supplies a local proxy credential;
the upstream subscription token stays in `cs`. A running router is required.
Existing plain `claude` sessions keep using the shared login and are not adopted.

On a conversation's first request, the router excludes accounts without fresh
usage or usable credentials, and accounts at the configured quota ceilings.
It chooses the account with the fewest live managed conversations. Ties favor
more remaining weekly quota per hour until reset, then five-hour capacity per
hour until reset. Reset denominators have a 15-minute floor. This assumes
comparable subscription capacities; it does not predict how much a session
will consume.

The assignment stays for one hour after the conversation's last request.
Resuming within that period retains the account; after inactivity expiry,
selection runs again. In-flight requests keep their account even if the hour
expires. Internal subagent requests with the same root conversation ID share
its assignment. There is no automatic account failover: an unavailable assigned
account returns an error instead of silently changing credentials or replaying
an interrupted response.

`cs usage` appends `(N)` for live managed conversations per account. `*` still
marks the shared manual login, independently of those counts. Closed sessions
stop counting, but their recent assignments remain available for resume.
`cs sessions` lists each managed process, conversation, and account.

Configuration lives in `~/.config/cs/config.toml`:

```toml
[routing]
port = 18437                 # loopback only; 0 chooses a free port
poll_secs = 60
max_usage_age_secs = 300
five_hour_ceiling = 90       # admission limit; existing assignments stay
weekly_ceiling = 95
```

Restart `cs start` after changing configuration. The old `[auto-switcher]`,
`[report]`, and `[warmup]` sections are ignored with a migration notice. The
old global auto-switch loop has been removed. `cs switch <alias>` still changes
the shared login for unmanaged Claude instances; it cannot move managed sessions.

The service installer now starts the router:

```sh
cs daemon install            # writes the service and prints its enable command
cs logs -f
```

The router uses the existing saved accounts and polls their usage with backoff.
It refreshes expired non-current account tokens only when no unmanaged Claude
process is detected in the configured Claude session directory. It never
independently rotates the current or actually signed-in shared account's token;
it reconciles newer live credentials instead. If it cannot identify the live
login, automatic token refresh pauses. If Keychain access or login refresh is unavailable,
that account can become unavailable until you sign in and run `cs refresh`.
Independent credential managers outside `cs` cannot participate in its locks.

Assignments and process leases are stored alongside `state.json` in
`routing.json`, with owner-only permissions. PID plus native process-start
identity prevents dead or reused processes contributing to counts. The proxy
binds only to loopback and accepts credentials issued by `cs run`. Requests
larger than 32 MiB are rejected; streams have no fixed total timeout.

## Session snapshots

Working in tmux with several Claudes open and need to reboot? `cs snapshot`
records every *live* Claude Code session — its resume id, `/name`, working
directory, and where it sits in tmux (session, window, split pane), resolved
while everything is still running, so stale session files can't sneak in.

```sh
cs snapshot                  # → Snapshot '0823-2201' saved (5 sessions).
# ...reboot...
cs snapshot restore          # rebuilds the tmux sessions/windows/panes and
                             # types the appropriate resume command into each pane
cs snapshot list             # all snapshots (the newest is the restore default)
cs snapshot list 0823-2201   # the sessions inside one: name · id · tmux spot · cwd
```

Restore recreates missing tmux sessions and windows with their original names
(split panes come back tiled), skips sessions that are already running, and
falls back to printing the resume commands when tmux isn't
available. Managed snapshots restore with `cs run -- --resume`; older/unmanaged snapshots
keep using `claude --resume`. Start the router before restoring managed sessions.
Snapshots live in `~/.local/share/cs/snapshots.json`; the last 10
are kept.

## How it works

**Claude** stores a login in two pieces:

- the OAuth tokens (`claudeAiOauth`) — on Linux in `~/.claude/.credentials.json`,
  on macOS in the login Keychain (service `Claude Code-credentials`)
- the account identity (`oauthAccount`) — in `~/.claude.json` on both platforms

A switch patches **only** those keys, so your MCP tokens, project history,
and settings are never touched. `cs add` captures the account into
`~/.local/share/cs/state.json` (mode 600), and before switching away `cs`
re-captures the current account so rotated refresh tokens stay fresh.

Honors `CLAUDE_CONFIG_DIR`, `XDG_DATA_HOME`, and `XDG_CONFIG_HOME`.

> Codex support was removed in 0.2.0 — `cs` is Claude-only now. Old Codex
> snapshots in `state.json` are preserved untouched; use cs ≤ 0.1.6 if you
> still need them.

## License

[MIT](LICENSE)
