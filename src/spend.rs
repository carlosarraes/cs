//! Ported from carlosarraes/ccost src/sources/claude.rs, src/pricing.rs, src/dedup.rs.

use crate::{
    pricing::{Pricing, Tokens},
    state::{Account, AccountKind},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    io::{BufRead, BufReader},
    path::Path,
    time::Duration,
};

const BASE: &str = "https://api.anthropic.com";
const DAY: i64 = 86400;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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
    pub retry_after: Option<u64>,
}
impl Error {
    fn new(reason: &'static str) -> Self {
        Self {
            reason,
            retry_after: None,
        }
    }
}
type Result<T> = std::result::Result<T, Error>;
type Query = Vec<(String, String)>;

fn day_start(time: i64) -> i64 {
    time.div_euclid(DAY) * DAY
}
fn timestamp(time: i64) -> String {
    chrono::DateTime::from_timestamp(time, 0)
        .unwrap()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn get(admin: &str, path: &str, query: &[(String, String)]) -> Result<Value> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(15))
        .redirects(0)
        .build();
    let mut request = agent
        .get(&format!("{BASE}{path}"))
        .set("x-api-key", admin)
        .set("anthropic-version", "2023-06-01");
    for (key, value) in query {
        request = request.query(key, value);
    }
    let response = request.call().map_err(http_error)?;
    let body = response
        .into_string()
        .map_err(|_| Error::new("could not read admin usage response"))?;
    serde_json::from_str(&body).map_err(|_| Error::new("invalid admin usage response"))
}

fn http_error(error: ureq::Error) -> Error {
    match error {
        ureq::Error::Status(429, response) => {
            let retry_after = response
                .header("retry-after")
                .and_then(|s| s.trim().parse::<u64>().ok())
                .or_else(|| {
                    response
                        .header("retry-after")
                        .and_then(|s| chrono::DateTime::parse_from_rfc2822(s).ok())
                        .map(|d| d.timestamp().saturating_sub(crate::usage::now()).max(1) as u64)
                })
                .unwrap_or(300)
                .clamp(1, 86400);
            Error {
                reason: "admin usage rate limited (429)",
                retry_after: Some(retry_after),
            }
        }
        ureq::Error::Status(401, _) => Error::new("admin key rejected (401)"),
        ureq::Error::Status(403, _) => {
            Error::new("admin key lacks organization usage access (403)")
        }
        ureq::Error::Status(_, _) => Error::new("admin usage request failed"),
        ureq::Error::Transport(_) => Error::new("admin usage network unavailable"),
    }
}

fn data(value: &Value) -> Result<&Vec<Value>> {
    value["data"]
        .as_array()
        .ok_or_else(|| Error::new("invalid admin usage data"))
}

fn cursor(value: &Value, field: &str, seen: &mut HashSet<String>) -> Result<Option<String>> {
    match value["has_more"].as_bool() {
        Some(false) => Ok(None),
        Some(true) => {
            let next = value[field]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| Error::new("missing admin pagination cursor"))?;
            if seen.len() >= 1000 || !seen.insert(next.into()) {
                return Err(Error::new("repeated admin pagination cursor"));
            }
            Ok(Some(next.into()))
        }
        None => Err(Error::new("invalid admin pagination response")),
    }
}

fn hint_matches(key: &str, hint: &str) -> bool {
    let hint = hint.replace('…', "...");
    let Some((prefix, suffix)) = hint.split_once("...") else {
        return false;
    };
    prefix.starts_with("sk-ant-")
        && suffix.len() >= 4
        && key.starts_with(prefix)
        && key.ends_with(suffix)
}

fn resolve_key_id(
    key: &str,
    mut fetch: impl FnMut(&str, &[(String, String)]) -> Result<Value>,
) -> Result<String> {
    let mut after = None;
    let mut seen = HashSet::new();
    let mut matches = HashSet::new();
    loop {
        let mut query: Query = vec![("limit".into(), "100".into())];
        if let Some(after) = after {
            query.push(("after_id".into(), after));
        }
        let page = fetch("/v1/organizations/api_keys", &query)?;
        for row in data(&page)? {
            if row["partial_key_hint"]
                .as_str()
                .is_some_and(|hint| hint_matches(key, hint))
            {
                let id = row["id"]
                    .as_str()
                    .filter(|s| s.starts_with("apikey_"))
                    .ok_or_else(|| Error::new("invalid organization API key id"))?;
                matches.insert(id.to_string());
            }
        }
        after = cursor(&page, "last_id", &mut seen)?;
        if after.is_none() {
            break;
        }
    }
    if matches.len() != 1 {
        return Err(Error::new(
            "API key hint is missing or ambiguous in this organization",
        ));
    }
    Ok(matches.into_iter().next().unwrap())
}

fn admin_report(
    id: &str,
    now: i64,
    prices: &Pricing,
    mut fetch: impl FnMut(&str, &[(String, String)]) -> Result<Value>,
) -> Result<Totals> {
    let base: Query = vec![
        ("starting_at".into(), timestamp(day_start(now - 30 * DAY))),
        ("ending_at".into(), timestamp(now)),
        ("bucket_width".into(), "1d".into()),
        ("api_key_ids[]".into(), id.into()),
        ("group_by[]".into(), "model".into()),
    ];
    let mut next = None;
    let mut seen = HashSet::new();
    let mut totals = Totals::default();
    loop {
        let mut query = base.clone();
        if let Some(next) = next {
            query.push(("page".into(), next));
        }
        let page = fetch("/v1/organizations/usage_report/messages", &query)?;
        for bucket in data(&page)? {
            let time = bucket["starting_at"]
                .as_str()
                .and_then(crate::usage::parse_rfc3339)
                .ok_or_else(|| Error::new("invalid usage bucket timestamp"))?;
            let rows = bucket["results"]
                .as_array()
                .ok_or_else(|| Error::new("invalid usage bucket results"))?;
            for row in rows {
                if row["api_key_id"].as_str().is_some_and(|key| key != id) {
                    return Err(Error::new("usage report returned a different API key"));
                }
                let model = row["model"]
                    .as_str()
                    .ok_or_else(|| Error::new("usage report missing model"))?;
                let usage: Usage = serde_json::from_value(row.clone())
                    .map_err(|_| Error::new("invalid usage token counts"))?;
                totals.add(time, now, prices.cost(model, &usage.tokens(true)));
            }
        }
        next = cursor(&page, "next_page", &mut seen)?;
        if next.is_none() {
            break;
        }
    }
    Ok(totals)
}

pub fn poll(alias: &str, account: &Account, now: i64, prices: &Pricing) -> Result<Totals> {
    let Some(AccountKind::ApiKey {
        key_id, admin_ref, ..
    }) = &account.kind
    else {
        return Err(Error::new("not an API key account"));
    };
    if admin_ref.is_some() {
        let secret = crate::accounts::read_key(alias)
            .map_err(|_| Error::new("API key credential unavailable"))?;
        let admin = secret
            .admin
            .as_deref()
            .ok_or_else(|| Error::new("admin key credential unavailable"))?;
        let id = match key_id {
            Some(id) => id.clone(),
            None => {
                let id = resolve_key_id(&secret.key, |path, query| get(admin, path, query))?;
                cache_key_id(alias, account, &id)
                    .map_err(|_| Error::new("could not cache API key id"))?;
                id
            }
        };
        admin_report(&id, now, prices, |path, query| get(admin, path, query))
    } else {
        let sessions = crate::routing::store::Store::default_path()
            .and_then(|s| s.update(|s| Ok(s.key_sessions.clone())))
            .map_err(|_| Error::new("session assignments unavailable"))?;
        let source = crate::native::source_config()
            .map_err(|_| Error::new("Claude project directory unavailable"))?;
        estimate(&source.join("projects"), &sessions, alias, now, prices)
    }
}

fn cache_key_id(alias: &str, expected: &Account, id: &str) -> anyhow::Result<()> {
    let _lock = crate::state::lock()?;
    let mut state = crate::state::State::load()?;
    let account = state
        .claude_mut()
        .accounts
        .get_mut(alias)
        .ok_or_else(|| anyhow::anyhow!("account removed"))?;
    anyhow::ensure!(account.kind == expected.kind, "account changed");
    if let Some(AccountKind::ApiKey { key_id, .. }) = &mut account.kind {
        *key_id = Some(id.into());
    }
    state.save()
}

#[derive(Default, Deserialize)]
struct CacheCreation {
    #[serde(default)]
    ephemeral_1h_input_tokens: u64,
    #[serde(default)]
    ephemeral_5m_input_tokens: u64,
}
#[derive(Default, Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    uncached_input_tokens: u64,
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
    fn tokens(&self, admin: bool) -> Tokens {
        let cache = self.cache_creation.as_ref();
        let one_hour = cache.map_or(0, |c| c.ephemeral_1h_input_tokens);
        let hour = if admin {
            one_hour
        } else {
            one_hour.min(self.cache_creation_input_tokens)
        };
        Tokens {
            input: if admin {
                self.uncached_input_tokens
            } else {
                self.input_tokens
            },
            output: self.output_tokens,
            write_5m: if admin {
                cache.map_or(0, |c| c.ephemeral_5m_input_tokens)
            } else {
                self.cache_creation_input_tokens - hour
            },
            write_1h: hour,
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
        tokens: usage.tokens(false),
    })
}

fn estimate(
    projects: &Path,
    sessions: &BTreeMap<String, String>,
    alias: &str,
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
                if sessions.get(&row.session).is_none_or(|a| a != alias) {
                    continue;
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: i64 = 1788177600; // 2026-08-31 12:00 UTC

    #[test]
    fn admin_report_follows_pages_and_sums_utc_daily_windows() {
        let mut calls = Vec::new();
        let totals = admin_report("apikey_fixture", NOW, &Pricing::default(), |path, query| {
            calls.push((path.to_string(), query.to_vec()));
            if query.iter().any(|(k,v)| k == "page" && v == "second") {
                Ok(json!({"has_more":false,"next_page":null,"data":[
                    {"starting_at":"2026-08-01T00:00:00Z","results":[{"model":"claude-sonnet-4-5","output_tokens":1000000}]},
                    {"starting_at":"2026-07-31T00:00:00Z","results":[{"model":"claude-sonnet-4-5","output_tokens":1000000}]}
                ]}))
            } else {
                Ok(json!({"has_more":true,"next_page":"second","data":[
                    {"starting_at":"2026-08-31T00:00:00Z","results":[{"model":"claude-sonnet-4-5","uncached_input_tokens":1000000,"output_tokens":1000000,"cache_read_input_tokens":1000000,"cache_creation":{"ephemeral_5m_input_tokens":1000000,"ephemeral_1h_input_tokens":1000000}}]},
                    {"starting_at":"2026-08-24T00:00:00Z","results":[{"model":"claude-sonnet-4-5","uncached_input_tokens":1000000}]},
                    {"starting_at":"2026-08-23T00:00:00Z","results":[{"model":"claude-sonnet-4-5","uncached_input_tokens":1000000}]}
                ]}))
            }
        }).unwrap();
        assert!((totals.seven_day_usd - 31.05).abs() < 1e-10);
        assert!((totals.thirty_day_usd - 49.05).abs() < 1e-10);
        assert_eq!(calls.len(), 2);
        assert!(calls[0]
            .1
            .contains(&("api_key_ids[]".into(), "apikey_fixture".into())));
        assert!(calls[0]
            .1
            .contains(&("starting_at".into(), "2026-08-01T00:00:00Z".into())));
        assert!(calls[0].1.contains(&("group_by[]".into(), "model".into())));
    }

    #[test]
    fn key_resolution_uses_full_partial_hint_and_rejects_ambiguity() {
        let key = ["sk-ant-api03-", "fixture-ABCD"].concat();
        let mut pages = 0;
        let id = resolve_key_id(&key, |_, query| {
            pages += 1;
            if query.iter().any(|(k,_)| k == "after_id") {
                Ok(json!({"has_more":false,"data":[{"id":"apikey_right","partial_key_hint":"sk-ant-api03-fix...ABCD"}]}))
            } else {
                Ok(json!({"has_more":true,"last_id":"apikey_wrong","data":[{"id":"apikey_wrong","partial_key_hint":"sk-ant-api03-other...ABCD"}]}))
            }
        }).unwrap();
        assert_eq!(id, "apikey_right");
        assert_eq!(pages, 2);
        assert!(
            resolve_key_id(&key, |_, _| Ok(json!({"has_more":false,"data":[
                {"id":"apikey_a","partial_key_hint":"sk-ant-api03-fix...ABCD"},
                {"id":"apikey_b","partial_key_hint":"sk-ant-api03-fix...ABCD"}
            ]})))
            .is_err()
        );
    }

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
        let total = estimate(d.path(), &sessions, "work-api", NOW, &Pricing::default()).unwrap();
        assert!((total.seven_day_usd - 0.002325).abs() < 1e-10);
        assert_eq!(total.seven_day_usd, total.thirty_day_usd);
        assert_eq!(
            estimate(d.path(), &sessions, "other", NOW, &Pricing::default())
                .unwrap()
                .thirty_day_usd,
            0.
        );
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

    #[test]
    fn malformed_or_repeated_pagination_fails_instead_of_returning_partial_spend() {
        assert!(
            admin_report("apikey_fixture", NOW, &Pricing::default(), |_, _| Ok(
                json!({"has_more":true,"next_page":"same","data":[]})
            ))
            .is_err()
        );
        assert!(
            admin_report("apikey_fixture", NOW, &Pricing::default(), |_, _| Ok(
                json!({"has_more":false,"data":[{"starting_at":"bad","results":[]}]})
            ))
            .is_err()
        );
    }

    #[test]
    fn rate_limits_honor_retry_after_and_errors_never_include_response_content() {
        let body = ["sk-ant-admin01-", "must-not-escape"].concat();
        let response: ureq::Response =
            format!("HTTP/1.1 429 Too Many Requests\r\nRetry-After: 120\r\n\r\n{body}")
                .parse()
                .unwrap();
        let error = http_error(ureq::Error::Status(429, response));
        assert_eq!(error.retry_after, Some(120));
        assert!(!error.reason.contains(&body));
        let response = ureq::Response::new(401, "Unauthorized", &body).unwrap();
        let error = http_error(ureq::Error::Status(401, response));
        assert_eq!(error.reason, "admin key rejected (401)");
        let response = ureq::Response::new(429, "Too Many Requests", &body).unwrap();
        assert_eq!(
            http_error(ureq::Error::Status(429, response)).retry_after,
            Some(300)
        );
    }
}
