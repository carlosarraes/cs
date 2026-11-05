# cs agent guide

cs selects a Claude account at launch and keeps that process on it.
No server is required for cs run. This guide is static and makes no API calls.

## Inspect

cs list                 # Saved aliases and emails
cs usage                # Subscription quotas and API-key 7d/30d spend
cs usage --live          # Refresh usage now
cs sessions             # Live conversation UUIDs, accounts, queued moves/errors
cs whoami               # Shared login identity, not every managed session

## Launch or resume

cs run -- --chrome
cs run -- --dangerously-skip-permissions --chrome --resume "pm"
cs run --account work -- claude -p "say hi"
cs run --account user@example.com -- claude -p "say hi"

Put cs options before -- and Claude options after it. The optional "claude"
prefix is accepted. Permission bypass is optional and is passed through to Claude.

Automatic routing balances eligible accounts. A recent conversation's account
is preferred within the one-hour affinity window. An exhausted account is skipped,
including on resume. Existing processes do not switch accounts automatically.

--account explicitly pins an alias or unique email. Unknown or ambiguous accounts,
unusable credentials, unavailable usage, or exhausted quota fail with nonzero exit.
An explicit pin never silently falls back. Pinned sessions count in cs usage.
The selected Claude account does not set GitHub/git or other external identities.

## Anthropic API keys

cs add-key work-api          # Read API key from masked stdin, never argv
cs add-key org-api --admin   # Also prompt for its organization admin key
cs run --account work-api -- -p "say hi"
cs del work-api              # Remove both API and admin secrets

`cs list` marks key aliases [key]; only sk-ant-… plus the last four characters is
shown. Secrets use macOS Keychain or Linux mode-0600 credential storage, never
state/config. Key accounts require explicit --account and are excluded from
automatic routing, switch next, shared-login switching, and the proxy. Utilities
also accept key aliases. Running key conversations cannot change accounts.

With an admin key, `cs usage` labels spend `admin usage_report`: paginated daily
per-model token usage, filtered to the resolved API key id. Admin keys grant
organization-wide access; cs uses them only for read-only GET metadata/usage
requests. Use the same organization as the API key. Ambiguous hints fail safely.

Without admin, `est.` counts only JSONL session ids cs launched with that key.
Claude Code logs do not record which API key was used. Sessions started outside
cs are not counted. Do not resume an unattributed conversation with a key or
change its credentials outside cs. Deleting an alias drops its attribution/cache.
Token-price totals cover midnight UTC 7/30 days ago through now, not invoices or
discounts/tools/premium charges. As-of time is local. Unknown models use fallback
rates with one warning. Offline static prices work without a network request.

[spend] poll_secs defaults to 3600; live_pricing=false optionally enables LiteLLM
pricing when true, cached 24h with a 3s timeout and offline fallback. `cs usage`
refreshes stale spend; --live forces refresh, respecting 429 backoff. Old state,
config, and quota observations remain compatible.

## Move running conversations

cs switch DEST --from SOURCE
cs switch DEST --session UUID
cs switch DEST --session UUID --interrupt
cs switch DEST --from SOURCE --interrupt

DEST is the destination account alias or unique email. --from selects all managed
conversations on SOURCE. --session selects one UUID from cs sessions.
Run cs sessions in another terminal; finding a UUID does not require a restart.
--from and --session are mutually exclusive.

Without --interrupt, the move queues until Claude reports idle, typically about
60 seconds after its response with no background agents. Then cs restarts Claude
on DEST and resumes the same conversation with supported original flags.
--interrupt deliberately interrupts selected sessions and restarts immediately.
Use it when Claude is waiting for a quota reset, which does not emit idle.

Queued does not mean completed. Check cs sessions for pending moves or failures.
An exhausted/unusable destination is rejected. If the replacement fails before
confirming the conversation, cs attempts to restore the original account.

Only sessions launched by a version supporting targeted switching can be moved.
Older launchers must be reopened once. Headless runs, worktrees, and unsupported
restart flags cannot be moved automatically. Normal shell directory changes are
supported; restarts use the original launch project to preserve relative flags.
Other sessions and the shared login are unaffected by targeted moves.

## Shared login and account maintenance

cs add work             # Save an account; restore the selected login
cs add work --current   # Snapshot the live login, then restore the selected account
cs add work --switch    # Save an account and make it active
cs refresh              # Update saved account after an external /login
cs switch work          # Change shared login; may disrupt unmanaged Claude
cs switch -             # Return to previous shared login
cs switch next          # Shared login: choose least-used 5h account
cs del work             # Remove saved alias, not a logout

Plain cs switch is not a targeted session move. Its --yes flag skips the running
Claude warning; it cannot be combined with --from or --session.

## Optional services and snapshots

cs start                # Usage polling/reports and older proxy clients
cs logs --follow        # Router log
cs daemon install       # Start cs start at login
cs snapshot             # Save open sessions
cs snapshot list
cs snapshot restore     # Reopen newest snapshot in tmux
cs snapshot --help      # Snapshot subcommands

Use cs <command> --help for detailed flags. Never print or copy account tokens.

Non-model utilities: `cs run --account ALIAS -- mcp list` bypasses model quota,
including at 100% weekly usage. Supported utility prefixes: mcp, doctor, auth status;
also standalone --help/-h and --version/-v. Put the command first after --.
Pins retain credential/identity checks with no fallback. Without --account, use the
currently selected account. Utilities create no conversation or session count.
Other arguments use normal quota admission; config is not a Claude subcommand.
