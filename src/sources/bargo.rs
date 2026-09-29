//! Bargo's free congressional-trades API. It republishes the official House
//! and Senate filings as normalized JSON with tickers already resolved, which
//! usually surfaces a trade hours before our own PDF/HTML scrape does. The
//! same transaction later seen through `house`/`senate` dedupes on the
//! canonical key.
//!
//! No API key; one request of 100 rows per poll round stays under the rate
//! limit (a second page immediately returns 429).

use anyhow::Result;
use chrono::{NaiveDate, Utc};
use serde::Deserialize;
use tracing::warn;

use super::{PollReport, SourceCtx};
use crate::model::{Chamber, Disclosure, Side};

const URL: &str = "https://www.bargo.ai/free-apis/congress/v1/trades?limit=100&page=0";

#[derive(Debug, Deserialize)]
pub struct BargoResponse {
    pub trades: Vec<BargoTrade>,
}

#[derive(Debug, Deserialize)]
pub struct BargoTrade {
    pub member: String,
    pub chamber: String,
    #[serde(default)]
    pub ticker: Option<String>,
    #[serde(default)]
    pub asset: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub amount_low: Option<f64>,
    #[serde(default)]
    pub amount_high: Option<f64>,
    pub transaction_date: String,
    pub disclosure_date: String,
    #[serde(default)]
    pub filing_portal: Option<String>,
}

fn unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&#39;", "'")
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

pub fn to_disclosure(t: &BargoTrade) -> Option<Disclosure> {
    let chamber = match t.chamber.as_str() {
        "house" => Chamber::House,
        "senate" => Chamber::Senate,
        _ => return None,
    };
    Some(Disclosure {
        source: "bargo".into(),
        chamber,
        politician: unescape(&t.member),
        ticker: t
            .ticker
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_ascii_uppercase),
        asset: unescape(&t.asset),
        asset_type: None,
        side: Side::parse(&t.kind)?,
        tx_date: NaiveDate::parse_from_str(&t.transaction_date, "%Y-%m-%d").ok()?,
        filed_date: NaiveDate::parse_from_str(&t.disclosure_date, "%Y-%m-%d").ok()?,
        amount_low: t.amount_low.unwrap_or(0.0),
        amount_high: t.amount_high.unwrap_or(0.0),
        owner: None,
        url: t.filing_portal.clone().unwrap_or_else(|| "https://www.bargo.ai".into()),
    })
}

pub async fn poll(ctx: &SourceCtx) -> Result<PollReport> {
    let mut report = PollReport::default();
    let cutoff = Utc::now().date_naive() - chrono::Duration::days(ctx.max_age_days);
    let resp = ctx.http.get(URL).send().await?;
    if resp.status().as_u16() == 429 {
        // Shared free tier; the primary scrapers cover the same filings.
        warn!("bargo rate limited; skipping this round");
        return Ok(report);
    }
    let resp: BargoResponse = resp.error_for_status()?.json().await?;
    for t in &resp.trades {
        let Some(d) = to_disclosure(t) else {
            warn!(member = %t.member, kind = %t.kind, "bargo: unrecognised trade row");
            continue;
        };
        if d.filed_date < cutoff {
            continue;
        }
        if ctx.db.insert_disclosure(&d)? {
            report.disclosures += 1;
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_live_shape() {
        let raw = include_str!("../../tests/fixtures/bargo_trades.json");
        let resp: BargoResponse = serde_json::from_str(raw).unwrap();
        assert!(!resp.trades.is_empty());
        let d = to_disclosure(&resp.trades[0]).unwrap();
        assert_eq!(d.source, "bargo");
        assert!(d.ticker.is_some());
        assert!(d.amount_low > 0.0);
    }

    #[test]
    fn same_trade_from_house_pdf_dedupes() {
        let t = BargoTrade {
            member: "Nancy Pelosi".into(),
            chamber: "house".into(),
            ticker: Some("BE".into()),
            asset: "Bloom Energy Corporation Class A".into(),
            kind: "purchase".into(),
            amount_low: Some(1_000_001.0),
            amount_high: Some(5_000_000.0),
            transaction_date: "2026-07-24".into(),
            disclosure_date: "2026-08-21".into(),
            filing_portal: None,
        };
        let from_bargo = to_disclosure(&t).unwrap();
        let mut from_house = from_bargo.clone();
        from_house.source = "house".into();
        from_house.owner = Some("SP".into());
        from_house.asset_type = Some("ST".into());
        from_house.asset = "Bloom Energy Corporation Class A Common Stock (BE) [ST]".into();
        assert_eq!(from_bargo.key(), from_house.key());
    }
}
