# cs agent guide

cs selects a Claude account at launch and keeps that process on it.
No server is required for cs run. This guide is static and makes no API calls.

## Inspect

cs list                 # Saved aliases and emails
cs usage                # Cached 5h/7d usage and active session counts
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
Older launchers must be reopened once. Headless runs, worktrees, changed working
directories, and unsupported restart flags cannot be moved automatically.
Other sessions and the shared login are unaffected by targeted moves.

## Shared login and account maintenance

cs add work             # Authenticate and save an account
cs add work --current   # Save the already logged-in account
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
