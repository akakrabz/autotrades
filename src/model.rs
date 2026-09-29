//! Core domain types shared by the sources, the engine and the bot.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Which chamber a politician sits in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Chamber {
    House,
    Senate,
}

impl fmt::Display for Chamber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Chamber::House => f.write_str("house"),
            Chamber::Senate => f.write_str("senate"),
        }
    }
}

/// The transaction type as disclosed by the filer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Purchase,
    Sale,
    SalePartial,
    /// Exchange of one asset for another; never mirrored.
    Exchange,
}

impl Side {
    /// Parses the many spellings used across the House PDFs, the Senate HTML
    /// and the aggregator APIs.
    pub fn parse(raw: &str) -> Option<Side> {
        let s = raw.trim().to_ascii_lowercase();
        match s.as_str() {
            "p" | "purchase" | "buy" | "bought" => Some(Side::Purchase),
            "s" | "sale" | "sell" | "sold" | "sale (full)" | "sale_full" | "sale full" => Some(Side::Sale),
            "s (partial)" | "sale (partial)" | "sale_partial" | "sale partial" | "partial sale" => Some(Side::SalePartial),
            "e" | "exchange" => Some(Side::Exchange),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Side::Purchase => "purchase",
            Side::Sale => "sale",
            Side::SalePartial => "sale_partial",
            Side::Exchange => "exchange",
        }
    }
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A single disclosed transaction, normalized across every source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Disclosure {
    /// Which scraper produced the record (`house`, `senate`, `bargo`).
    pub source: String,
    pub chamber: Chamber,
    /// Display name, e.g. `Nancy Pelosi`.
    pub politician: String,
    /// Upper-case ticker symbol. Records without a ticker are kept for the
    /// audit trail but never traded.
    pub ticker: Option<String>,
    pub asset: String,
    /// House asset-type code (`ST`, `OP`, `GS`...) or the Senate "Asset Type"
    /// column. Only plain stock rows are mirrored.
    pub asset_type: Option<String>,
    pub side: Side,
    pub tx_date: NaiveDate,
    pub filed_date: NaiveDate,
    pub amount_low: f64,
    pub amount_high: f64,
    pub owner: Option<String>,
    /// Link to the filing this came from.
    pub url: String,
}

impl Disclosure {
    /// Canonical identity of a disclosed transaction. The same filing can be
    /// observed through the House/Senate scrapers *and* through an aggregator,
    /// so the key deliberately ignores the source, the owner column (which
    /// aggregators drop) and the full/partial distinction of a sale (which
    /// aggregators collapse). Fields that every source agrees on remain:
    /// chamber, family name, ticker, transaction date, direction, bucket.
    pub fn key(&self) -> String {
        let name = last_name(&self.politician).to_ascii_lowercase();
        let ticker = self.ticker.as_deref().unwrap_or("-");
        let asset = if self.ticker.is_some() {
            String::new()
        } else {
            // Ticker-less rows (funds, LLCs) need the asset text to stay distinct.
            self.asset
                .to_ascii_lowercase()
                .chars()
                .filter(|c| c.is_alphanumeric())
                .take(32)
                .collect()
        };
        let direction = match self.side {
            Side::Purchase => "buy",
            Side::Sale | Side::SalePartial => "sell",
            Side::Exchange => "exchange",
        };
        format!(
            "{}:{}:{}:{}:{}:{}:{}",
            self.chamber, name, ticker, asset, self.tx_date, direction, self.amount_low as i64
        )
    }

    /// Whether this row is a plain stock/ETF transaction that the engine is
    /// willing to mirror (no options, bonds, funds without tickers...).
    pub fn is_mirrorable_equity(&self) -> bool {
        if self.ticker.is_none() {
            return false;
        }
        match self.asset_type.as_deref().map(|s| s.trim().to_ascii_uppercase()) {
            // House codes: ST = stocks (incl. ETFs). Everything else is skipped.
            Some(t) if t == "ST" => true,
            // Senate column values.
            Some(t) if t == "STOCK" || t == "ETF" || t == "EXCHANGE TRADED FUND" => true,
            Some(_) => false,
            // Aggregators carry no type column: fall back to the asset text.
            None => {
                let a = self.asset.to_ascii_lowercase();
                ![
                    "option",
                    "call",
                    "put ",
                    "warrant",
                    "bond",
                    "note",
                    "bill",
                    "llc",
                    "l.p.",
                    "trust units",
                ]
                .iter()
                .any(|w| a.contains(w))
            }
        }
    }
}

/// `"Nancy Pelosi"` -> `"Pelosi"`, `"Rudy C. Yakym III"` -> `"Yakym"`.
///
/// Every source builds `politician` as `First Last` (see [`display_name`]), so
/// the family name is the last non-suffix word.
pub fn last_name(full: &str) -> &str {
    full.split(|c: char| c.is_whitespace() || c == ',')
        .rfind(|w| !w.is_empty() && !is_name_suffix(w))
        .unwrap_or(full)
        .trim()
}

fn is_name_suffix(word: &str) -> bool {
    matches!(
        word.trim_end_matches('.'),
        "Jr" | "Sr" | "II" | "III" | "IV" | "Hon" | "Mr" | "Mrs" | "Ms" | "Dr"
    )
}

/// Builds the canonical `First Last` display name from the separate fields the
/// government indexes provide, dropping honorifics and generational suffixes
/// (`"Hon."`, `"III"`, `"Jr."`) so the same person hashes identically across
/// sources.
pub fn display_name(first: &str, last: &str) -> String {
    let words = |s: &str| -> Vec<String> {
        s.split(|c: char| c.is_whitespace() || c == ',')
            .filter(|w| !w.is_empty() && !is_name_suffix(w))
            .map(str::to_string)
            .collect()
    };
    let mut parts = words(first);
    parts.extend(words(last));
    parts.join(" ")
}

/// Parses `"$1,001 - $15,000"`, `"$1,000,001 - $5,000,000"`, `"Over $50,000,000"`.
pub fn parse_amount_range(raw: &str) -> Option<(f64, f64)> {
    let nums: Vec<f64> = raw
        .split(|c: char| !(c.is_ascii_digit() || c == ','))
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.replace(',', "").parse::<f64>().ok())
        .collect();
    match nums.as_slice() {
        [lo, hi, ..] if hi >= lo => Some((*lo, *hi)),
        [single] if raw.to_ascii_lowercase().contains("over") => Some((*single, *single * 2.0)),
        [single] => Some((*single, *single)),
        _ => None,
    }
}

/// STOCK Act amount buckets mapped to a sizing multiplier. A `$1,001 - $15,000`
/// trade is the unit; larger disclosed ranges scale the mirrored notional
/// sub-linearly so a single $5M disclosure cannot blow through an allocation.
pub fn amount_weight(amount_low: f64) -> f64 {
    match amount_low as i64 {
        i64::MIN..=1_000 => 1.0,
        1_001..=15_000 => 1.0,
        15_001..=50_000 => 2.0,
        50_001..=100_000 => 3.0,
        100_001..=250_000 => 4.0,
        250_001..=500_000 => 5.0,
        500_001..=1_000_000 => 6.0,
        1_000_001..=5_000_000 => 7.0,
        _ => 8.0,
    }
}

/// One line of a 13F information table, aggregated per CUSIP.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Holding13F {
    pub cik: u64,
    pub accession: String,
    /// Report period end (quarter end), `YYYY-MM-DD`.
    pub period: String,
    pub cusip: String,
    pub issuer: String,
    pub shares: f64,
    /// Market value in USD as reported.
    pub value: f64,
    pub ticker: Option<String>,
}

/// A directional signal produced by the news scorer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewsSignal {
    pub news_id: String,
    pub ticker: String,
    pub direction: SignalDirection,
    pub confidence: f64,
    pub rationale: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SignalDirection {
    Buy,
    Sell,
    None,
}

impl fmt::Display for SignalDirection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SignalDirection::Buy => f.write_str("buy"),
            SignalDirection::Sell => f.write_str("sell"),
            SignalDirection::None => f.write_str("none"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amount_ranges() {
        assert_eq!(parse_amount_range("$1,001 - $15,000"), Some((1001.0, 15000.0)));
        assert_eq!(
            parse_amount_range("$1,000,001 -\n$5,000,000"),
            Some((1_000_001.0, 5_000_000.0))
        );
        assert_eq!(parse_amount_range("Over $50,000,000"), Some((50_000_000.0, 100_000_000.0)));
        assert_eq!(parse_amount_range("n/a"), None);
    }

    #[test]
    fn sides() {
        assert_eq!(Side::parse("P"), Some(Side::Purchase));
        assert_eq!(Side::parse("S (partial)"), Some(Side::SalePartial));
        assert_eq!(Side::parse("Sale (Full)"), Some(Side::Sale));
        assert_eq!(Side::parse("Exchange"), Some(Side::Exchange));
        assert_eq!(Side::parse("gift"), None);
    }

    #[test]
    fn last_names() {
        assert_eq!(last_name("Nancy Pelosi"), "Pelosi");
        assert_eq!(last_name("Rudy C. Yakym III"), "Yakym");
        assert_eq!(last_name("A. Mitchell McConnell, Jr."), "McConnell");
        assert_eq!(display_name("A. Mitchell", "McConnell, Jr."), "A. Mitchell McConnell");
        assert_eq!(display_name("Rudy C.", "Yakym"), "Rudy C. Yakym");
    }

    #[test]
    fn mirrorable_rows() {
        let mut d = Disclosure {
            source: "bargo".into(),
            chamber: Chamber::House,
            politician: "Nancy Pelosi".into(),
            ticker: Some("BE".into()),
            asset: "Bloom Energy Corporation - Common Stock".into(),
            asset_type: None,
            side: Side::Purchase,
            tx_date: NaiveDate::from_ymd_opt(2026, 7, 24).unwrap(),
            filed_date: NaiveDate::from_ymd_opt(2026, 8, 21).unwrap(),
            amount_low: 1001.0,
            amount_high: 15000.0,
            owner: None,
            url: String::new(),
        };
        assert!(d.is_mirrorable_equity());
        d.asset = "Bloom Energy Corporation - Call Option".into();
        assert!(!d.is_mirrorable_equity());
        d.asset = "x".into();
        d.asset_type = Some("OP".into());
        assert!(!d.is_mirrorable_equity());
        d.asset_type = Some("ST".into());
        assert!(d.is_mirrorable_equity());
        d.asset_type = Some("Stock".into());
        assert!(d.is_mirrorable_equity());
        d.ticker = None;
        assert!(!d.is_mirrorable_equity());
    }

    #[test]
    fn weights_are_monotonic() {
        let lows = [
            1001.0,
            15001.0,
            50001.0,
            100001.0,
            250001.0,
            500001.0,
            1_000_001.0,
            5_000_001.0,
        ];
        let ws: Vec<f64> = lows.iter().map(|l| amount_weight(*l)).collect();
        assert!(ws.windows(2).all(|w| w[0] < w[1]));
    }
}
