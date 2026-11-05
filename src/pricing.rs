//! Ported from carlosarraes/ccost src/sources/claude.rs, src/pricing.rs, src/dedup.rs.

use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

const LITELLM_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
static WARNED_UNKNOWN: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Default, Clone, Copy)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub write_5m: u64,
    pub write_1h: u64,
    pub read: u64,
}
impl Tokens {
    pub fn total(&self) -> u64 {
        [
            self.input,
            self.output,
            self.write_5m,
            self.write_1h,
            self.read,
        ]
        .into_iter()
        .fold(0, u64::saturating_add)
    }
}

/// USD per million tokens; one-hour writes always cost twice the input rate.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Rates {
    pub input: f64,
    pub output: f64,
    pub write: f64,
    pub read: f64,
}
impl Rates {
    fn new(input: f64, output: f64) -> Self {
        Self {
            input,
            output,
            write: input * 1.25,
            read: input * 0.1,
        }
    }
    pub fn cost(&self, t: &Tokens) -> f64 {
        (t.input as f64 * self.input
            + t.output as f64 * self.output
            + t.write_5m as f64 * self.write
            + t.write_1h as f64 * self.input * 2.
            + t.read as f64 * self.read)
            / 1_000_000.
    }
}

pub struct Pricing {
    entries: BTreeMap<String, Rates>,
}
impl Default for Pricing {
    fn default() -> Self {
        let models = [
            ("claude-opus-4", 15., 75.),
            ("claude-opus-4-1", 15., 75.),
            ("claude-opus-4-5", 5., 25.),
            ("claude-opus-4-6", 5., 25.),
            ("claude-opus-4-7", 5., 25.),
            ("claude-opus-4-8", 5., 25.),
            ("claude-opus-5", 5., 25.),
            ("claude-opus-5-5", 4., 20.),
            ("claude-sonnet-5", 2., 10.),
            ("claude-sonnet-5-5", 2., 10.),
            ("claude-sonnet-4", 3., 15.),
            ("claude-sonnet-4-5", 3., 15.),
            ("claude-sonnet-4-6", 3., 15.),
            ("claude-haiku-4-5", 1., 5.),
            ("claude-3-5-haiku", 0.8, 4.),
            ("claude-3-5-sonnet", 3., 15.),
            ("claude-3-7-sonnet", 3., 15.),
            ("claude-3-opus", 15., 75.),
            ("claude-3-sonnet", 3., 15.),
            ("claude-3-haiku", 0.25, 1.25),
        ];
        let mut entries: BTreeMap<String, Rates> = models
            .into_iter()
            .map(|(m, i, o)| (m.into(), Rates::new(i, o)))
            .collect();
        entries.get_mut("claude-opus-5-5").unwrap().read = 0.2;
        entries.get_mut("claude-sonnet-5-5").unwrap().read = 0.1;
        Self { entries }
    }
}
impl Pricing {
    pub fn load(live: bool) -> Self {
        let mut table = Self::default();
        let Some(path) = cache_path() else {
            return table;
        };
        let fresh = fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age.as_secs() < 86400);
        let fetched = if live && !fresh {
            ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(3))
                .build()
                .get(LITELLM_URL)
                .call()
                .ok()
                .and_then(|r| r.into_string().ok())
        } else {
            None
        };
        if let Some(body) = fetched {
            if table.extend(&body) {
                let _ = crate::util::atomic_write(&path, body.as_bytes());
            }
        } else if let Ok(body) = fs::read_to_string(path) {
            table.extend(&body);
        }
        table
    }

    fn extend(&mut self, body: &str) -> bool {
        let Ok(raw) = serde_json::from_str::<BTreeMap<String, serde_json::Value>>(body) else {
            return false;
        };
        let mut entries = BTreeMap::new();
        for (name, value) in raw {
            if !(name.starts_with("claude-") || name.starts_with("anthropic/claude-")) {
                continue;
            }
            let price = |field: &str| {
                value[field]
                    .as_f64()
                    .filter(|v| v.is_finite() && *v >= 0.)
                    .map(|v| v * 1_000_000.)
            };
            let (Some(input), Some(output)) = (
                price("input_cost_per_token"),
                price("output_cost_per_token"),
            ) else {
                continue;
            };
            let mut rates = Rates::new(input, output);
            rates.write = price("cache_creation_input_token_cost").unwrap_or(rates.write);
            rates.read = price("cache_read_input_token_cost").unwrap_or(rates.read);
            entries.insert(name.rsplit('/').next().unwrap().to_string(), rates);
        }
        let valid = !entries.is_empty();
        self.entries.extend(entries);
        valid
    }

    pub fn lookup(&self, model: &str) -> (Rates, bool) {
        let normalized = model.replace('.', "-");
        let model = normalized.strip_prefix("anthropic/").unwrap_or(&normalized);
        if let Some(rates) = self.entries.get(model) {
            return (*rates, false);
        }
        // A dated release matches its exact model family, never a different version.
        if let Some((family, date)) = model.rsplit_once('-') {
            if date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) {
                if let Some(rates) = self.entries.get(family) {
                    return (*rates, false);
                }
            }
        }
        (Rates::new(3., 15.), true)
    }

    pub fn cost(&self, model: &str, tokens: &Tokens) -> f64 {
        let (rates, unknown) = self.lookup(model);
        if unknown && !WARNED_UNKNOWN.swap(true, Ordering::Relaxed) {
            eprintln!("cs: unknown Claude model; using fallback token prices for spend");
        }
        rates.cost(tokens)
    }
}

fn cache_path() -> Option<PathBuf> {
    dirs::cache_dir().map(|p| p.join("cs/model_prices_and_context_window.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn prices_cache_split_and_dated_models_without_guessing_new_versions() {
        let prices = Pricing::default();
        let (rates, unknown) = prices.lookup("claude-sonnet-4-5-20250929");
        assert!(!unknown);
        let tokens = Tokens {
            input: 100,
            output: 200,
            write_5m: 300,
            write_1h: 400,
            read: 500,
        };
        assert!((rates.cost(&tokens) - 0.006975).abs() < 1e-10);
        assert!(prices.lookup("claude-sonnet-99").1);
        assert!(prices.lookup("unrecognized").1);
        assert_eq!(prices.lookup("claude-opus-4-1-20250805").0.input, 15.);
        assert_eq!(prices.lookup("claude-opus-4-5").0.input, 5.);
    }

    #[test]
    fn litellm_cache_defaults_are_per_token_and_only_claude_is_loaded() {
        let mut prices = Pricing::default();
        prices.extend(&json!({
            "anthropic/claude-test": {"input_cost_per_token":0.000002,"output_cost_per_token":0.00001},
            "claude-invalid": {"input_cost_per_token":-1.,"output_cost_per_token":2.},
            "gpt-test": {"input_cost_per_token":1.,"output_cost_per_token":2.}
        }).to_string());
        let (rates, unknown) = prices.lookup("claude-test");
        assert!(!unknown);
        assert_eq!(rates.input, 2.);
        assert_eq!(rates.write, 2.5);
        assert_eq!(rates.read, 0.2);
        assert!(prices.lookup("claude-invalid").1);
        assert!(prices.lookup("gpt-test").1);
    }
}
