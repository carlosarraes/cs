# cs agent guide

cs selects a Claude account at launch and keeps that process on it.
No server is required for cs run. This guide is static and makes no API calls.

## Inspect

cs list                 # Saved aliases and emails
cs usage                # Subscription quotas and API-key 7d/30d spend
cs usage --live          # Refresh usage now
cs sessions             # Live conversation UUIDs, accounts, queued moves/errors
cs whoami               # Shared OAuth identity or selected key mode

## Launch or resume

cs run -- --chrome
cs run -- --dangerously-skip-permissions --chrome --resume "pm"
cs run --account work -- claude -p "say hi"
cs run --account user@example.com -- claude -p "say hi"

Put cs options before -- and Claude options after it. The optional "claude"
prefix is accepted. Permission bypass is optional and is passed through to Claude.

Duplicate or unmatched resume names open a picker like plain Claude; headless runs need a UUID for these names.

Automatic routing balances eligible accounts. A recent conversation's account
is preferred within the one-hour affinity window. An exhausted account is skipped,
including on resume. Existing processes do not switch accounts automatically.

--account explicitly pins an alias or unique email. Unknown or ambiguous accounts,
unusable credentials, unavailable usage, or exhausted quota fail with nonzero exit.
An explicit pin never silently falls back. Pinned sessions count in cs usage.
The selected Claude account does not set GitHub/git or other external identities.

## Anthropic API keys

cs add-key work-api          # Read API key from masked stdin, never argv
cs run --account work-api -- -p "say hi"
cs del work-api              # Remove the API key secret

`cs list` marks key aliases [key]; only sk-ant-… plus the last four characters is
shown. Secrets use macOS Keychain or Linux mode-0600 credential storage, never
state/config. Select a key with cs switch limit, then cs run defaults to that key;
explicit --account also works. Switching to a key clears shared OAuth and its old
identity; whoami reports key mode. Unmanaged claude may need cs run to use the key.
Returning to a subscription restores OAuth and strips key environment variables
from future subscription launches. Keys are excluded from automatic quota routing,
switch next, and the proxy.

`cs usage` shows `$X 7d  $Y 30d (as of HH:MM) est.` per key, plus a `local` line
for all local Claude JSONL usage. Estimates use ccost/ccusage-style token×price
math from local logs — not billed Anthropic spend. Per-key rows only count sessions
cs launched or moved to that alias. Moved conversations count only log entries
at or after the confirmed move; the local total includes the whole transcript.

Token-price totals cover midnight UTC 7/30 days ago through now, not invoices or
discounts/tools/premium charges. As-of time is local. Unknown models use fallback
rates with one warning. Offline static prices work without a network request.

[spend] poll_secs defaults to 3600; live_pricing=false optionally enables LiteLLM
pricing when true, cached 24h with a 3s timeout and offline fallback. `cs usage`
refreshes stale spend; --live forces refresh. Old state, config, and quota
observations remain compatible.


## Move running conversations

cs switch carraes                  # Shared login + all managed conversations
cs switch carraes --only carlos2    # Only conversations on alias carlos2
cs switch limit                    # Saved API-key destination
cs switch DEST --from SOURCE       # Alias for --only
cs switch DEST --session UUID
cs switch DEST --session UUID --interrupt
cs switch DEST --from SOURCE --interrupt

Plain switch accepts an alias, - or next. It changes shared login and queues all
supported managed leases not already on DEST, even if DEST is already current.
Targets are saved account aliases and their conversations, never hosts.
With selectors, DEST can also be a unique email. --only ALIAS selects managed
conversations currently on ALIAS; --from has identical behavior. --session selects
one UUID from cs sessions. Selectors leave shared login unchanged.
Run cs sessions in another terminal; finding a UUID does not require a restart.
--only/--from and --session are mutually exclusive.

Without --interrupt, the move queues until Claude reports idle with no background
agents. Idle sessions switch immediately; cs start is not needed for switching.
Then cs restarts Claude on DEST and resumes the same conversation with supported
original flags.
--interrupt deliberately interrupts selected sessions and restarts immediately.
Use it when Claude is waiting for a quota reset, which does not emit idle.

Queued does not mean completed. Check cs sessions for pending moves or failures.
An exhausted/unusable destination is rejected. If the replacement fails before
confirming the conversation, cs attempts to restore the original account.

Only sessions launched by a version supporting targeted switching can be moved.
Older launchers must be reopened once. Headless runs, worktrees, and unsupported
restart flags cannot be moved automatically. Normal shell directory changes are
supported; restarts use the original launch project to preserve relative flags.
Plain switch reports unsupported sessions as skipped and queues supported ones.
Explicit selectors reject the batch if any selected session is unsupported.
Subscription conversations can move to a key via key authentication, without OAuth
quota checks. Key-bound conversations cannot change accounts again; start a new
conversation for another account. Other sessions and shared login are unaffected
by moves with selectors.

## Shared login and account maintenance

cs add work             # Save an account; restore the selected login
cs add work --current   # Snapshot the live login, then restore the selected account
cs add work --switch    # Save an account and make it active
cs refresh              # Update saved account after an external /login
cs switch work          # Shared login + queue all supported managed sessions
cs switch -             # Return to previous shared login
cs switch next          # Choose least-used 5h subscription; shared + managed
cs del work             # Remove saved alias, not a logout

Plain switch waits for idle. Its --yes flag skips the running-Claude warning and
cannot be combined with --only, --from or --session. --interrupt requires a selector.
If shared credential restore/clear fails, saved current stays unchanged and no moves
are queued. On macOS, SSH/non-GUI Keychain reads can fail with code 36 even when
an item exists. Retry with Keychain access; never print secrets or disable Keychain.

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

Use `cs t3-claude` or the `cs-t3-claude` symlink as a passthrough for t3. `cs pick` names the routed account without launching Claude.
t3 tracks and kills the pid it spawns, so t3-claude picks an account from local state only
(no network, ssh, renewal, or usage polling), then execs claude; no wrapper stays alive,
so there is no supervision, 30s credential renewal, or in-place switching for t3 sessions.
Its pid is the lease; dead pids are reaped by the next routing update. A still-valid profile
token is kept and a fresher one is imported into the saved account. Missing or stale usage
falls back to a local valid subscription account (no network); fresh usage that is over
ceiling still fails until `cs usage --live` or the `cs start` daemon refreshes it.

Borrow: cs run --borrow HOST -- <claude args>; cs pick --borrow HOST.
cs t3-claude and cs-t3-claude reject --borrow and CS_BORROW (borrow stays out of t3 for now).
CS_BORROW sets the default host for cs run and cs pick.
CLI --borrow overrides the environment and conflicts with --account.
Owner SSH commands: cs lend emits an access-only JSON grant; cs lend-release LEASE_ID
releases its durable lease. Refresh credentials remain on the owner.
