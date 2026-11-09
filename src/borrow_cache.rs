//! Borrower side of `CS_BORROW`: a local cache of short-lived access tokens lent by another
//! machine. The t3 spawn path only reads this cache and `exec`s Claude; the network is touched
//! at most once, under a single-flight lock with a hard deadline, and only when nothing usable
//! is cached. Refresh tokens never reach this machine.
use crate::{
    borrow::{jump_host, shell_quote, ssh_timeout, valid_host, Grant, Inventory},
    state, usage, util,
};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    os::unix::{
        fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
        io::AsRawFd,
    },
    path::PathBuf,
    time::{Duration, Instant},
};

/// A cached token must have at least this long left to start a session with it.
const SPAWN_MIN_REMAINING_SECS: i64 = 600;
/// `borrow refresh` lends again once a cached token has less than this left.
const RELEND_BELOW_SECS: i64 = 1800;
/// After a failed refresh, spawns skip the network for this long and use the local login.
const FAILURE_BACKOFF_SECS: i64 = 60;
const MAX_SPAWN_BUDGET_SECS: u64 = 15;
const MANUAL_BUDGET_SECS: u64 = 60;

#[derive(Clone, Serialize, Deserialize)]
struct Cached {
    email: String,
    expires_at: i64,
    access_token: String,
    #[serde(default)]
    oauth_meta: Value,
    #[serde(default)]
    identity: Value,
    #[serde(default)]
    usage: Option<usage::Usage>,
    #[serde(default)]
    usage_fetched_at: Option<i64>,
    /// Monotonic rotation stamp: the least recently used account is picked next.
    #[serde(default)]
    last_used: i64,
}
impl std::fmt::Debug for Cached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cached")
            .field("email", &self.email)
            .field("access_token", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Cache {
    #[serde(default)]
    entries: BTreeMap<String, Cached>,
}

fn dir() -> Result<PathBuf> {
    Ok(state::state_path()?.with_file_name("borrow"))
}

fn file(host: &str, suffix: &str) -> Result<PathBuf> {
    if !valid_host(host) {
        bail!("invalid borrow host");
    }
    Ok(dir()?.join(format!("{host}.{suffix}")))
}

pub fn profile_path(host: &str, alias: &str) -> Result<PathBuf> {
    let key: String = alias.bytes().map(|b| format!("{b:02x}")).collect();
    Ok(dir()?.join("profiles").join(host).join(key))
}

fn ensure_dir() -> Result<()> {
    let dir = dir()?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

struct FileLock(#[allow(dead_code)] File);

/// `until: None` blocks; otherwise gives up with `None` once the instant passes.
fn lock_file(path: &PathBuf, until: Option<Instant>) -> Result<Option<FileLock>> {
    ensure_dir()?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    loop {
        let op = if until.is_some() {
            libc::LOCK_EX | libc::LOCK_NB
        } else {
            libc::LOCK_EX
        };
        // SAFETY: flock on an fd we own only touches the kernel lock table.
        if unsafe { libc::flock(file.as_raw_fd(), op) } == 0 {
            return Ok(Some(FileLock(file)));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::WouldBlock {
            return Err(error).context("locking borrow cache");
        }
        if until.is_some_and(|limit| Instant::now() >= limit) {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn load(host: &str) -> Result<Cache> {
    match fs::read(file(host, "json")?) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            eprintln!("cs: borrow {host}: ignoring unreadable token cache");
            Cache::default()
        })),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Cache::default()),
        Err(error) => Err(error.into()),
    }
}

/// Read-modify-write the cache under its short lock; `write` skips the save.
fn with_cache<T>(host: &str, write: bool, f: impl FnOnce(&mut Cache) -> Result<T>) -> Result<T> {
    let _lock = lock_file(&file(host, "lock")?, None)?;
    let mut cache = load(host)?;
    let result = f(&mut cache)?;
    if write {
        util::atomic_write(&file(host, "json")?, &serde_json::to_vec(&cache)?)?;
    }
    Ok(result)
}

fn window_exhausted(window: Option<&usage::Window>, now: i64) -> bool {
    window.is_some_and(|w| {
        w.utilization >= 100.
            && w.resets_at
                .as_deref()
                .and_then(usage::parse_rfc3339)
                .is_none_or(|reset| reset > now)
    })
}

fn over(window: Option<&usage::Window>, ceiling: f64) -> bool {
    window.is_some_and(|w| w.utilization >= ceiling)
}

/// Least recently used usable grant; accounts over the routing ceilings go last and
/// exhausted ones are skipped. A pure function so rotation can be tested directly.
fn choose(cache: &Cache, now: i64, five_ceiling: f64, weekly_ceiling: f64) -> Option<String> {
    cache
        .entries
        .iter()
        .filter(|(_, entry)| {
            !entry.access_token.is_empty()
                && entry.expires_at > now + SPAWN_MIN_REMAINING_SECS
                && !entry.usage.as_ref().is_some_and(|u| {
                    window_exhausted(u.five_hour.as_ref(), now)
                        || window_exhausted(u.seven_day.as_ref(), now)
                })
        })
        .min_by_key(|(alias, entry)| {
            let pressed = entry.usage.as_ref().is_some_and(|u| {
                over(u.five_hour.as_ref(), five_ceiling)
                    || over(u.seven_day.as_ref(), weekly_ceiling)
            });
            (pressed, entry.last_used, (*alias).clone())
        })
        .map(|(alias, _)| alias.clone())
}

fn pick_cached(host: &str, record: bool) -> Result<Option<(String, Cached)>> {
    let routing = crate::config::load()?.routing;
    with_cache(host, record, |cache| {
        let now = usage::now();
        let Some(alias) = choose(
            cache,
            now,
            routing.five_hour_ceiling,
            routing.weekly_ceiling,
        ) else {
            return Ok(None);
        };
        let newest = cache
            .entries
            .values()
            .map(|e| e.last_used)
            .max()
            .unwrap_or(0);
        let entry = cache.entries.get_mut(&alias).expect("chosen entry exists");
        if record {
            entry.last_used = chrono::Utc::now().timestamp_millis().max(newest + 1);
        }
        Ok(Some((alias, entry.clone())))
    })
}

fn failure_marker(host: &str) -> Result<PathBuf> {
    file(host, "failed")
}

fn recently_failed(host: &str) -> bool {
    failure_marker(host)
        .ok()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|text| text.trim().parse::<i64>().ok())
        .is_some_and(|at| (0..FAILURE_BACKOFF_SECS).contains(&(usage::now() - at)))
}

fn valid_alias(alias: &str) -> bool {
    (1..=64).contains(&alias.len())
        && alias
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._@+-".contains(&b))
}

pub struct Row {
    pub alias: String,
    pub email: String,
    pub expires_at: i64,
    pub refreshed: bool,
}

/// Fetch the owner's inventory, then lend each alias that is missing or near expiry, one at a
/// time with `--alias`, so every account gets a turn instead of the router's favourite.
fn refresh(host: &str, via: Option<&str>, deadline: Instant) -> Result<Vec<Row>> {
    ensure_dir()?;
    let result = refresh_inner(host, via, deadline);
    let marker = failure_marker(host)?;
    if result.is_ok() {
        let _ = fs::remove_file(marker);
    } else {
        util::atomic_write(&marker, usage::now().to_string().as_bytes())?;
    }
    result
}

fn refresh_inner(host: &str, via: Option<&str>, deadline: Instant) -> Result<Vec<Row>> {
    let listing = ssh_timeout(host, via, "lend --inventory", deadline)?;
    let inventory: Inventory = serde_json::from_slice(&listing)
        .map_err(|_| anyhow!("invalid inventory response from {host}"))?;
    let mut accounts: Vec<_> = inventory
        .accounts
        .into_iter()
        .filter(|a| valid_alias(&a.alias))
        .collect();
    accounts.sort_by(|a, b| a.alias.cmp(&b.alias));
    accounts.dedup_by(|a, b| a.alias == b.alias);
    let now = usage::now();
    let existing = load(host)?;
    let mut needed: Vec<_> = accounts
        .iter()
        .filter(|a| {
            existing
                .entries
                .get(&a.alias)
                .is_none_or(|e| e.access_token.is_empty() || e.expires_at < now + RELEND_BELOW_SECS)
        })
        .map(|a| a.alias.clone())
        .collect();
    let pressure = |alias: &String| {
        accounts
            .iter()
            .find(|a| &a.alias == alias)
            .and_then(|a| a.usage.as_ref())
            .and_then(|u| u.five_hour.as_ref())
            .map_or(0., |w| w.utilization)
    };
    needed.sort_by(|a, b| pressure(a).total_cmp(&pressure(b)));
    let mut lent: BTreeMap<String, Grant> = BTreeMap::new();
    for alias in &needed {
        if Instant::now() >= deadline {
            eprintln!("cs: borrow {host}: out of time before lending {alias}");
            break;
        }
        let remote = format!("lend --alias {} --no-lease", shell_quote(alias));
        let grant = ssh_timeout(host, via, &remote, deadline).and_then(|bytes| {
            serde_json::from_slice::<Grant>(&bytes).map_err(|_| anyhow!("invalid lend response"))
        });
        match grant {
            Ok(grant)
                if &grant.alias == alias
                    && !grant.access_token.is_empty()
                    && grant.expires_at > now + SPAWN_MIN_REMAINING_SECS =>
            {
                lent.insert(alias.clone(), grant);
            }
            Ok(_) => eprintln!("cs: borrow {host}: {alias}: owner returned an unusable grant"),
            Err(error) => eprintln!("cs: borrow {host}: {alias}: {error}"),
        }
    }
    if !needed.is_empty() && lent.is_empty() {
        bail!("owner lent none of {} requested accounts", needed.len());
    }
    with_cache(host, true, |cache| {
        cache
            .entries
            .retain(|alias, _| accounts.iter().any(|a| &a.alias == alias));
        for account in &accounts {
            if let Some(grant) = lent.remove(&account.alias) {
                let mut meta = grant.oauth_meta;
                if let Some(object) = meta.as_object_mut() {
                    for key in ["accessToken", "refreshToken", "expiresAt"] {
                        object.remove(key);
                    }
                }
                let last_used = cache.entries.get(&account.alias).map_or(0, |e| e.last_used);
                cache.entries.insert(
                    account.alias.clone(),
                    Cached {
                        email: grant.email,
                        expires_at: grant.expires_at,
                        access_token: grant.access_token,
                        oauth_meta: meta,
                        identity: grant.identity,
                        usage: None,
                        usage_fetched_at: None,
                        last_used,
                    },
                );
            }
            if let Some(entry) = cache.entries.get_mut(&account.alias) {
                entry.usage = account.usage.clone();
                entry.usage_fetched_at = account.usage_fetched_at;
            }
        }
        Ok(accounts
            .iter()
            .filter_map(|a| {
                cache.entries.get(&a.alias).map(|e| Row {
                    alias: a.alias.clone(),
                    email: e.email.clone(),
                    expires_at: e.expires_at,
                    refreshed: needed.contains(&a.alias) && e.expires_at >= now + RELEND_BELOW_SECS,
                })
            })
            .collect())
    })
}

fn budget() -> Duration {
    let secs = std::env::var("CS_BORROW_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(MAX_SPAWN_BUDGET_SECS);
    Duration::from_secs(secs.clamp(1, MAX_SPAWN_BUDGET_SECS))
}

/// A usable grant for this spawn. Warm cache: no lock but the cache's own, no network. Cold:
/// one process refreshes while the others wait, then everyone reads the shared result.
fn acquire(host: &str, via: Option<&str>, record: bool) -> Result<(String, Cached)> {
    if let Some(found) = pick_cached(host, record)? {
        return Ok(found);
    }
    if recently_failed(host) {
        bail!("owner refresh failed moments ago");
    }
    let deadline = Instant::now() + budget();
    let refresh_lock = file(host, "refresh.lock")?;
    let _single_flight = loop {
        let wait = (Instant::now() + Duration::from_millis(100)).min(deadline);
        if let Some(guard) = lock_file(&refresh_lock, Some(wait))? {
            break guard;
        }
        if let Some(found) = pick_cached(host, record)? {
            return Ok(found);
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for another refresh");
        }
    };
    if let Some(found) = pick_cached(host, record)? {
        return Ok(found);
    }
    if recently_failed(host) {
        bail!("owner refresh failed moments ago");
    }
    refresh(host, via, deadline)?;
    pick_cached(host, record)?.context("owner has no usable account")
}

/// `cs borrow refresh`: keep the cache warm so t3 spawns never wait on the network.
pub fn refresh_command(host: Option<&str>, via: Option<&str>) -> Result<()> {
    let host = host
        .map(str::to_owned)
        .or_else(|| std::env::var("CS_BORROW").ok().filter(|h| !h.is_empty()))
        .context("--from HOST or CS_BORROW is required")?;
    let via = jump_host(via);
    if !valid_host(&host) || via.as_deref().is_some_and(|v| !valid_host(v)) {
        bail!("invalid borrow host");
    }
    let deadline = Instant::now() + Duration::from_secs(MANUAL_BUDGET_SECS);
    let _single_flight = lock_file(&file(&host, "refresh.lock")?, Some(deadline))?
        .context("another refresh is still running")?;
    let rows = refresh(&host, via.as_deref(), deadline)?;
    let now = usage::now();
    for row in &rows {
        println!(
            "{} ({})  expires in {}  {}",
            row.alias,
            row.email,
            usage::fmt_duration((row.expires_at - now).max(0)),
            if row.refreshed { "refreshed" } else { "cached" }
        );
    }
    println!("{host}: {} account(s) cached", rows.len());
    Ok(())
}

/// The t3 entry for `CS_BORROW` / `--borrow`: pick a cached grant and become Claude. Any
/// borrow trouble falls back to the local login instead of failing the probe.
pub fn t3(host: &str, args: Vec<OsString>, pick: bool) -> Result<()> {
    let via = jump_host(None);
    match acquire(host, via.as_deref(), !pick) {
        Ok((alias, grant)) if pick => {
            println!("{alias} ({})", grant.email);
            Ok(())
        }
        Ok((alias, grant)) => exec(host, &alias, &grant, args),
        Err(error) => {
            eprintln!("cs: borrow {host}: {error}; falling back to the local login");
            crate::launch::t3_claude(args, pick)
        }
    }
}

fn exec(host: &str, alias: &str, grant: &Cached, args: Vec<OsString>) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let source = crate::native::source_config()?;
    let profile = profile_path(host, alias)?;
    let mut oauth = if grant.oauth_meta.is_object() {
        grant.oauth_meta.clone()
    } else {
        json!({})
    };
    oauth["accessToken"] = json!(grant.access_token);
    oauth["expiresAt"] = json!(grant.expires_at.saturating_mul(1000));
    crate::native::prepare_borrowed(&profile, &grant.email, &grant.identity, &oauth, &source)?;
    let mut command = crate::native::exec_command(&profile, args)?;
    command
        .env_remove("CS_NATIVE_LEASE")
        .env_remove("CS_NATIVE_GENERATION");
    eprintln!("cs: t3-claude --borrow {host} -> {alias} ({})", grant.email);
    let error = command.exec();
    Err(anyhow::Error::from(error).context("launching Claude"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(expires_in: i64, last_used: i64, five: Option<f64>) -> Cached {
        Cached {
            email: "x@x".into(),
            expires_at: usage::now() + expires_in,
            access_token: "test-token".into(),
            oauth_meta: Value::Null,
            identity: Value::Null,
            usage: five.map(|utilization| usage::Usage {
                five_hour: Some(usage::Window {
                    utilization,
                    resets_at: None,
                }),
                seven_day: None,
            }),
            usage_fetched_at: None,
            last_used,
        }
    }

    fn cache(entries: &[(&str, Cached)]) -> Cache {
        Cache {
            entries: entries
                .iter()
                .map(|(alias, e)| (alias.to_string(), e.clone()))
                .collect(),
        }
    }

    #[test]
    fn choose_rotates_least_recently_used_and_skips_unusable() {
        let now = usage::now();
        let c = cache(&[
            ("a", entry(7200, 5, None)),
            ("b", entry(7200, 3, None)),
            ("c", entry(60, 0, None)),
            ("d", entry(7200, 0, Some(100.))),
        ]);
        assert_eq!(choose(&c, now, 90., 95.).as_deref(), Some("b"));
    }

    #[test]
    fn choose_prefers_accounts_under_the_ceiling_before_older_ones() {
        let c = cache(&[
            ("old", entry(7200, 1, Some(95.))),
            ("new", entry(7200, 9, Some(10.))),
        ]);
        assert_eq!(choose(&c, usage::now(), 90., 95.).as_deref(), Some("new"));
        let c = cache(&[("old", entry(7200, 1, Some(95.)))]);
        assert_eq!(choose(&c, usage::now(), 90., 95.).as_deref(), Some("old"));
        assert_eq!(choose(&Cache::default(), usage::now(), 90., 95.), None);
    }

    #[test]
    fn debug_never_prints_the_token() {
        let text = format!("{:?}", entry(60, 0, None));
        assert!(!text.contains("test-token") && text.contains("REDACTED"));
    }

    #[test]
    fn aliases_from_the_owner_are_restricted() {
        assert!(valid_alias("carlos2") && valid_alias("a@b.io"));
        assert!(!valid_alias("") && !valid_alias("a b") && !valid_alias("a';rm"));
    }
}
