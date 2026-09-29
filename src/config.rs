//! Configuration: a TOML file for everything non-secret, environment variables
//! for tokens and keys.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::model::Chamber;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_db_path")]
    pub db_path: PathBuf,
    /// Seconds between polling rounds.
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    /// Disclosures filed more than this many days before they were first seen
    /// are recorded but never traded. Keeps a fresh install from replaying a
    /// year of history, and keeps stale 45-day-old PTRs out of the book.
    #[serde(default = "default_max_age_days")]
    pub max_disclosure_age_days: i64,
    /// Start in dry-run mode (orders are logged, not sent). Can be toggled from
    /// Telegram; the toggle is persisted in the database.
    #[serde(default = "default_true")]
    pub dry_run: bool,
    /// Hard cap on the notional of any single order, regardless of pilot.
    #[serde(default = "default_max_order")]
    pub max_order_usd: f64,

    pub ibkr: IbkrConfig,
    #[serde(default)]
    pub sources: SourcesConfig,
    #[serde(default)]
    pub telegram: TelegramConfig,
    #[serde(default)]
    pub news: NewsConfig,
    #[serde(default)]
    pub pilots: Vec<PilotConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IbkrConfig {
    #[serde(default = "default_ib_host")]
    pub host: String,
    /// 7497 = TWS paper, 4002 = IB Gateway paper.
    #[serde(default = "default_ib_port")]
    pub port: u16,
    #[serde(default = "default_ib_client_id")]
    pub client_id: i32,
    /// Refuse to run against anything that is not a paper account.
    #[serde(default = "default_true")]
    pub require_paper_account: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SourcesConfig {
    #[serde(default = "default_true")]
    pub house: bool,
    #[serde(default = "default_true")]
    pub senate: bool,
    #[serde(default = "default_true")]
    pub bargo: bool,
    #[serde(default = "default_true")]
    pub edgar_13f: bool,
    /// Identifies you to the SEC as required by their fair-access policy,
    /// e.g. `"autotrades you@example.com"`. Falls back to `SEC_USER_AGENT`.
    #[serde(default)]
    pub sec_user_agent: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct TelegramConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct NewsConfig {
    #[serde(default)]
    pub enabled: bool,
    /// RSS/Atom feed URLs polled every round.
    #[serde(default)]
    pub feeds: Vec<String>,
    #[serde(default = "default_model")]
    pub model: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PilotKind {
    /// Mirror STOCK Act periodic transaction reports of one politician.
    Politician {
        /// Case-insensitive substring matched against the disclosed name,
        /// e.g. `"Pelosi"` or `"Nancy Pelosi"`.
        #[serde(rename = "match")]
        name_match: String,
        #[serde(default)]
        chamber: Option<Chamber>,
        /// Notional for a `$1,001 - $15,000` disclosure; larger buckets scale it.
        per_trade_usd: f64,
        #[serde(default = "default_true")]
        scale_by_amount: bool,
        /// Upper bound of exposure to a single ticker under this pilot.
        #[serde(default)]
        max_position_usd: Option<f64>,
    },
    /// Track a 13F filer's reported holdings and rebalance to their weights.
    ThirteenF {
        cik: u64,
        /// Only the N largest reported positions are held.
        #[serde(default = "default_top_n")]
        top_n: usize,
        /// Skip a rebalance leg smaller than this.
        #[serde(default = "default_min_trade")]
        min_trade_usd: f64,
    },
    /// Act on LLM-scored news headlines.
    News {
        per_trade_usd: f64,
        #[serde(default = "default_min_confidence")]
        min_confidence: f64,
        #[serde(default)]
        max_position_usd: Option<f64>,
    },
}

// `deny_unknown_fields` cannot be combined with `flatten`, so pilot typos are
// caught by `validate` instead of by serde.
#[derive(Debug, Clone, Deserialize)]
pub struct PilotConfig {
    /// Short identifier used from Telegram, e.g. `pelosi`.
    pub id: String,
    #[serde(default)]
    pub enabled: bool,
    /// Total capital this pilot may deploy (cost basis).
    pub allocation_usd: f64,
    #[serde(flatten)]
    pub kind: PilotKind,
}

/// Secrets, read from the environment (a `.env` file is loaded if present).
#[derive(Debug, Clone)]
pub struct Secrets {
    pub telegram_bot_token: Option<String>,
    pub telegram_owner_id: Option<i64>,
    pub anthropic_api_key: Option<String>,
    pub openfigi_api_key: Option<String>,
    pub sec_user_agent: Option<String>,
}

impl Secrets {
    pub fn from_env() -> Result<Self> {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let owner = match get("TELEGRAM_OWNER_ID") {
            Some(v) => Some(
                v.trim()
                    .parse::<i64>()
                    .context("TELEGRAM_OWNER_ID must be an integer chat id")?,
            ),
            None => None,
        };
        Ok(Self {
            telegram_bot_token: get("TELEGRAM_BOT_TOKEN"),
            telegram_owner_id: owner,
            anthropic_api_key: get("ANTHROPIC_API_KEY"),
            openfigi_api_key: get("OPENFIGI_API_KEY"),
            sec_user_agent: get("SEC_USER_AGENT"),
        })
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).with_context(|| format!("reading config {}", path.display()))?;
        let cfg: Config = toml::from_str(&raw).with_context(|| format!("parsing config {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        for p in &self.pilots {
            anyhow::ensure!(
                !p.id.is_empty() && p.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                "pilot id {:?} must be alphanumeric",
                p.id
            );
            anyhow::ensure!(seen.insert(p.id.as_str()), "duplicate pilot id {:?}", p.id);
            anyhow::ensure!(p.allocation_usd > 0.0, "pilot {:?}: allocation_usd must be positive", p.id);
            match &p.kind {
                PilotKind::Politician {
                    per_trade_usd,
                    name_match,
                    ..
                } => {
                    anyhow::ensure!(*per_trade_usd > 0.0, "pilot {:?}: per_trade_usd must be positive", p.id);
                    anyhow::ensure!(!name_match.trim().is_empty(), "pilot {:?}: match must not be empty", p.id);
                }
                PilotKind::ThirteenF { top_n, .. } => {
                    anyhow::ensure!(*top_n > 0, "pilot {:?}: top_n must be positive", p.id);
                }
                PilotKind::News {
                    per_trade_usd,
                    min_confidence,
                    ..
                } => {
                    anyhow::ensure!(*per_trade_usd > 0.0, "pilot {:?}: per_trade_usd must be positive", p.id);
                    anyhow::ensure!(
                        (0.0..=1.0).contains(min_confidence),
                        "pilot {:?}: min_confidence must be in 0..=1",
                        p.id
                    );
                }
            }
        }
        anyhow::ensure!(self.poll_interval_secs >= 60, "poll_interval_secs must be at least 60");
        Ok(())
    }

    pub fn pilot(&self, id: &str) -> Option<&PilotConfig> {
        self.pilots.iter().find(|p| p.id == id)
    }

    pub fn sec_user_agent(&self, secrets: &Secrets) -> Option<String> {
        self.sources.sec_user_agent.clone().or_else(|| secrets.sec_user_agent.clone())
    }
}

fn default_db_path() -> PathBuf {
    PathBuf::from("autotrades.db")
}
fn default_poll_interval() -> u64 {
    900
}
fn default_max_age_days() -> i64 {
    10
}
fn default_true() -> bool {
    true
}
fn default_max_order() -> f64 {
    10_000.0
}
fn default_ib_host() -> String {
    "127.0.0.1".into()
}
fn default_ib_port() -> u16 {
    7497
}
fn default_ib_client_id() -> i32 {
    17
}
fn default_top_n() -> usize {
    15
}
fn default_min_trade() -> f64 {
    100.0
}
fn default_min_confidence() -> f64 {
    0.75
}
fn default_model() -> String {
    "claude-opus-5".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses() {
        let raw = include_str!("../autotrades.example.toml");
        let cfg: Config = toml::from_str(raw).expect("example config must parse");
        cfg.validate().expect("example config must validate");
        assert!(cfg.pilots.iter().any(|p| matches!(p.kind, PilotKind::Politician { .. })));
        assert!(cfg.pilots.iter().any(|p| matches!(p.kind, PilotKind::ThirteenF { .. })));
    }
}
