# cs

Switch between Claude Code and Codex accounts — like `cd -`, but for your CLI logins.

`cs` snapshots each account's credentials under an alias and restores them
instantly, so you can hop between personal/work/org accounts without
re-authenticating every time. It never implements OAuth itself: it delegates
login to `claude`/`codex` and only ever swaps credentials on disk. Claude Code
is the default; add `--codex` to any command to target Codex instead.

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
cs whoami                # show the live account on both Claude and Codex
cs switch next           # switch to the least-used (5h) Claude account
cs usage [--live]        # every Claude account's 5h/7d usage

cs add work --codex               # same commands for Codex, with --codex
cs add work --codex --device-auth # device-code login, for SSH / headless
cs switch - --codex               # toggle the previous Codex account
```

| Command | Description |
|---|---|
| `cs add <alias> [--current] [--force]` | Authenticate (or snapshot `--current`) and save under `<alias>` |
| `cs switch <alias>` / `cs switch -` | Activate an alias; `-` is the previous one |
| `cs del <alias>` | Remove a saved alias (live credentials untouched) |
| `cs list` | List saved aliases |
| `cs whoami` | Show the live signed-in account on both Claude and Codex |
| `cs switch next` | Activate the least-used (5h) Claude account |
| `cs usage [--live]` | Show every Claude account's 5h/7d usage (daemon's last poll, or `--live`) |
| `cs start` | Run the auto-switcher in the foreground (see below) |
| `cs logs [-f]` | Print / follow the auto-switcher log |
| `cs daemon install` | Install a systemd user unit / launchd agent that runs `cs start` at login |
| `cs --version` | Print version |

Every account command takes `--codex` to target Codex instead of Claude Code
(the default). Claude and Codex aliases are independent — you can have a `work`
in each. If Claude Code is running when you switch, `cs` warns you first (pass
`--yes` to skip the prompt).

## Auto-switcher (Claude only)

With two or more Claude subscriptions saved, `cs start` rotates them for you:
every time the current account's 5-hour usage crosses a multiple of `step`
(15 → 30 → 45 %…), it switches to the least-used other account. Claude Code
picks up the new credentials live, so a running session just keeps going.

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
step = 15                 # switch when the current account crosses a multiple of this (%)
poll_secs = 60            # how often the current account is polled
idle_poll_secs = 300      # how often the others are polled
min_switch_gap_secs = 300 # never switch twice within this window
ceiling = 90              # never switch TO an account at/above this 5h %

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

## How it works

**Claude** stores a login in two pieces:

- the OAuth tokens (`claudeAiOauth`) — on Linux in `~/.claude/.credentials.json`,
  on macOS in the login Keychain (service `Claude Code-credentials`)
- the account identity (`oauthAccount`) — in `~/.claude.json` on both platforms

A Claude switch patches **only** those keys, so your MCP tokens, project history,
and settings are never touched.

**Codex** keeps everything in one file, `~/.codex/auth.json`, so a Codex switch
is a whole-file swap; the account email is read from the `id_token`.

Either way, `cs add` captures the account into `~/.local/share/cs/state.json`
(mode 600, split per provider), and before switching away it re-captures the
current account so rotated refresh tokens stay fresh.

Honors `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `XDG_DATA_HOME`, and `XDG_CONFIG_HOME`.

## License

[MIT](LICENSE)
