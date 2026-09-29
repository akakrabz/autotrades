//! Data sources. Each one turns a public feed into rows in the database; the
//! engine decides what, if anything, to do with them.

pub mod bargo;
pub mod edgar13f;
pub mod house;
pub mod news;
pub mod senate;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tracing::{error, info};

use crate::config::{Config, PilotKind, Secrets};
use crate::db::Db;

pub const USER_AGENT: &str = concat!(
    "autotrades/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/akakrabz/autotrades)"
);

/// Shared state handed to every source.
pub struct SourceCtx {
    pub http: reqwest::Client,
    pub db: Arc<Db>,
    pub max_age_days: i64,
    pub sec_user_agent: Option<String>,
    pub openfigi_key: Option<String>,
    /// CIKs of every configured 13F pilot (enabled or not, so history is ready
    /// the moment one is switched on).
    pub ciks: Vec<u64>,
}

impl SourceCtx {
    pub fn new(cfg: &Config, secrets: &Secrets, db: Arc<Db>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(90))
            .gzip(true)
            .build()?;
        let ciks = cfg
            .pilots
            .iter()
            .filter_map(|p| match p.kind {
                PilotKind::ThirteenF { cik, .. } => Some(cik),
                _ => None,
            })
            .collect();
        Ok(Self {
            http,
            db,
            max_age_days: cfg.max_disclosure_age_days,
            sec_user_agent: cfg.sec_user_agent(secrets),
            openfigi_key: secrets.openfigi_api_key.clone(),
            ciks,
        })
    }
}

/// What a polling round found.
#[derive(Debug, Default, Clone, Copy)]
pub struct PollReport {
    /// Filings (PTRs / 13F accessions) not seen before.
    pub filings: usize,
    /// Individual transactions stored for the first time.
    pub disclosures: usize,
    /// 13F holdings snapshots stored.
    pub holdings: usize,
    /// Headlines stored.
    pub news: usize,
    pub errors: usize,
}

impl PollReport {
    pub fn merge(&mut self, o: PollReport) {
        self.filings += o.filings;
        self.disclosures += o.disclosures;
        self.holdings += o.holdings;
        self.news += o.news;
        self.errors += o.errors;
    }

    pub fn is_quiet(&self) -> bool {
        self.disclosures == 0 && self.holdings == 0 && self.errors == 0
    }
}

/// Polls every enabled source. A failing source is logged and counted; it
/// never prevents the others from running.
pub async fn poll_all(ctx: &SourceCtx, cfg: &Config, secrets: &Secrets) -> PollReport {
    let mut total = PollReport::default();
    let mut run = |name: &'static str, r: Result<PollReport>| match r {
        Ok(rep) => {
            info!(
                source = name,
                filings = rep.filings,
                disclosures = rep.disclosures,
                holdings = rep.holdings,
                news = rep.news,
                "polled"
            );
            total.merge(rep);
        }
        Err(e) => {
            error!(source = name, "poll failed: {e:#}");
            total.errors += 1;
        }
    };
    // Bargo first: it is the cheapest and usually the earliest signal.
    if cfg.sources.bargo {
        run("bargo", bargo::poll(ctx).await);
    }
    if cfg.sources.house {
        run("house", house::poll(ctx).await);
    }
    if cfg.sources.senate {
        run("senate", senate::poll(ctx).await);
    }
    if cfg.sources.edgar_13f && !ctx.ciks.is_empty() {
        run("13f", edgar13f::poll(ctx).await);
    }
    if cfg.news.enabled {
        run("news-feeds", news::fetch_feeds(ctx, &cfg.news.feeds).await);
        match &secrets.anthropic_api_key {
            Some(key) => match news::score_pending(ctx, &cfg.news.model, key).await {
                Ok(n) => info!(signals = n, "news scored"),
                Err(e) => {
                    error!("news scoring failed: {e:#}");
                    total.errors += 1;
                }
            },
            None => error!("news.enabled but ANTHROPIC_API_KEY is not set"),
        }
    }
    total
}
