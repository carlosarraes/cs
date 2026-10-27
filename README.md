# cs

Route Claude Code conversations across subscription accounts, with quota-aware
placement and one-hour session affinity. Use `cs run` to launch a managed
conversation. `cs start` optionally keeps usage fresh and prints periodic reports.

`cs` snapshots each account's credentials under an alias and restores them
instantly, so you can hop between personal/work/org accounts without
re-authenticating every time. It delegates login to `claude`, saves credentials securely, and renews tokens
when it can establish refresh ownership.

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
| `cs run [--account <alias-or-email>] [-- <claude args>]` | Launch a native Claude conversation with automatic selection or an explicit account |
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

Launch each managed conversation with `cs run`:

```sh
cs start
# In other terminals:
cs run
cs run -- --resume <conversation-id>
cs run -- --model sonnet
cs sessions
cs usage --live
```

`cs run` selects an account before launching Claude with its native claude.ai
login in an isolated account profile. It preserves conversation history, user
content, project settings, and ordinary Claude flags, including named resume:

```sh
cs run -- --dangerously-skip-permissions --chrome --resume "pm"
```

No `cs switch` or explicit account is needed. `cs start` is optional for new
launches; it remains available for usage polling/reports and older proxy clients.
Existing processes are not migrated, restarted, or switched by installing this
version. Restart only the conversation you want to move to native authentication.

To choose a specific subscription, use an alias or its unique email:

```sh
cs run --account carlos -- claude -p "say hi"
cs run --account carlos@mondrio.io -- claude -p "say hi"
cs run --account carlos3 -- --chrome --resume "pm"
```

`--account` works interactively and headlessly. It resolves an alias first,
then a unique email (case insensitive). No additional email mapping is needed.
Unknown/ambiguous accounts, unusable credentials, unavailable usage, and exhausted
five-hour or weekly quota fail with a nonzero exit. Pins use a successful usage
reading less than five minutes old and before its reset, or fetch a fresh reading.
Admission ceilings do not apply to pins; actual 100% limits do. There is never
silent fallback to a different account.

Profiles live beside `state.json` under `profiles/`, with private permissions.
Credentials use Claude's profile-specific Keychain entry on macOS (or its private
credentials file when a new Keychain entry cannot be created), and its credentials
file on Linux. Tokens are not passed in arguments or authentication environment
variables. Conflicting auth variables and `apiKeyHelper` are removed for the child.
`cs` owns refresh grants; profiles receive access credentials and reload updates.

Each process stays on its chosen account and counts in `cs usage`/`cs sessions`.
Bare, safe-mode, background, remote, cloud, and desktop launches are unsupported;
managed runs require a local foreground process and enabled tracking hooks.
Settings that disable tracking hooks are rejected. Close a conversation before
resuming it elsewhere. An in-process resume onto another account's retained
conversation is blocked; reopen it with `cs run -- --resume <name-or-id>` instead.

Pinning chooses the **Claude subscription**, not the identity used by `gh`, git,
MCP servers, or other publishing tools. Configure those tools' credentials
separately to publish as the intended work account.

At launch, automatic selection excludes accounts without fresh
usage or usable credentials, and accounts at the configured quota ceilings.
It chooses the account with the fewest live managed conversations. Ties favor
more remaining weekly quota per hour until reset, then five-hour capacity per
hour until reset. Reset denominators have a 15-minute floor. This assumes
comparable subscription capacities; it does not predict how much a session
will consume.

The assignment stays for one hour after the conversation's last user prompt.
Resuming within that period prefers the account. If either quota window is at
100%, an automatic resume selects another eligible account instead. Explicit
`--account` pins still fail without fallback. After inactivity expiry,
selection runs again at launch. Reopening or completing a response does not
extend this timestamp. A running process and its subagents keep their account
for their entire lifetime. There is no automatic account failover: an unavailable assigned
account returns an error instead of silently changing credentials or replaying
an interrupted response.

`cs usage` shows `(N)` immediately after each account name for live managed
conversations, including `(0)` when none are active. Manual login status appears
in the row text, independently of those counts. Closed sessions
stop counting, but their recent assignments remain available for resume.
`cs sessions` lists each managed process, conversation, and account.

Configuration lives in `~/.config/cs/config.toml`:

```toml
[routing]
port = 18437                 # loopback only; 0 chooses a free port
poll_secs = 60
report_secs = 300
max_usage_age_secs = 300
five_hour_ceiling = 90       # admission limit; existing assignments stay
weekly_ceiling = 95
```

The server prints cached usage and active session counts at startup and every
`report_secs` seconds (default five minutes), on the next polling cycle. Set
`report_secs = 0` to disable reports. Reports also appear in `cs logs`; they do
not make additional usage API requests.

Restart `cs start` after changing configuration. The old `[auto-switcher]`,
`[report]`, and `[warmup]` sections are ignored with a migration notice. The
old global auto-switch loop has been removed. `cs switch <alias>` still changes
the shared login for unmanaged Claude instances; it cannot move managed sessions.

The service installer now starts the router:

```sh
cs daemon install            # writes the service and prints its enable command
cs logs -f
```

The service polls saved accounts with backoff. Managed launches also maintain
credentials without requiring the service. Renewal preserves the selected account;
it never switches identity. To avoid competing refresh-token rotations, renewal
pauses for the shared login while unmanaged Claude processes may own its grant.
Other saved accounts renew independently; identifying the shared login does not
require an unlocked Keychain. If both the shared identity and credentials are
unavailable, renewal pauses instead of guessing. A near-expiry renewal failure names the account on
stderr. Close unmanaged processes or renew their login and run `cs refresh` if
needed. Do not give the same saved refresh grant to an independent credential manager;
it cannot participate in cs's locks.
An already-started renewal can finish persisting its token after its conversation
exits; closing a conversation does not cancel a grant exchange midway.

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
keep using `claude --resume`. Managed restores select native accounts without requiring the router.
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
