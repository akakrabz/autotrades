//! Senate periodic transaction reports from the electronic Financial
//! Disclosure system (eFD).
//!
//! eFD is a Django site: you must accept the prohibition-on-use agreement
//! (a CSRF-protected form POST) before the DataTables JSON endpoint answers.
//! Electronic PTRs render as an HTML table with a real *Ticker* column;
//! paper PTRs are scanned images and are recorded as `unparsed`.

use anyhow::{Context, Result, bail};
use chrono::{NaiveDate, Utc};
use regex::Regex;
use reqwest::cookie::{CookieStore, Jar};
use scraper::{Html, Selector};
use std::sync::{Arc, LazyLock};
use tracing::{info, warn};

use super::{PollReport, SourceCtx, USER_AGENT};
use crate::model::{Chamber, Disclosure, Side, display_name, parse_amount_range};

const BASE: &str = "https://efdsearch.senate.gov";

static CSRF_INPUT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"name="csrfmiddlewaretoken"\s+value="([^"]+)""#).unwrap());
static PTR_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"href="(/search/view/(ptr|paper)/([0-9a-f\-]{36})/)""#).unwrap());

/// A filing listed by the search endpoint.
#[derive(Debug, Clone)]
pub struct SenateFiling {
    pub first: String,
    pub last: String,
    pub uuid: String,
    pub electronic: bool,
    pub filed: NaiveDate,
}

impl SenateFiling {
    pub fn url(&self) -> String {
        let kind = if self.electronic { "ptr" } else { "paper" };
        format!("{BASE}/search/view/{kind}/{}/", self.uuid)
    }
}

/// An authenticated eFD session (cookies + CSRF token).
pub struct EfdSession {
    http: reqwest::Client,
    csrf: String,
}

impl EfdSession {
    pub async fn open() -> Result<Self> {
        let jar = Arc::new(Jar::default());
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .cookie_provider(jar.clone())
            .timeout(std::time::Duration::from_secs(60))
            .build()?;
        let home = format!("{BASE}/search/home/");
        let html = http.get(&home).send().await?.error_for_status()?.text().await?;
        let token = CSRF_INPUT
            .captures(&html)
            .map(|c| c[1].to_string())
            .context("eFD home: csrf token not found")?;
        http.post(&home)
            .header("Referer", &home)
            .form(&[("prohibition_agreement", "1"), ("csrfmiddlewaretoken", token.as_str())])
            .send()
            .await?;
        // After the agreement Django rotates the token; the cookie is authoritative.
        let url: reqwest::Url = BASE.parse()?;
        let cookies = jar
            .cookies(&url)
            .map(|v| v.to_str().unwrap_or("").to_string())
            .unwrap_or_default();
        let csrf = cookies
            .split(';')
            .map(str::trim)
            .find_map(|c| c.strip_prefix("csrftoken="))
            .map(str::to_string)
            .context("eFD: csrftoken cookie missing after accepting agreement")?;
        Ok(Self { http, csrf })
    }

    /// Lists senator PTRs submitted on or after `since`.
    pub async fn list_ptrs(&self, since: NaiveDate) -> Result<Vec<SenateFiling>> {
        let mut out = Vec::new();
        let page = 100;
        let mut start = 0;
        loop {
            let start_s = start.to_string();
            let page_s = page.to_string();
            let since_s = format!("{} 00:00:00", since.format("%m/%d/%Y"));
            let form = [
                ("draw", "1"),
                ("start", start_s.as_str()),
                ("length", page_s.as_str()),
                ("report_types", "[11]"), // periodic transaction reports
                ("filer_types", "[1]"),   // senators
                ("submitted_start_date", since_s.as_str()),
                ("submitted_end_date", ""),
                ("order[0][column]", "4"),
                ("order[0][dir]", "desc"),
                ("columns[4][data]", "4"),
            ];
            let resp: serde_json::Value = self
                .http
                .post(format!("{BASE}/search/report/data/"))
                .header("Referer", format!("{BASE}/search/"))
                .header("X-CSRFToken", &self.csrf)
                .header("X-Requested-With", "XMLHttpRequest")
                .form(&form)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            let rows = resp["data"].as_array().cloned().unwrap_or_default();
            let n = rows.len();
            for r in rows {
                if let Some(f) = parse_search_row(&r) {
                    out.push(f);
                }
            }
            if n < page {
                break;
            }
            start += page;
        }
        Ok(out)
    }

    pub async fn fetch(&self, url: &str) -> Result<String> {
        Ok(self.http.get(url).send().await?.error_for_status()?.text().await?)
    }
}

fn parse_search_row(r: &serde_json::Value) -> Option<SenateFiling> {
    let cell = |i: usize| r.get(i).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let link_html = cell(3);
    let link = PTR_LINK.captures(&link_html)?;
    Some(SenateFiling {
        first: cell(0),
        last: cell(1),
        electronic: &link[2] == "ptr",
        uuid: link[3].to_string(),
        filed: NaiveDate::parse_from_str(cell(4).trim(), "%m/%d/%Y").ok()?,
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct SenateRow {
    pub tx_date: NaiveDate,
    pub owner: Option<String>,
    pub ticker: Option<String>,
    pub asset: String,
    pub asset_type: String,
    pub side: Side,
    pub amount_low: f64,
    pub amount_high: f64,
}

/// Parses the transactions table of an electronic PTR page.
pub fn parse_ptr_html(html: &str) -> Vec<SenateRow> {
    let doc = Html::parse_document(html);
    let table = Selector::parse("table").unwrap();
    let tr = Selector::parse("tr").unwrap();
    let td = Selector::parse("td").unwrap();
    let mut rows = Vec::new();
    for t in doc.select(&table) {
        for r in t.select(&tr) {
            let cells: Vec<String> = r
                .select(&td)
                .map(|c| c.text().collect::<Vec<_>>().join(" "))
                .map(|s| squash(&s))
                .collect();
            if cells.len() < 8 {
                continue;
            }
            let Ok(tx_date) = NaiveDate::parse_from_str(&cells[1], "%m/%d/%Y") else {
                continue;
            };
            let Some(side) = Side::parse(&cells[6]) else { continue };
            let ticker = match cells[3].trim() {
                "" | "--" | "N/A" => None,
                t => Some(t.to_ascii_uppercase()),
            };
            let (amount_low, amount_high) = parse_amount_range(&cells[7]).unwrap_or((0.0, 0.0));
            rows.push(SenateRow {
                tx_date,
                owner: Some(cells[2].clone()).filter(|s| !s.is_empty()),
                ticker,
                asset: cells[4].clone(),
                asset_type: cells[5].clone(),
                side,
                amount_low,
                amount_high,
            });
        }
    }
    rows
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn to_disclosures(f: &SenateFiling, rows: &[SenateRow]) -> Vec<Disclosure> {
    let politician = display_name(&f.first, &f.last);
    rows.iter()
        .map(|r| Disclosure {
            source: "senate".into(),
            chamber: Chamber::Senate,
            politician: politician.clone(),
            ticker: r.ticker.clone(),
            asset: r.asset.clone(),
            asset_type: Some(r.asset_type.clone()),
            side: r.side,
            tx_date: r.tx_date,
            filed_date: f.filed,
            amount_low: r.amount_low,
            amount_high: r.amount_high,
            owner: r.owner.clone(),
            url: f.url(),
        })
        .collect()
}

pub async fn poll(ctx: &SourceCtx) -> Result<PollReport> {
    let mut report = PollReport::default();
    let since = Utc::now().date_naive() - chrono::Duration::days(ctx.max_age_days);
    let session = EfdSession::open().await?;
    let filings = session.list_ptrs(since).await?;
    if filings.is_empty() {
        bail!("eFD returned no PTR filings since {since}; the search endpoint may have changed");
    }
    for f in filings {
        if ctx.db.filing_seen(&f.uuid)? {
            continue;
        }
        report.filings += 1;
        if !f.electronic {
            ctx.db.mark_filing(&f.uuid, "senate", "unparsed", Some("paper filing"))?;
            info!(uuid = %f.uuid, filer = %f.last, "senate paper PTR skipped");
            continue;
        }
        let html = match session.fetch(&f.url()).await {
            Ok(h) => h,
            Err(e) => {
                warn!(uuid = %f.uuid, "senate PTR download failed: {e}");
                report.errors += 1;
                continue;
            }
        };
        let rows = parse_ptr_html(&html);
        for d in to_disclosures(&f, &rows) {
            if ctx.db.insert_disclosure(&d)? {
                report.disclosures += 1;
            }
        }
        let status = if rows.is_empty() { "unparsed" } else { "parsed" };
        ctx.db
            .mark_filing(&f.uuid, "senate", status, Some(&format!("{} rows", rows.len())))?;
        info!(uuid = %f.uuid, filer = %f.last, rows = rows.len(), "senate PTR {status}");
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_search_rows() {
        let row = serde_json::json!([
            "Richard",
            "Blumenthal",
            "Blumenthal, Richard (Senator)",
            "<a href=\"/search/view/ptr/9e2ff733-aeac-4ce8-872c-3d6b7913da88/\" target=\"_blank\">Periodic Transaction Report for 09/28/2026</a>",
            "09/28/2026"
        ]);
        let f = parse_search_row(&row).unwrap();
        assert!(f.electronic);
        assert_eq!(f.uuid, "9e2ff733-aeac-4ce8-872c-3d6b7913da88");
        assert_eq!(f.filed, NaiveDate::from_ymd_opt(2026, 9, 28).unwrap());
        let paper = serde_json::json!([
            "A",
            "B",
            "B, A",
            "<a href=\"/search/view/paper/00000000-0000-0000-0000-000000000000/\">x</a>",
            "01/02/2026"
        ]);
        assert!(!parse_search_row(&paper).unwrap().electronic);
    }

    #[test]
    fn parses_ptr_table() {
        let html = include_str!("../../tests/fixtures/senate_ptr_table.html");
        let rows = parse_ptr_html(html);
        assert_eq!(rows.len(), 33, "{rows:#?}");
        let r = &rows[0];
        assert_eq!(r.tx_date, NaiveDate::from_ymd_opt(2026, 9, 11).unwrap());
        assert_eq!(r.owner.as_deref(), Some("Spouse"));
        assert_eq!(r.ticker, None);
        assert_eq!(r.side, Side::Purchase);
        assert_eq!((r.amount_low, r.amount_high), (1001.0, 15000.0));
        assert_eq!(r.asset_type, "Other");
    }

    #[test]
    fn parses_stock_row_with_ticker() {
        let html = r#"<table><tbody><tr>
            <td>1</td><td>03/04/2026</td><td>Self</td><td><a href="x">aapl</a></td>
            <td>Apple Inc</td><td>Stock</td><td>Sale (Partial)</td><td>$15,001 - $50,000</td><td>--</td>
        </tr></tbody></table>"#;
        let rows = parse_ptr_html(html);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].ticker.as_deref(), Some("AAPL"));
        assert_eq!(rows[0].side, Side::SalePartial);
        assert_eq!(rows[0].asset_type, "Stock");
        assert_eq!((rows[0].amount_low, rows[0].amount_high), (15001.0, 50000.0));
    }
}
