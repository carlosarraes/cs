# cs

Switch between Claude Code accounts — like `cd -`, but for your login.

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
cs usage [--live]        # every account's 5h/7d usage

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
| `cs usage [--live]` | Show every account's 5h/7d usage (daemon's last poll, or `--live`) |
| `cs snapshot` | Save the currently-open Claude sessions (`list`/`restore`/`del` manage them) |
| `cs start` | Run the auto-switcher in the foreground (see below) |
| `cs logs [-f]` | Print / follow the auto-switcher log |
| `cs daemon install` | Install a systemd user unit / launchd agent that runs `cs start` at login |
| `cs --version` | Print version |

If Claude Code is running when you switch, `cs` warns you first (pass `--yes`
to skip the prompt).

Claude forces a fresh login every 7 days, so sooner or later you will run
`/login` inside Claude Code rather than through `cs`. Run `cs refresh`
afterwards and the saved copy catches up; it finds the right alias by email, so
it cannot write one account's tokens into another account's alias, and it fixes
`current` if the live login drifted. The daemon does this on its own while it
runs. `cs` also refuses to overwrite an alias whose saved account differs from
the live login, so a mistaken login can never repoint an alias at someone else.

## Auto-switcher (Claude only)

With two or more Claude subscriptions saved, `cs start` rotates them for you:
once the current account's 5-hour usage reaches `switch_at` (95%), it switches
to the least-used other account. Claude Code picks up the new credentials
live, so a running session just keeps going.

It switches as *rarely* as it can, on purpose: prompt caches are per account,
so every switch makes each open session re-read its whole context uncached on
the new account (roughly 15% of a 5h window). So `cs` only switches near the
top, only to an account with real headroom (`ceiling`), not when the current
window is about to reset anyway, and preferably while no session is mid-turn.

```sh
cs daemon install   # writes ~/.config/cs/config.toml + a user service, prints the enable command
cs logs -f          # follow what it's doing from any terminal
cs usage            # per-account 5h/7d usage, from the daemon's last poll
```

Or just run `cs start` in a tmux pane. Every `report.interval_secs` it logs a
status block:

```
[15:05] report  * carraes  5h 45% (resets 2h14m) · 7d 12% (resets 4d2h) · running 3h02m
                - carlos   5h 33% (resets 4h50m) · 7d  3% (resets 6d1h) · idle 4h10m
[15:07] switch  carraes → carlos  (carraes 45% · carlos 33%)
```

Configuration lives in `~/.config/cs/config.toml` (every key optional, re-read
every tick so edits apply without a restart):

```toml
[auto-switcher]
enabled = true
switch_at = 95            # switch once the current account reaches this 5h %
ceiling = 75              # never switch TO an account at/above this 5h %
skip_if_reset_within_secs = 1800  # don't switch to bridge a window that resets soon
prefer_idle = true        # wait for sessions to go idle first (up to idle_wait_max_secs)
idle_wait_max_secs = 300
poll_secs = 60            # how often the current account is polled
idle_poll_secs = 300      # how often the others are polled
min_switch_gap_secs = 900 # never switch twice within this window

[report]
enabled = true
interval_secs = 300
verbose = false           # also log the active settings in each report

[warmup]                  # reserved — not implemented yet
enabled = false
```

Usage comes from the same endpoint Claude Code's `/usage` uses, polled with each
account's stored token (the current one with the live token). That endpoint
rate-limits bursts; `cs` honors its `Retry-After` and keeps the last numbers
visible while it waits. An account whose stored token has expired shows as
unknown and is switched *to* only as a last resort (when no account with known
usage qualifies) — Claude Code refreshes the token as soon as it becomes
current again. The daemon and a manual `cs switch` share a lock on `state.json`, so
they can't overwrite each other.

## Session snapshots

Working in tmux with several Claudes open and need to reboot? `cs snapshot`
records every *live* Claude Code session — its resume id, `/name`, working
directory, and where it sits in tmux (session, window, split pane), resolved
while everything is still running, so stale session files can't sneak in.

```sh
cs snapshot                  # → Snapshot '0823-2201' saved (5 sessions).
# ...reboot...
cs snapshot restore          # rebuilds the tmux sessions/windows/panes and
                             # types `claude --resume <id>` into each pane
cs snapshot list             # all snapshots (the newest is the restore default)
cs snapshot list 0823-2201   # the sessions inside one: name · id · tmux spot · cwd
```

Restore recreates missing tmux sessions and windows with their original names
(split panes come back tiled), skips sessions that are already running, and
falls back to printing the `claude --resume` commands when tmux isn't
available. Snapshots live in `~/.local/share/cs/snapshots.json`; the last 10
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
