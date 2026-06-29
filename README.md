# cs

Switch between Claude Code accounts — like `cd -`, but for your Claude logins.

`cs` snapshots each account's credentials under an alias and restores them
instantly, so you can hop between personal/work/org accounts without
re-authenticating every time. It never implements OAuth itself: it delegates
login to `claude` and only ever swaps two JSON keys on disk.

> Linux only, OAuth subscription accounts only (for now).

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
```

| Command | Description |
|---|---|
| `cs add <alias> [--current] [--force]` | Authenticate (or snapshot `--current`) and save under `<alias>` |
| `cs switch <alias>` / `cs switch -` | Activate an alias; `-` is the previous one |
| `cs del <alias>` | Remove a saved alias (live credentials untouched) |
| `cs list` | List saved aliases |
| `cs --version` | Print version |

If Claude Code is running when you switch, `cs` warns you first (pass `--yes`
to skip the prompt).

## How it works

A Claude login lives in two files:

- `~/.claude/.credentials.json` → `claudeAiOauth` (the OAuth tokens)
- `~/.claude.json` → `oauthAccount` (account identity)

`cs add` captures those two values into `~/.local/share/cs/state.json` (mode
600). `cs switch` writes them back atomically, patching **only** those keys — so
your MCP tokens, project history, and settings are never touched. Before
switching away it re-captures the current account, so Claude's rotated refresh
tokens are always kept fresh.

Honors `CLAUDE_CONFIG_DIR` and `XDG_DATA_HOME`.

## License

[MIT](LICENSE)
