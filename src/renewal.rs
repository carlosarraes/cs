//! When cs may rotate a saved OAuth grant.
//!
//! Exchanging a refresh token revokes the previous access token. Claude sessions launched
//! through `cs t3-claude` hold only an access token and cannot re-read it mid-request, so a
//! rotation under a live session can kill an in-flight request. Accounts with live sessions
//! therefore renew only when the token is actually about to expire; idle accounts keep the
//! caller's earlier due time so their usage stays pollable.

use crate::{routing::store::Store, state};
use anyhow::Result;
use std::{collections::BTreeMap, fmt, os::unix::fs::DirBuilderExt};

/// Renewal threshold for an account with live sessions. Claude Code treats a token as expired
/// about 5 minutes early, and the daemon polls every minute, so 12 minutes leaves room for a
/// fresh access token to land in the profile before any session gives up on the old one.
pub const NEAR_EXPIRY_MS: i64 = 12 * 60 * 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sessions {
    Known(usize),
    /// The session store could not be read; treated as live.
    Unknown,
}

impl Sessions {
    fn idle(self) -> bool {
        self == Sessions::Known(0)
    }
}

/// Why a renewal is due; rendered into `daemon.log`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reason {
    pub left_ms: i64,
    pub sessions: Sessions,
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let left = if self.left_ms <= 0 {
            "expired".to_string()
        } else {
            format!("{}m left", self.left_ms / 60_000)
        };
        match self.sessions {
            Sessions::Known(0) => write!(f, "idle, no sessions, {left}"),
            Sessions::Known(1) => write!(f, "near expiry, {left}, 1 live session"),
            Sessions::Known(n) => write!(f, "near expiry, {left}, {n} live sessions"),
            Sessions::Unknown => write!(f, "near expiry, {left}, live sessions unknown"),
        }
    }
}

/// Decide whether to rotate a grant now. `idle_margin_ms` is the caller's usual due time and
/// applies only when no session uses the account; otherwise only `NEAR_EXPIRY_MS` counts.
pub fn decide(
    now_ms: i64,
    expires_at: Option<i64>,
    idle_margin_ms: i64,
    sessions: Sessions,
) -> Option<Reason> {
    let left_ms = expires_at?.saturating_sub(now_ms);
    let margin = if sessions.idle() {
        idle_margin_ms
    } else {
        NEAR_EXPIRY_MS
    };
    (left_ms <= margin).then_some(Reason { left_ms, sessions })
}

/// Live sessions per alias, from the routing leases (`cs run` and `cs t3-claude` alike).
pub fn session_counts() -> Option<BTreeMap<String, usize>> {
    Store::default_path()
        .and_then(|store| store.read(|s| Ok(s.counts())))
        .ok()
}

pub fn sessions_in(counts: Option<&BTreeMap<String, usize>>, alias: &str) -> Sessions {
    counts.map_or(Sessions::Unknown, |counts| {
        Sessions::Known(counts.get(alias).copied().unwrap_or(0))
    })
}

pub fn live_sessions(alias: &str) -> Sessions {
    sessions_in(session_counts().as_ref(), alias)
}

/// Serialises every writer of one profile's credentials (exchange, `.credentials.json`, and the
/// macOS Keychain item), so two refreshers can never both spend the same grant. Taken after
/// `accounts::credential_lock` and before `state::lock`. Not re-entrant within a process.
pub fn profile_lock(alias: &str) -> Result<state::LockGuard> {
    let profile = crate::native::profile_path(alias)?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&profile)?;
    state::lock_at(&profile.join(".credentials.lock"), true)
}

/// Context on a failed rotation attempt; `log_renewal_failure` skips errors carrying it because
/// the attempt already logged itself.
#[derive(Debug)]
pub struct Failed(pub Reason);

impl fmt::Display for Failed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "renewal ({}) failed", self.0)
    }
}

/// Mark and log a failed rotation. Errors here never include token material: cs composes them,
/// and OAuth HTTP failures carry only the status and error code.
pub fn failed(alias: &str, reason: Reason, error: anyhow::Error) -> anyhow::Error {
    let error = error.context(Failed(reason));
    crate::daemon::record("credentials", &format!("{alias}: {error:#}"));
    error
}

/// Record a successful rotation. Never pass token material here.
pub fn log_renewed(alias: &str, reason: Reason, expires_at_ms: Option<i64>) {
    let expires = expires_at_ms
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M %:z")
                .to_string()
        })
        .unwrap_or_else(|| "unknown".into());
    crate::daemon::record(
        "credentials",
        &format!("{alias}: renewed ({reason}); new token expires {expires}"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = 60_000;
    const NOW: i64 = 1_000_000_000;
    const CURRENT_MARGIN: i64 = 30 * MIN;
    const IDLE_MARGIN: i64 = 5 * MIN;

    #[test]
    fn live_sessions_defer_renewal_until_near_expiry() {
        for left in [13 * MIN, 20 * MIN, 29 * MIN, 30 * MIN, 8 * 60 * MIN] {
            assert_eq!(
                decide(NOW, Some(NOW + left), CURRENT_MARGIN, Sessions::Known(7)),
                None,
                "left={left}"
            );
        }
    }

    #[test]
    fn near_expiry_renews_even_with_live_sessions() {
        for left in [NEAR_EXPIRY_MS, 11 * MIN, MIN, 0, -MIN] {
            for margin in [CURRENT_MARGIN, IDLE_MARGIN] {
                assert!(
                    decide(NOW, Some(NOW + left), margin, Sessions::Known(7)).is_some(),
                    "left={left} margin={margin}"
                );
            }
        }
    }

    #[test]
    fn idle_aliases_keep_their_existing_due_time() {
        let idle = Sessions::Known(0);
        assert!(decide(NOW, Some(NOW + 20 * MIN), CURRENT_MARGIN, idle).is_some());
        assert!(decide(NOW, Some(NOW + 31 * MIN), CURRENT_MARGIN, idle).is_none());
        assert!(decide(NOW, Some(NOW + 4 * MIN), IDLE_MARGIN, idle).is_some());
        assert!(decide(NOW, Some(NOW + 6 * MIN), IDLE_MARGIN, idle).is_none());
        assert!(decide(NOW, None, CURRENT_MARGIN, idle).is_none());
    }

    #[test]
    fn unreadable_session_store_is_treated_as_live() {
        assert!(decide(NOW, Some(NOW + 20 * MIN), CURRENT_MARGIN, Sessions::Unknown).is_none());
        assert!(decide(NOW, Some(NOW + 10 * MIN), CURRENT_MARGIN, Sessions::Unknown).is_some());
        assert_eq!(sessions_in(None, "a"), Sessions::Unknown);
        let counts = BTreeMap::from([("a".to_string(), 3)]);
        assert_eq!(sessions_in(Some(&counts), "a"), Sessions::Known(3));
        assert_eq!(sessions_in(Some(&counts), "b"), Sessions::Known(0));
    }

    #[test]
    fn reasons_describe_time_left_and_sessions() {
        let reason = |left_ms, sessions| Reason { left_ms, sessions }.to_string();
        assert_eq!(
            reason(12 * MIN + 30_000, Sessions::Known(7)),
            "near expiry, 12m left, 7 live sessions"
        );
        assert_eq!(
            reason(4 * MIN, Sessions::Known(1)),
            "near expiry, 4m left, 1 live session"
        );
        assert_eq!(reason(-1, Sessions::Known(0)), "idle, no sessions, expired");
        assert_eq!(
            reason(MIN, Sessions::Unknown),
            "near expiry, 1m left, live sessions unknown"
        );
    }
}
