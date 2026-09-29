//! News-driven signals. Headlines arrive from RSS/Atom feeds and from Telegram
//! (channel posts or messages forwarded to the bot); Claude screens each batch
//! for company-specific, material, directional events and returns structured
//! `NewsSignal`s. Everything that is macro, sector-wide or ambiguous scores
//! `none`, so in practice most headlines produce no trade.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tracing::{info, warn};

use super::{PollReport, SourceCtx};
use crate::db::NewsItem;
use crate::model::{NewsSignal, SignalDirection};

const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1/messages";
const MAX_BATCH: usize = 40;

const SYSTEM_PROMPT: &str = "You are a conservative equity news screener for a small, slow, paper-trading portfolio.\n\
You receive a batch of headlines. For each headline decide whether it implies a clear, near-term, tradeable move in ONE specific US-listed stock.\n\
Rules:\n\
- Output direction \"none\" unless the headline is company-specific AND material: earnings/guidance surprise, M&A announcement, FDA/regulatory decision, major contract win or loss, fraud/investigation, index inclusion, large buyback or dividend change, CEO departure with a clear read.\n\
- Macro, sector, index, commodity, crypto and opinion headlines are always \"none\".\n\
- Only name a ticker you are certain of (the common US listing, upper-case). If unsure of the ticker, use \"none\".\n\
- confidence is your probability (0-1) that the stock moves at least 2% in that direction within the next 5 trading days beyond the market. Be calibrated; most headlines deserve \"none\" or low confidence.\n\
- rationale: one short sentence.\n\
Return one entry per headline, in order, echoing its id.";

fn schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "signals": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "news_id": {"type": "string"},
                        "ticker": {"type": "string"},
                        "direction": {"type": "string", "enum": ["buy", "sell", "none"]},
                        "confidence": {"type": "number"},
                        "rationale": {"type": "string"}
                    },
                    "required": ["news_id", "ticker", "direction", "confidence", "rationale"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["signals"],
        "additionalProperties": false
    })
}

#[derive(Debug, Deserialize)]
struct ScoredBatch {
    signals: Vec<ScoredItem>,
}

#[derive(Debug, Deserialize)]
struct ScoredItem {
    news_id: String,
    ticker: String,
    direction: String,
    confidence: f64,
    rationale: String,
}

/// Stable id for a feed entry.
pub fn news_id(source: &str, entry_id: &str) -> String {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut h = DefaultHasher::new();
    source.hash(&mut h);
    entry_id.hash(&mut h);
    format!("{source}:{:016x}", h.finish())
}

/// Pulls every configured feed and stores unseen headlines.
pub async fn fetch_feeds(ctx: &SourceCtx, feeds: &[String]) -> Result<PollReport> {
    let mut report = PollReport::default();
    for url in feeds {
        let body = match ctx.http.get(url).send().await.and_then(|r| r.error_for_status()) {
            Ok(r) => r.bytes().await?,
            Err(e) => {
                warn!(url, "feed fetch failed: {e}");
                report.errors += 1;
                continue;
            }
        };
        let feed = match feed_rs::parser::parse(&body[..]) {
            Ok(f) => f,
            Err(e) => {
                warn!(url, "feed parse failed: {e}");
                report.errors += 1;
                continue;
            }
        };
        let source = reqwest::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_else(|| url.clone());
        for e in feed.entries {
            let Some(title) = e.title.map(|t| t.content.trim().to_string()).filter(|t| !t.is_empty()) else {
                continue;
            };
            let link = e.links.first().map(|l| l.href.clone());
            let entry_id = if e.id.is_empty() {
                link.clone().unwrap_or_else(|| title.clone())
            } else {
                e.id.clone()
            };
            let item = NewsItem {
                id: news_id(&source, &entry_id),
                source: source.clone(),
                title,
                link,
            };
            if ctx.db.insert_news(&item)? {
                report.news += 1;
            }
        }
    }
    Ok(report)
}

/// Scores up to [`MAX_BATCH`] unscored headlines with Claude and stores the
/// resulting signals. Returns how many actionable (non-`none`) signals came out.
pub async fn score_pending(ctx: &SourceCtx, model: &str, api_key: &str) -> Result<usize> {
    let items = ctx.db.unscored_news(MAX_BATCH)?;
    if items.is_empty() {
        return Ok(0);
    }
    let headlines: Vec<serde_json::Value> = items
        .iter()
        .map(|i| serde_json::json!({"id": i.id, "source": i.source, "title": i.title}))
        .collect();
    let body = serde_json::json!({
        "model": model,
        "max_tokens": 8000,
        "fallbacks": "default",
        "system": SYSTEM_PROMPT,
        "output_config": {"effort": "medium", "format": {"type": "json_schema", "schema": schema()}},
        "messages": [{"role": "user", "content": serde_json::to_string(&headlines)?}]
    });
    let resp = ctx
        .http
        .post(ANTHROPIC_URL)
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "server-side-fallback-2026-07-01")
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    let json: serde_json::Value = resp.json().await.context("anthropic response was not JSON")?;
    if !status.is_success() {
        bail!(
            "anthropic {status}: {}",
            json["error"]["message"].as_str().unwrap_or("unknown error")
        );
    }
    match json["stop_reason"].as_str() {
        Some("refusal") => {
            warn!(
                "news scoring batch refused ({:?}); marking batch scored",
                json["stop_details"]["category"]
            );
            ctx.db
                .mark_news_scored(&items.iter().map(|i| i.id.clone()).collect::<Vec<_>>())?;
            return Ok(0);
        }
        Some("max_tokens") => bail!("news scoring hit max_tokens; batch left unscored"),
        _ => {}
    }
    let text = json["content"]
        .as_array()
        .and_then(|c| c.iter().find(|b| b["type"] == "text"))
        .and_then(|b| b["text"].as_str())
        .context("no text block in anthropic response")?;
    let scored: ScoredBatch = serde_json::from_str(text).context("structured output did not match schema")?;

    let known: std::collections::HashSet<&str> = items.iter().map(|i| i.id.as_str()).collect();
    let signals: Vec<NewsSignal> = scored
        .signals
        .into_iter()
        .filter(|s| known.contains(s.news_id.as_str()))
        .filter_map(|s| {
            let direction = match s.direction.as_str() {
                "buy" => SignalDirection::Buy,
                "sell" => SignalDirection::Sell,
                _ => return None,
            };
            let ticker = s.ticker.trim().to_ascii_uppercase();
            if ticker.is_empty() || ticker == "NONE" || !ticker.chars().all(|c| c.is_ascii_alphanumeric() || c == '.') {
                return None;
            }
            Some(NewsSignal {
                news_id: s.news_id,
                ticker,
                direction,
                confidence: s.confidence.clamp(0.0, 1.0),
                rationale: s.rationale,
            })
        })
        .collect();
    ctx.db.insert_signals(&signals)?;
    ctx.db
        .mark_news_scored(&items.iter().map(|i| i.id.clone()).collect::<Vec<_>>())?;
    info!(
        headlines = items.len(),
        signals = signals.len(),
        model = json["model"].as_str().unwrap_or(model),
        "news batch scored"
    );
    Ok(signals.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_is_strict() {
        let s = schema();
        assert_eq!(s["additionalProperties"], false);
        assert_eq!(s["properties"]["signals"]["items"]["additionalProperties"], false);
    }

    #[test]
    fn ids_are_stable() {
        assert_eq!(news_id("a", "b"), news_id("a", "b"));
        assert_ne!(news_id("a", "b"), news_id("a", "c"));
    }
}
