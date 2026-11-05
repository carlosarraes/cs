//! Local Claude Code JSONL spend estimates.
//! Ported from carlosarraes/ccost src/sources/claude.rs, src/pricing.rs, src/dedup.rs.
//! These are token×price estimates from local logs, not billed Anthropic spend.

use crate::{
    pricing::{Pricing, Tokens},
    state::{Account, AccountKind},
};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::{BufRead, BufReader},
    path::Path,
};

const DAY: i64 = 86400;

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Totals {
    pub seven_day_usd: f64,
    pub thirty_day_usd: f64,
}
impl Totals {
    fn add(&mut self, timestamp: i64, now: i64, cost: f64) {
        if timestamp > now {
            return;
        }
        if timestamp >= day_start(now - 30 * DAY) {
            self.thirty_day_usd += cost;
        }
        if timestamp >= day_start(now - 7 * DAY) {
            self.seven_day_usd += cost;
        }
    }
}

#[derive(Debug)]
pub struct Error {
    pub reason: &'static str,
}
impl Error {
    fn new(reason: &'static str) -> Self {
        Self { reason }
    }
}
type Result<T> = std::result::Result<T, Error>;

fn day_start(time: i64) -> i64 {
    time.div_euclid(DAY) * DAY
}

#[derive(Default, Deserialize)]
struct CacheCreation {
    #[serde(default)]
    ephemeral_1h_input_tokens: u64,
}
#[derive(Default, Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation: Option<CacheCreation>,
}
impl Usage {
    fn tokens(&self) -> Tokens {
        let one_hour = self
            .cache_creation
            .as_ref()
            .map_or(0, |c| c.ephemeral_1h_input_tokens)
            .min(self.cache_creation_input_tokens);
        Tokens {
            input: self.input_tokens,
            output: self.output_tokens,
            write_5m: self.cache_creation_input_tokens - one_hour,
            write_1h: one_hour,
            read: self.cache_read_input_tokens,
        }
    }
}

struct Row {
    time: i64,
    session: String,
    id: Option<String>,
    request: Option<String>,
    side: bool,
    model: String,
    tokens: Tokens,
}

fn parse_line(line: &str) -> Option<Row> {
    if !line.contains("\"usage\":{") && !line.contains("\"usage\": {") {
        return None;
    }
    let value: Value = serde_json::from_str(line).ok()?;
    for id in [
        &value["sessionId"],
        &value["requestId"],
        &value["message"]["id"],
    ] {
        if id.as_str() == Some("") {
            return None;
        }
    }
    if let Some(version) = value.get("version").filter(|v| !v.is_null()) {
        let mut parts = version.as_str()?.split('.');
        for _ in 0..2 {
            let part = parts.next()?;
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
        }
        if !parts
            .next()?
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_digit())
        {
            return None;
        }
    }
    let usage: Usage = serde_json::from_value(value["message"]["usage"].clone()).ok()?;
    Some(Row {
        time: crate::usage::parse_rfc3339(value["timestamp"].as_str()?)?,
        session: value["sessionId"].as_str()?.into(),
        id: value["message"]["id"].as_str().map(String::from),
        request: value["requestId"].as_str().map(String::from),
        side: value["isSidechain"].as_bool().unwrap_or(false),
        model: value["message"]["model"].as_str()?.into(),
        tokens: usage.tokens(),
    })
}

/// `alias` filters to sessions cs attributed to that key alias.
/// `None` sums every local Claude Code session (subscriptions included).
fn estimate(
    projects: &Path,
    sessions: &BTreeMap<String, String>,
    alias: Option<&str>,
    now: i64,
    prices: &Pricing,
) -> Result<Totals> {
    let mut rows: Vec<Row> = Vec::new();
    let mut exact: HashMap<String, usize> = HashMap::new();
    let mut by_message: HashMap<String, Vec<usize>> = HashMap::new();
    let mut pending = vec![projects.to_path_buf()];
    while let Some(path) = pending.pop() {
        let entries = match fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && path == projects => {
                continue
            }
            Err(_) => return Err(Error::new("could not read Claude project logs")),
        };
        let mut paths = entries
            .map(|e| e.map(|e| e.path()))
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(|_| Error::new("could not read Claude project logs"))?;
        paths.sort();
        for path in paths {
            let metadata = fs::symlink_metadata(&path)
                .map_err(|_| Error::new("could not read Claude project logs"))?;
            if metadata.is_dir() {
                pending.push(path);
                continue;
            }
            if !metadata.is_file() || path.extension().is_none_or(|s| s != "jsonl") {
                continue;
            }
            let file = fs::File::open(path)
                .map_err(|_| Error::new("could not read Claude project logs"))?;
            for line in BufReader::new(file).lines() {
                let line = line.map_err(|_| Error::new("could not read Claude project logs"))?;
                let Some(row) = parse_line(&line) else {
                    continue;
                };
                if let Some(alias) = alias {
                    if sessions.get(&row.session).is_none_or(|a| a != alias) {
                        continue;
                    }
                }
                let Some(id) = &row.id else {
                    rows.push(row);
                    continue;
                };
                let key = format!("{id}\0{}", row.request.as_deref().unwrap_or(""));
                let found = exact.get(&key).copied().or_else(|| {
                    by_message.get(id).and_then(|indices| {
                        indices.iter().copied().find(|i| row.side || rows[*i].side)
                    })
                });
                if let Some(i) = found {
                    let old = &rows[i];
                    if (!row.side && old.side)
                        || (row.side == old.side && row.tokens.total() > old.tokens.total())
                    {
                        rows[i] = row;
                    }
                    exact.insert(key, i);
                } else {
                    exact.insert(key, rows.len());
                    by_message.entry(id.clone()).or_default().push(rows.len());
                    rows.push(row);
                }
            }
        }
    }
    let mut totals = Totals::default();
    for row in rows {
        totals.add(row.time, now, prices.cost(&row.model, &row.tokens));
    }
    Ok(totals)
}

fn projects_dir() -> Result<std::path::PathBuf> {
    let source = crate::native::source_config()
        .map_err(|_| Error::new("Claude project directory unavailable"))?;
    Ok(source.join("projects"))
}

fn key_sessions() -> Result<BTreeMap<String, String>> {
    crate::routing::store::Store::default_path()
        .and_then(|s| s.update(|s| Ok(s.key_sessions.clone())))
        .map_err(|_| Error::new("session assignments unavailable"))
}

pub fn poll(alias: &str, account: &Account, now: i64, prices: &Pricing) -> Result<Totals> {
    let Some(AccountKind::ApiKey { .. }) = &account.kind else {
        return Err(Error::new("not an API key account"));
    };
    let sessions = key_sessions()?;
    estimate(&projects_dir()?, &sessions, Some(alias), now, prices)
}

pub fn poll_local(now: i64, prices: &Pricing) -> Result<Totals> {
    estimate(&projects_dir()?, &BTreeMap::new(), None, now, prices)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: i64 = 1788177600; // 2026-08-31 12:00 UTC

    fn line(session: &str, msg: &str, req: &str, input: u64) -> String {
        json!({"timestamp":"2026-08-31T10:00:00Z","sessionId":session,"requestId":req,"version":"2.0.1",
            "message":{"id":msg,"model":"claude-sonnet-4-5","usage":{"input_tokens":input,"output_tokens":0,
            "cache_creation_input_tokens":300,"cache_creation":{"ephemeral_1h_input_tokens":200},"cache_read_input_tokens":500}}}).to_string()
    }

    #[test]
    fn estimator_dedups_replays_and_counts_only_attributed_sessions() {
        let d = tempfile::tempdir().unwrap();
        let a = line("ours", "m1", "r1", 100);
        let larger = line("ours", "m1", "r1", 200);
        let mut side: Value = serde_json::from_str(&line("ours", "m1", "side", 999)).unwrap();
        side["isSidechain"] = json!(true);
        let invalid = a.replace("2.0.1", "fixture");
        let empty = line("ours", "m2", "", 1000000);
        std::fs::write(
            d.path().join("first.jsonl"),
            [
                a.as_str(),
                &side.to_string(),
                &invalid,
                &empty,
                &line("outside", "m9", "r9", 1000000),
            ]
            .join("\n"),
        )
        .unwrap();
        std::fs::create_dir(d.path().join("subagents")).unwrap();
        std::fs::write(
            d.path().join("subagents/replay.jsonl"),
            [a.as_str(), larger.as_str()].join("\n"),
        )
        .unwrap();
        let sessions = BTreeMap::from([("ours".into(), "work-api".into())]);
        let total = estimate(
            d.path(),
            &sessions,
            Some("work-api"),
            NOW,
            &Pricing::default(),
        )
        .unwrap();
        assert!((total.seven_day_usd - 0.002325).abs() < 1e-10);
        assert_eq!(total.seven_day_usd, total.thirty_day_usd);
        assert_eq!(
            estimate(d.path(), &sessions, Some("other"), NOW, &Pricing::default())
                .unwrap()
                .thirty_day_usd,
            0.
        );
        let all = estimate(d.path(), &sessions, None, NOW, &Pricing::default()).unwrap();
        assert!(all.thirty_day_usd > total.thirty_day_usd);
    }

    #[test]
    fn unknown_models_use_fallback_and_empty_or_invalid_ids_are_rejected() {
        let input = line("ours", "msg", "req", 100).replace("claude-sonnet-4-5", "unknown-model");
        let row = parse_line(&input).unwrap();
        assert!(Pricing::default().lookup(&row.model).1);
        assert!(parse_line(&line("ours", "", "req", 1)).is_none());
        assert!(parse_line(&line("", "msg", "req", 1)).is_none());
        assert!(parse_line(&input.replace("2.0.1", "2.0")).is_none());
    }
}
