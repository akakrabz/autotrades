//! SEC EDGAR Form 13F-HR: quarterly holdings of institutional managers.
//!
//! Flow per tracked CIK: `data.sec.gov/submissions/CIK##########.json` lists
//! recent filings; a new `13F-HR` accession has an *information table* XML in
//! its Archives directory (any `.xml` other than `primary_doc.xml`). Rows are
//! keyed by CUSIP, so tickers are resolved through OpenFIGI and cached.
//!
//! SEC fair-access rules: identify yourself in `User-Agent` and stay under 10
//! requests/second.

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::time::Duration;
use tracing::{info, warn};

use super::{PollReport, SourceCtx};
use crate::model::Holding13F;

const OPENFIGI_URL: &str = "https://api.openfigi.com/v3/mapping";

#[derive(Debug, Clone)]
pub struct Filing13F {
    pub accession: String,
    pub period: String,
}

fn sec_get(ctx: &SourceCtx, url: &str) -> Result<reqwest::RequestBuilder> {
    // www.sec.gov answers 403 unless the User-Agent carries a contact email.
    let ua = ctx
        .sec_user_agent
        .as_deref()
        .filter(|ua| ua.contains('@'))
        .context("SEC_USER_AGENT / sources.sec_user_agent must be set to \"<app name> <your@email>\" for EDGAR access")?;
    Ok(ctx.http.get(url).header("User-Agent", ua))
}

/// Newest original (non-amended) 13F-HR filings, most recent first.
pub fn parse_submissions(json: &serde_json::Value) -> Vec<Filing13F> {
    let recent = &json["filings"]["recent"];
    let arr = |k: &str| recent[k].as_array().cloned().unwrap_or_default();
    let (forms, accs, periods) = (arr("form"), arr("accessionNumber"), arr("reportDate"));
    let mut out = Vec::new();
    for (i, form) in forms.iter().enumerate() {
        if form.as_str() != Some("13F-HR") {
            continue;
        }
        let (Some(acc), Some(period)) = (accs.get(i), periods.get(i)) else {
            continue;
        };
        out.push(Filing13F {
            accession: acc.as_str().unwrap_or_default().to_string(),
            period: period.as_str().unwrap_or_default().to_string(),
        });
    }
    out
}

/// Parses an information table, aggregating shares/value per CUSIP and
/// skipping options (`putCall`) and principal-amount (debt) rows.
pub fn parse_info_table(xml: &str, cik: u64, accession: &str, period: &str) -> Result<Vec<Holding13F>> {
    let doc = roxmltree::Document::parse(xml).context("13F information table is not valid XML")?;
    let text_of = |n: roxmltree::Node, name: &str| -> Option<String> {
        n.descendants()
            .find(|c| c.is_element() && c.tag_name().name() == name)
            .and_then(|c| c.text())
            .map(|t| t.trim().to_string())
    };
    let mut by_cusip: BTreeMap<String, Holding13F> = BTreeMap::new();
    for n in doc
        .descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "infoTable")
    {
        if text_of(n, "putCall").is_some() {
            continue;
        }
        if text_of(n, "sshPrnamtType").as_deref() != Some("SH") {
            continue;
        }
        let Some(cusip) = text_of(n, "cusip").map(|c| c.to_ascii_uppercase()) else {
            continue;
        };
        let shares: f64 = text_of(n, "sshPrnamt")
            .and_then(|s| s.replace(',', "").parse().ok())
            .unwrap_or(0.0);
        let value: f64 = text_of(n, "value")
            .and_then(|s| s.replace(',', "").parse().ok())
            .unwrap_or(0.0);
        let entry = by_cusip.entry(cusip.clone()).or_insert_with(|| Holding13F {
            cik,
            accession: accession.to_string(),
            period: period.to_string(),
            cusip,
            issuer: text_of(n, "nameOfIssuer").unwrap_or_default(),
            shares: 0.0,
            value: 0.0,
            ticker: None,
        });
        entry.shares += shares;
        entry.value += value;
    }
    Ok(by_cusip.into_values().collect())
}

async fn fetch_info_table(ctx: &SourceCtx, cik: u64, accession: &str) -> Result<String> {
    let nodash = accession.replace('-', "");
    let dir = format!("https://www.sec.gov/Archives/edgar/data/{cik}/{nodash}");
    let index: serde_json::Value = sec_get(ctx, &format!("{dir}/index.json"))?
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let names: Vec<String> = index["directory"]["item"]
        .as_array()
        .map(|a| a.iter().filter_map(|i| i["name"].as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let mut candidates: Vec<&String> = names
        .iter()
        .filter(|n| n.to_ascii_lowercase().ends_with(".xml") && !n.eq_ignore_ascii_case("primary_doc.xml"))
        .collect();
    candidates.sort_by_key(|n| !n.to_ascii_lowercase().contains("info"));
    for name in candidates {
        tokio::time::sleep(Duration::from_millis(150)).await;
        let body = sec_get(ctx, &format!("{dir}/{name}"))?
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        if body.contains("informationTable") {
            return Ok(body);
        }
    }
    bail!("no information table found in {dir}")
}

/// Resolves CUSIPs to US tickers through OpenFIGI, using the database as a
/// cache. Unlisted CUSIPs are cached as `None` so they are not retried.
pub async fn map_cusips(ctx: &SourceCtx, holdings: &mut [Holding13F]) -> Result<()> {
    let mut missing: Vec<String> = Vec::new();
    for h in holdings.iter_mut() {
        match ctx.db.cusip_lookup(&h.cusip)? {
            Some(t) => h.ticker = t,
            None => missing.push(h.cusip.clone()),
        }
    }
    missing.sort();
    missing.dedup();
    let batch = if ctx.openfigi_key.is_some() { 100 } else { 10 };
    for chunk in missing.chunks(batch) {
        let jobs: Vec<serde_json::Value> = chunk
            .iter()
            .map(|c| serde_json::json!({"idType": "ID_CUSIP", "idValue": c, "exchCode": "US"}))
            .collect();
        let mut req = ctx.http.post(OPENFIGI_URL).json(&jobs);
        if let Some(k) = &ctx.openfigi_key {
            req = req.header("X-OPENFIGI-APIKEY", k);
        }
        let resp = req.send().await?;
        if resp.status().as_u16() == 429 {
            warn!("openfigi rate limited; remaining CUSIPs will be mapped next round");
            break;
        }
        let results: Vec<serde_json::Value> = resp.error_for_status()?.json().await?;
        for (cusip, r) in chunk.iter().zip(results.iter()) {
            let hit = r["data"].as_array().and_then(|d| {
                d.iter()
                    .find(|x| matches!(x["securityType"].as_str(), Some("Common Stock") | Some("ETP") | Some("REIT")))
                    .or_else(|| d.first())
            });
            let ticker = hit.and_then(|x| x["ticker"].as_str()).map(str::to_string);
            let name = hit.and_then(|x| x["name"].as_str()).map(str::to_string);
            ctx.db.cusip_store(cusip, ticker.as_deref(), name.as_deref())?;
            for h in holdings.iter_mut().filter(|h| &h.cusip == cusip) {
                h.ticker = ticker.clone();
            }
        }
        // Keyless tier: 25 requests/minute.
        tokio::time::sleep(Duration::from_millis(if ctx.openfigi_key.is_some() { 300 } else { 2500 })).await;
    }
    Ok(())
}

pub async fn poll(ctx: &SourceCtx) -> Result<PollReport> {
    let mut report = PollReport::default();
    for &cik in &ctx.ciks {
        let url = format!("https://data.sec.gov/submissions/CIK{cik:010}.json");
        let subs: serde_json::Value = match sec_get(ctx, &url)?.send().await.and_then(|r| r.error_for_status()) {
            Ok(r) => r.json().await?,
            Err(e) => {
                warn!(cik, "EDGAR submissions fetch failed: {e}");
                report.errors += 1;
                continue;
            }
        };
        let Some(latest) = parse_submissions(&subs).into_iter().next() else {
            warn!(cik, "no 13F-HR filings found");
            continue;
        };
        if ctx.db.filing_seen(&latest.accession)? {
            continue;
        }
        report.filings += 1;
        let xml = match fetch_info_table(ctx, cik, &latest.accession).await {
            Ok(x) => x,
            Err(e) => {
                warn!(cik, accession = %latest.accession, "13F info table fetch failed: {e}");
                report.errors += 1;
                continue;
            }
        };
        let mut holdings = parse_info_table(&xml, cik, &latest.accession, &latest.period)?;
        map_cusips(ctx, &mut holdings).await?;
        let mapped = holdings.iter().filter(|h| h.ticker.is_some()).count();
        ctx.db.insert_holdings(&holdings)?;
        ctx.db.mark_filing(
            &latest.accession,
            "13f",
            "parsed",
            Some(&format!("{} holdings, {mapped} mapped", holdings.len())),
        )?;
        ctx.db.set_kv(&format!("13f_rebalance_pending:{cik}"), &latest.accession)?;
        info!(cik, accession = %latest.accession, period = %latest.period, holdings = holdings.len(), mapped, "new 13F-HR stored");
        report.holdings += 1;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<informationTable xmlns="http://www.sec.gov/edgar/document/thirteenf/informationtable">
  <infoTable><nameOfIssuer>ALLY FINL INC</nameOfIssuer><titleOfClass>COM</titleOfClass><cusip>02005N100</cusip>
    <value>577211815</value><shrsOrPrnAmt><sshPrnamt>12561737</sshPrnamt><sshPrnamtType>SH</sshPrnamtType></shrsOrPrnAmt>
    <investmentDiscretion>DFND</investmentDiscretion><votingAuthority><Sole>12561737</Sole><Shared>0</Shared><None>0</None></votingAuthority></infoTable>
  <infoTable><nameOfIssuer>ALLY FINL INC</nameOfIssuer><titleOfClass>COM</titleOfClass><cusip>02005N100</cusip>
    <value>128838056</value><shrsOrPrnAmt><sshPrnamt>2803875</sshPrnamt><sshPrnamtType>SH</sshPrnamtType></shrsOrPrnAmt>
    <investmentDiscretion>DFND</investmentDiscretion><votingAuthority><Sole>2803875</Sole><Shared>0</Shared><None>0</None></votingAuthority></infoTable>
  <infoTable><nameOfIssuer>APPLE INC</nameOfIssuer><titleOfClass>COM</titleOfClass><cusip>037833100</cusip>
    <value>1000</value><shrsOrPrnAmt><sshPrnamt>10</sshPrnamt><sshPrnamtType>SH</sshPrnamtType></shrsOrPrnAmt><putCall>Put</putCall>
    <investmentDiscretion>SOLE</investmentDiscretion><votingAuthority><Sole>10</Sole><Shared>0</Shared><None>0</None></votingAuthority></infoTable>
  <infoTable><nameOfIssuer>US TREASURY</nameOfIssuer><titleOfClass>NOTE</titleOfClass><cusip>912828XX1</cusip>
    <value>5000</value><shrsOrPrnAmt><sshPrnamt>5000</sshPrnamt><sshPrnamtType>PRN</sshPrnamtType></shrsOrPrnAmt>
    <investmentDiscretion>SOLE</investmentDiscretion><votingAuthority><Sole>0</Sole><Shared>0</Shared><None>0</None></votingAuthority></infoTable>
</informationTable>"#;

    #[test]
    fn aggregates_per_cusip_and_skips_options_and_debt() {
        let h = parse_info_table(XML, 1067983, "0001193125-26-352200", "2026-06-30").unwrap();
        assert_eq!(h.len(), 1, "{h:#?}");
        assert_eq!(h[0].cusip, "02005N100");
        assert_eq!(h[0].shares, 12561737.0 + 2803875.0);
        assert_eq!(h[0].value, 577211815.0 + 128838056.0);
        assert_eq!(h[0].issuer, "ALLY FINL INC");
    }

    #[test]
    fn picks_original_13f_hr_filings() {
        let subs = serde_json::json!({"filings": {"recent": {
            "form": ["13F-HR/A", "13F-HR", "10-K", "13F-HR"],
            "accessionNumber": ["a", "b", "c", "d"],
            "filingDate": ["2026-09-01", "2026-08-14", "2026-02-01", "2026-05-15"],
            "reportDate": ["2026-06-30", "2026-06-30", "2025-12-31", "2026-03-31"]
        }}});
        let f = parse_submissions(&subs);
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].accession, "b");
        assert_eq!(f[0].period, "2026-06-30");
    }
}
