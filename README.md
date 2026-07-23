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

cs add work --codex      # same commands for Codex, with --codex
cs switch - --codex      # toggle the previous Codex account
```

| Command | Description |
|---|---|
| `cs add <alias> [--current] [--force]` | Authenticate (or snapshot `--current`) and save under `<alias>` |
| `cs switch <alias>` / `cs switch -` | Activate an alias; `-` is the previous one |
| `cs del <alias>` | Remove a saved alias (live credentials untouched) |
| `cs list` | List saved aliases |
| `cs whoami` | Show the live signed-in account on both Claude and Codex |
| `cs --version` | Print version |

Every account command takes `--codex` to target Codex instead of Claude Code
(the default). Claude and Codex aliases are independent — you can have a `work`
in each. If Claude Code is running when you switch, `cs` warns you first (pass
`--yes` to skip the prompt).

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

Honors `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, and `XDG_DATA_HOME`.

## License

[MIT](LICENSE)
