//! House of Representatives periodic transaction reports (PTRs).
//!
//! The Clerk publishes a daily ZIP (`{year}FD.zip`) containing a tab-separated
//! index of every financial-disclosure filing of the year. Rows with
//! `FilingType == "P"` are PTRs; each one is a PDF at a predictable URL.
//! Electronically filed PTRs have a text layer that `pdftotext -layout`
//! renders as a fixed-width table we can parse. Paper filings are scanned
//! images and are recorded as `unparsed`.

use anyhow::{Context, Result, bail};
use chrono::{Datelike, NaiveDate, Utc};
use regex::Regex;
use std::io::Read;
use std::sync::LazyLock;
use tracing::{info, warn};

use super::{PollReport, SourceCtx};
use crate::model::{Chamber, Disclosure, Side, display_name, parse_amount_range};

const INDEX_URL: &str = "https://disclosures-clerk.house.gov/public_disc/financial-pdfs";
const PTR_URL: &str = "https://disclosures-clerk.house.gov/public_disc/ptr-pdfs";

#[derive(Debug, Clone)]
pub struct IndexRow {
    pub first: String,
    pub last: String,
    pub filing_type: String,
    pub year: i32,
    pub filed: NaiveDate,
    pub doc_id: String,
}

pub fn ptr_url(year: i32, doc_id: &str) -> String {
    format!("{PTR_URL}/{year}/{doc_id}.pdf")
}

/// Parses the `{year}FD.txt` index (CRLF, tab separated, header row).
pub fn parse_index(txt: &str) -> Vec<IndexRow> {
    txt.lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.trim_end_matches('\r').split('\t').collect();
            if f.len() < 9 {
                return None;
            }
            Some(IndexRow {
                last: f[1].trim().to_string(),
                first: f[2].trim().to_string(),
                filing_type: f[4].trim().to_string(),
                year: f[6].trim().parse().ok()?,
                filed: NaiveDate::parse_from_str(f[7].trim(), "%m/%d/%Y").ok()?,
                doc_id: f[8].trim().to_string(),
            })
        })
        .collect()
}

pub async fn fetch_index(ctx: &SourceCtx, year: i32) -> Result<Vec<IndexRow>> {
    let url = format!("{INDEX_URL}/{year}FD.zip");
    let bytes = ctx.http.get(&url).send().await?.error_for_status()?.bytes().await?;
    let cursor = std::io::Cursor::new(bytes);
    let mut zip = zip::ZipArchive::new(cursor).context("reading House index zip")?;
    let mut txt = Vec::new();
    zip.by_name(&format!("{year}FD.txt"))
        .context("index txt missing from zip")?
        .read_to_end(&mut txt)?;
    Ok(parse_index(&String::from_utf8_lossy(&txt)))
}

/// Runs `pdftotext -layout` on the PDF bytes.
pub async fn pdf_to_text(pdf: &[u8]) -> Result<String> {
    let tmp = tempfile::Builder::new().suffix(".pdf").tempfile()?;
    tokio::fs::write(tmp.path(), pdf).await?;
    let out = tokio::process::Command::new("pdftotext")
        .arg("-layout")
        .arg(tmp.path())
        .arg("-")
        .output()
        .await
        .context("running pdftotext (install poppler-utils)")?;
    if !out.status.success() {
        bail!("pdftotext failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A transaction row as printed in the PTR table.
#[derive(Debug, Clone, PartialEq)]
pub struct PtrRow {
    pub owner: Option<String>,
    pub asset: String,
    pub ticker: Option<String>,
    pub asset_type: Option<String>,
    pub side: Side,
    pub tx_date: NaiveDate,
    pub notified: NaiveDate,
    pub amount_low: f64,
    pub amount_high: f64,
}

static ROW_START: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:(SP|JT|DC)\s+)?(.*?)\s+(P|S \(partial\)|S|E)\s+(\d{2}/\d{2}/\d{4})\s+(\d{2}/\d{2}/\d{4})\s*(.*)$")
        .unwrap()
});
// Small-caps labels ("Filing Status:", "Description:") lose their lowercase
// letters in the text layer and come out as `F      S      : New`.
static LABEL_LINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*[A-Z](?:\s+[A-Z])*\s*:").unwrap());
static TICKER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\(([A-Z][A-Z0-9.\-]{0,9})\)\s*\[([A-Z]{2,3})\]").unwrap());
static ASSET_TYPE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([A-Z]{2,3})\]").unwrap());
static MONEY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$[\d,]+").unwrap());

/// Parses the text layer of an electronically filed PTR into rows.
pub fn parse_ptr_text(text: &str) -> Vec<PtrRow> {
    let lines: Vec<&str> = text.lines().collect();
    let mut rows = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let Some(caps) = ROW_START.captures(lines[i]) else {
            i += 1;
            continue;
        };
        let owner = caps.get(1).map(|m| m.as_str().to_string());
        let mut asset = caps[2].trim().to_string();
        let side = Side::parse(&caps[3]).unwrap_or(Side::Exchange);
        let tx_date = NaiveDate::parse_from_str(&caps[4], "%m/%d/%Y");
        let notified = NaiveDate::parse_from_str(&caps[5], "%m/%d/%Y");
        let mut tail = caps[6].to_string();

        // Gather the continuation lines: asset-name wraps and the second half
        // of the amount range, until the next row / blank line / page header.
        let mut j = i + 1;
        while j < lines.len() {
            let l = lines[j];
            let t = l.trim();
            if t.is_empty() || ROW_START.is_match(l) || t.starts_with("* For the complete") {
                break;
            }
            // A page break can split a row; the repeated column header
            // (`ID Owner Asset...`, `Type Date Gains >`, `$200?`) is skipped.
            if t.starts_with("ID ") || t.starts_with("Type ") || t == "$200?" {
                j += 1;
                continue;
            }
            if !LABEL_LINE.is_match(l) {
                // Left part continues the asset name, right part (3+ spaces
                // away) continues the amount column.
                let mut parts = t.splitn(2, "   ");
                let left = parts.next().unwrap_or("").trim();
                let right = parts.next().unwrap_or("").trim();
                if !left.is_empty() {
                    asset.push(' ');
                    asset.push_str(left);
                }
                if !right.is_empty() {
                    tail.push(' ');
                    tail.push_str(right);
                }
            }
            j += 1;
        }

        let (tx_date, notified) = match (tx_date, notified) {
            (Ok(a), Ok(b)) => (a, b),
            _ => {
                i = j;
                continue;
            }
        };
        // Amount tokens appear in the row before any description text, and a
        // wrapped asset name may carry the high bound.
        let joined = format!("{tail} {asset}");
        let money: Vec<&str> = MONEY.find_iter(&joined).map(|m| m.as_str()).take(2).collect();
        let (amount_low, amount_high) = parse_amount_range(&money.join(" - ")).unwrap_or((0.0, 0.0));

        let (ticker, asset_type) = match TICKER.captures(&asset) {
            Some(c) => (Some(c[1].to_string()), Some(c[2].to_string())),
            None => (None, ASSET_TYPE.captures(&asset).map(|c| c[1].to_string())),
        };
        // Strip the trailing amount text that leaked into the asset column
        // when the amount wrapped, then tidy whitespace.
        let asset = MONEY.replace_all(&asset, "");
        let asset = asset
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .trim_end_matches(" -")
            .to_string();

        rows.push(PtrRow {
            owner,
            asset,
            ticker,
            asset_type,
            side,
            tx_date,
            notified,
            amount_low,
            amount_high,
        });
        i = j;
    }
    rows
}

pub fn to_disclosures(row: &IndexRow, ptr_rows: &[PtrRow]) -> Vec<Disclosure> {
    let politician = display_name(&row.first, &row.last);
    ptr_rows
        .iter()
        .map(|r| Disclosure {
            source: "house".into(),
            chamber: Chamber::House,
            politician: politician.clone(),
            ticker: r.ticker.clone(),
            asset: r.asset.clone(),
            asset_type: r.asset_type.clone(),
            side: r.side,
            tx_date: r.tx_date,
            filed_date: row.filed,
            amount_low: r.amount_low,
            amount_high: r.amount_high,
            owner: r.owner.clone(),
            url: ptr_url(row.year, &row.doc_id),
        })
        .collect()
}

/// One polling round: fetch the index, download every PTR we have not seen
/// and store the transactions it discloses.
pub async fn poll(ctx: &SourceCtx) -> Result<PollReport> {
    let mut report = PollReport::default();
    let year = Utc::now().year();
    let cutoff = Utc::now().date_naive() - chrono::Duration::days(ctx.max_age_days);
    let first_run = ctx.db.count_filings("house")? == 0;

    let mut index = fetch_index(ctx, year).await?;
    // January: last year's filings can still trickle in.
    if Utc::now().month() == 1
        && let Ok(prev) = fetch_index(ctx, year - 1).await
    {
        index.extend(prev);
    }
    for row in index.iter().filter(|r| r.filing_type == "P") {
        if ctx.db.filing_seen(&row.doc_id)? {
            continue;
        }
        report.filings += 1;
        if first_run && row.filed < cutoff {
            // Do not download a year of history on the first run.
            ctx.db
                .mark_filing(&row.doc_id, "house", "skipped", Some("older than max_disclosure_age_days"))?;
            continue;
        }
        let url = ptr_url(row.year, &row.doc_id);
        let pdf = match ctx.http.get(&url).send().await.and_then(|r| r.error_for_status()) {
            Ok(r) => r.bytes().await?,
            Err(e) => {
                warn!(doc_id = %row.doc_id, "house PTR download failed: {e}");
                report.errors += 1;
                continue;
            }
        };
        let text = match pdf_to_text(&pdf).await {
            Ok(t) => t,
            Err(e) => {
                warn!(doc_id = %row.doc_id, "pdftotext failed: {e}");
                report.errors += 1;
                continue;
            }
        };
        let rows = parse_ptr_text(&text);
        if rows.is_empty() {
            let note = if text.contains("Transaction") {
                "no rows parsed"
            } else {
                "no text layer (paper filing?)"
            };
            ctx.db.mark_filing(&row.doc_id, "house", "unparsed", Some(note))?;
            info!(doc_id = %row.doc_id, filer = %row.last, "house PTR {note}");
            continue;
        }
        for d in to_disclosures(row, &rows) {
            if ctx.db.insert_disclosure(&d)? {
                report.disclosures += 1;
            }
        }
        ctx.db
            .mark_filing(&row.doc_id, "house", "parsed", Some(&format!("{} rows", rows.len())))?;
        info!(doc_id = %row.doc_id, filer = %row.last, rows = rows.len(), "house PTR parsed");
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_index_with_crlf() {
        let txt = "Prefix\tLast\tFirst\tSuffix\tFilingType\tStateDst\tYear\tFilingDate\tDocID\r\nHon.\tPelosi\tNancy\t\tP\tCA11\t2026\t8/21/2026\t20035143\r\n";
        let rows = parse_index(txt);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].doc_id, "20035143");
        assert_eq!(rows[0].filed, NaiveDate::from_ymd_opt(2026, 8, 21).unwrap());
        assert_eq!(rows[0].filing_type, "P");
    }

    #[test]
    fn parses_pelosi_ptr() {
        let text = include_str!("../../tests/fixtures/house_ptr_20035143.txt");
        let rows = parse_ptr_text(text);
        assert_eq!(rows.len(), 7, "{rows:#?}");
        let be = &rows[0];
        assert_eq!(be.owner.as_deref(), Some("SP"));
        assert_eq!(be.ticker.as_deref(), Some("BE"));
        assert_eq!(be.asset_type.as_deref(), Some("ST"));
        assert_eq!(be.side, Side::Purchase);
        assert_eq!(be.tx_date, NaiveDate::from_ymd_opt(2026, 7, 24).unwrap());
        assert_eq!((be.amount_low, be.amount_high), (1_000_001.0, 5_000_000.0));
        assert!(
            be.asset.starts_with("Bloom Energy Corporation Class A Common Stock"),
            "{}",
            be.asset
        );
        // Options rows keep their ticker but carry the OP type so the engine skips them.
        assert_eq!(rows[1].asset_type.as_deref(), Some("OP"));
        assert_eq!(rows[4].ticker.as_deref(), Some("INTC"));
        // The LLC row has no ticker.
        let llc = rows.last().unwrap();
        assert_eq!(llc.ticker, None);
        assert_eq!(llc.asset_type.as_deref(), Some("AB"));
        assert_eq!((llc.amount_low, llc.amount_high), (500_001.0, 1_000_000.0));
    }

    #[test]
    fn parses_treasury_bill_without_owner() {
        let text = include_str!("../../tests/fixtures/house_ptr_20034984.txt");
        let rows = parse_ptr_text(text);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].owner, None);
        assert_eq!(rows[0].ticker, None);
        assert_eq!(rows[0].asset_type.as_deref(), Some("GS"));
        assert_eq!((rows[0].amount_low, rows[0].amount_high), (15_001.0, 50_000.0));
    }

    #[test]
    fn parses_sales_across_pages() {
        let text = include_str!("../../tests/fixtures/house_ptr_20034807.txt");
        let rows = parse_ptr_text(text);
        assert_eq!(rows.len(), 13, "{rows:#?}");
        assert!(rows.iter().all(|r| r.side == Side::Sale));
        assert!(rows.iter().all(|r| r.asset_type.as_deref() == Some("ST")));
        let tickers: Vec<&str> = rows.iter().filter_map(|r| r.ticker.as_deref()).collect();
        assert!(
            tickers.contains(&"GOOGL") && tickers.contains(&"IBM") && tickers.contains(&"MSFT"),
            "{tickers:?}"
        );
        assert_eq!(
            rows.iter()
                .filter(|r| (r.amount_low, r.amount_high) == (1001.0, 15000.0))
                .count(),
            12
        );
        // The wrapped `$15,001 -` / `$50,000` range on the last page.
        let tpr = rows.iter().find(|r| r.ticker.as_deref() == Some("TPR")).unwrap();
        assert_eq!((tpr.amount_low, tpr.amount_high), (15001.0, 50000.0));
    }

    #[test]
    fn survives_page_break_inside_a_row() {
        let text = include_str!("../../tests/fixtures/house_ptr_20035491.txt");
        let rows = parse_ptr_text(text);
        assert_eq!(rows.len(), 99);
        // Every equity row keeps its ticker even when the wrapped asset name
        // lands on the next page, and no header `$200?` leaks into an amount.
        assert!(
            rows.iter()
                .filter(|r| r.asset_type.as_deref() == Some("ST"))
                .all(|r| r.ticker.is_some())
        );
        assert!(
            rows.iter().all(|r| r.asset_type.is_some()),
            "every row has a bracketed asset type"
        );
        assert!(rows.iter().all(|r| r.amount_low >= 1000.0 && r.amount_high > r.amount_low));
        let split = rows
            .iter()
            .find(|r| r.ticker.as_deref() == Some("DVN") && r.tx_date == NaiveDate::from_ymd_opt(2026, 9, 2).unwrap())
            .unwrap();
        assert_eq!((split.amount_low, split.amount_high), (100_001.0, 250_000.0));
        assert_eq!(split.side, Side::SalePartial);
    }

    #[test]
    fn parses_partial_sale_with_single_space_gap() {
        let text = include_str!("../../tests/fixtures/house_ptr_partial.txt");
        let rows = parse_ptr_text(text);
        let fang = rows.iter().find(|r| r.ticker.as_deref() == Some("FANG")).expect("FANG row");
        assert_eq!(fang.side, Side::SalePartial);
        assert!(fang.asset.starts_with("Diamondback Energy"));
        assert!(rows.iter().filter(|r| r.side == Side::SalePartial).count() >= 2);
    }
}
