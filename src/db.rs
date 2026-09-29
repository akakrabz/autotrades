//! SQLite persistence. A single connection behind a mutex is plenty for one
//! operator; every call is a handful of microseconds and never awaits while
//! holding the lock.

use anyhow::{Context, Result};
use chrono::{NaiveDate, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::sync::Mutex;

use crate::model::{Chamber, Disclosure, Holding13F, NewsSignal, Side, SignalDirection};

pub struct Db {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS kv (
    k TEXT PRIMARY KEY,
    v TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS filings (
    id        TEXT PRIMARY KEY,   -- house DocID / senate uuid / 13F accession
    source    TEXT NOT NULL,
    status    TEXT NOT NULL,      -- parsed | unparsed | skipped
    note      TEXT,
    seen_at   TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS disclosures (
    key         TEXT PRIMARY KEY,
    source      TEXT NOT NULL,
    chamber     TEXT NOT NULL,
    politician  TEXT NOT NULL,
    ticker      TEXT,
    asset       TEXT NOT NULL,
    asset_type  TEXT,
    side        TEXT NOT NULL,
    tx_date     TEXT NOT NULL,
    filed_date  TEXT NOT NULL,
    amount_low  REAL NOT NULL,
    amount_high REAL NOT NULL,
    owner       TEXT,
    url         TEXT NOT NULL,
    seen_at     TEXT NOT NULL,
    processed   INTEGER NOT NULL DEFAULT 0,
    attempts    INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS disclosures_seen ON disclosures(seen_at DESC);
CREATE TABLE IF NOT EXISTS holdings_13f (
    cik       INTEGER NOT NULL,
    accession TEXT NOT NULL,
    period    TEXT NOT NULL,
    cusip     TEXT NOT NULL,
    issuer    TEXT NOT NULL,
    shares    REAL NOT NULL,
    value     REAL NOT NULL,
    ticker    TEXT,
    PRIMARY KEY (accession, cusip)
);
CREATE TABLE IF NOT EXISTS cusip_map (
    cusip  TEXT PRIMARY KEY,
    ticker TEXT,                  -- NULL = looked up, no listing found
    name   TEXT
);
CREATE TABLE IF NOT EXISTS pilot_state (
    pilot_id       TEXT PRIMARY KEY,
    enabled        INTEGER NOT NULL,
    allocation_usd REAL
);
CREATE TABLE IF NOT EXISTS positions (
    pilot_id   TEXT NOT NULL,
    ticker     TEXT NOT NULL,
    qty        REAL NOT NULL,
    cost_basis REAL NOT NULL,     -- total USD paid for the open quantity
    PRIMARY KEY (pilot_id, ticker)
);
CREATE TABLE IF NOT EXISTS orders (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    ts          TEXT NOT NULL,
    pilot_id    TEXT NOT NULL,
    ticker      TEXT NOT NULL,
    side        TEXT NOT NULL,    -- BUY | SELL
    qty         REAL NOT NULL,
    price       REAL,             -- reference price used for sizing
    notional    REAL NOT NULL,
    ib_order_id INTEGER,
    status      TEXT NOT NULL,    -- dry_run | submitted | filled | cancelled | error
    fill_price  REAL,
    reason      TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS news_items (
    id     TEXT PRIMARY KEY,
    ts     TEXT NOT NULL,
    source TEXT NOT NULL,
    title  TEXT NOT NULL,
    link   TEXT,
    scored INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS signals (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    ts         TEXT NOT NULL,
    news_id    TEXT NOT NULL,
    ticker     TEXT NOT NULL,
    direction  TEXT NOT NULL,
    confidence REAL NOT NULL,
    rationale  TEXT NOT NULL,
    processed  INTEGER NOT NULL DEFAULT 0
);
"#;

/// Additive schema changes for databases created by earlier versions.
fn migrate(conn: &Connection) -> Result<()> {
    let has_column = |table: &str, col: &str| -> Result<bool> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let cols: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(cols.iter().any(|c| c == col))
    };
    if !has_column("disclosures", "attempts")? {
        conn.execute("ALTER TABLE disclosures ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0", [])?;
    }
    Ok(())
}

fn now() -> String {
    Utc::now().to_rfc3339()
}

#[derive(Debug, Clone)]
pub struct Position {
    pub pilot_id: String,
    pub ticker: String,
    pub qty: f64,
    pub cost_basis: f64,
}

#[derive(Debug, Clone)]
pub struct OrderRow {
    pub id: i64,
    pub ts: String,
    pub pilot_id: String,
    pub ticker: String,
    pub side: String,
    pub qty: f64,
    pub price: Option<f64>,
    pub notional: f64,
    pub ib_order_id: Option<i64>,
    pub status: String,
    pub fill_price: Option<f64>,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct NewOrder<'a> {
    pub pilot_id: &'a str,
    pub ticker: &'a str,
    pub side: &'a str,
    pub qty: f64,
    pub price: Option<f64>,
    pub notional: f64,
    pub ib_order_id: Option<i64>,
    pub status: &'a str,
    pub reason: &'a str,
}

#[derive(Debug, Clone)]
pub struct PilotState {
    pub enabled: bool,
    pub allocation_usd: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct NewsItem {
    pub id: String,
    pub source: String,
    pub title: String,
    pub link: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SignalRow {
    pub id: i64,
    pub ts: String,
    pub signal: NewsSignal,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening database {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---- kv -------------------------------------------------------------

    pub fn get_kv(&self, k: &str) -> Result<Option<String>> {
        Ok(self
            .lock()
            .query_row("SELECT v FROM kv WHERE k = ?1", [k], |r| r.get(0))
            .optional()?)
    }

    pub fn set_kv(&self, k: &str, v: &str) -> Result<()> {
        self.lock().execute(
            "INSERT INTO kv(k, v) VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            params![k, v],
        )?;
        Ok(())
    }

    pub fn del_kv(&self, k: &str) -> Result<()> {
        self.lock().execute("DELETE FROM kv WHERE k = ?1", [k])?;
        Ok(())
    }

    pub fn get_flag(&self, k: &str, default: bool) -> bool {
        match self.get_kv(k) {
            Ok(Some(v)) => v == "1",
            _ => default,
        }
    }

    pub fn set_flag(&self, k: &str, v: bool) -> Result<()> {
        self.set_kv(k, if v { "1" } else { "0" })
    }

    // ---- filings ----------------------------------------------------------

    pub fn filing_seen(&self, id: &str) -> Result<bool> {
        Ok(self
            .lock()
            .query_row("SELECT 1 FROM filings WHERE id = ?1", [id], |_| Ok(()))
            .optional()?
            .is_some())
    }

    pub fn mark_filing(&self, id: &str, source: &str, status: &str, note: Option<&str>) -> Result<()> {
        self.lock().execute(
            "INSERT INTO filings(id, source, status, note, seen_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET status = excluded.status, note = excluded.note",
            params![id, source, status, note, now()],
        )?;
        Ok(())
    }

    pub fn count_filings(&self, source: &str) -> Result<i64> {
        Ok(self
            .lock()
            .query_row("SELECT COUNT(*) FROM filings WHERE source = ?1", [source], |r| r.get(0))?)
    }

    // ---- disclosures --------------------------------------------------------

    /// Inserts a disclosure if its canonical key is new. Returns `true` when it
    /// was inserted (i.e. the engine has not seen this transaction before).
    pub fn insert_disclosure(&self, d: &Disclosure) -> Result<bool> {
        let n = self.lock().execute(
            "INSERT OR IGNORE INTO disclosures
             (key, source, chamber, politician, ticker, asset, asset_type, side, tx_date, filed_date,
              amount_low, amount_high, owner, url, seen_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                d.key(),
                d.source,
                d.chamber.to_string(),
                d.politician,
                d.ticker,
                d.asset,
                d.asset_type,
                d.side.as_str(),
                d.tx_date.to_string(),
                d.filed_date.to_string(),
                d.amount_low,
                d.amount_high,
                d.owner,
                d.url,
                now()
            ],
        )?;
        Ok(n == 1)
    }

    pub fn unprocessed_disclosures(&self) -> Result<Vec<(String, Disclosure)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT key, source, chamber, politician, ticker, asset, asset_type, side, tx_date, filed_date,
                    amount_low, amount_high, owner, url
             FROM disclosures WHERE processed = 0 ORDER BY filed_date, tx_date",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                Disclosure {
                    source: r.get(1)?,
                    chamber: match r.get::<_, String>(2)?.as_str() {
                        "senate" => Chamber::Senate,
                        _ => Chamber::House,
                    },
                    politician: r.get(3)?,
                    ticker: r.get(4)?,
                    asset: r.get(5)?,
                    asset_type: r.get(6)?,
                    side: Side::parse(&r.get::<_, String>(7)?).unwrap_or(Side::Exchange),
                    tx_date: parse_date(&r.get::<_, String>(8)?),
                    filed_date: parse_date(&r.get::<_, String>(9)?),
                    amount_low: r.get(10)?,
                    amount_high: r.get(11)?,
                    owner: r.get(12)?,
                    url: r.get(13)?,
                },
            ))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Records a failed execution attempt and returns the new attempt count.
    pub fn mark_disclosure_failed(&self, key: &str) -> Result<i64> {
        let conn = self.lock();
        conn.execute("UPDATE disclosures SET attempts = attempts + 1 WHERE key = ?1", [key])?;
        Ok(conn.query_row("SELECT attempts FROM disclosures WHERE key = ?1", [key], |r| r.get(0))?)
    }

    pub fn mark_disclosure_processed(&self, key: &str) -> Result<()> {
        self.lock()
            .execute("UPDATE disclosures SET processed = 1 WHERE key = ?1", [key])?;
        Ok(())
    }

    /// Most recent disclosures, newest first, as compact display lines.
    pub fn recent_disclosures(&self, limit: usize) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT politician, chamber, side, ticker, asset, amount_low, amount_high, tx_date, filed_date, source
             FROM disclosures ORDER BY seen_at DESC, filed_date DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |r| {
            let ticker: Option<String> = r.get(3)?;
            let asset: String = r.get(4)?;
            Ok(format!(
                "{} ({}) {} {} ${:.0}-${:.0} tx {} filed {} [{}]",
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                ticker.unwrap_or_else(|| asset.chars().take(24).collect()),
                r.get::<_, f64>(5)?,
                r.get::<_, f64>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, String>(8)?,
                r.get::<_, String>(9)?,
            ))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    // ---- 13F ------------------------------------------------------------

    pub fn insert_holdings(&self, holdings: &[Holding13F]) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        for h in holdings {
            tx.execute(
                "INSERT OR REPLACE INTO holdings_13f(cik, accession, period, cusip, issuer, shares, value, ticker)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    h.cik as i64,
                    h.accession,
                    h.period,
                    h.cusip,
                    h.issuer,
                    h.shares,
                    h.value,
                    h.ticker
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Holdings of the latest filing stored for a CIK.
    pub fn latest_holdings(&self, cik: u64) -> Result<Vec<Holding13F>> {
        let conn = self.lock();
        let acc: Option<String> = conn
            .query_row(
                "SELECT accession FROM holdings_13f WHERE cik = ?1 ORDER BY period DESC LIMIT 1",
                [cik as i64],
                |r| r.get(0),
            )
            .optional()?;
        let Some(acc) = acc else { return Ok(vec![]) };
        let mut stmt = conn.prepare(
            "SELECT cik, accession, period, cusip, issuer, shares, value, ticker FROM holdings_13f WHERE accession = ?1",
        )?;
        let rows = stmt.query_map([&acc], |r| {
            Ok(Holding13F {
                cik: r.get::<_, i64>(0)? as u64,
                accession: r.get(1)?,
                period: r.get(2)?,
                cusip: r.get(3)?,
                issuer: r.get(4)?,
                shares: r.get(5)?,
                value: r.get(6)?,
                ticker: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn cusip_lookup(&self, cusip: &str) -> Result<Option<Option<String>>> {
        Ok(self
            .lock()
            .query_row("SELECT ticker FROM cusip_map WHERE cusip = ?1", [cusip], |r| {
                r.get::<_, Option<String>>(0)
            })
            .optional()?)
    }

    pub fn cusip_store(&self, cusip: &str, ticker: Option<&str>, name: Option<&str>) -> Result<()> {
        self.lock().execute(
            "INSERT OR REPLACE INTO cusip_map(cusip, ticker, name) VALUES (?1, ?2, ?3)",
            params![cusip, ticker, name],
        )?;
        Ok(())
    }

    // ---- pilots / positions -----------------------------------------------

    pub fn pilot_state(&self, id: &str) -> Result<Option<PilotState>> {
        Ok(self
            .lock()
            .query_row(
                "SELECT enabled, allocation_usd FROM pilot_state WHERE pilot_id = ?1",
                [id],
                |r| {
                    Ok(PilotState {
                        enabled: r.get::<_, i64>(0)? == 1,
                        allocation_usd: r.get(1)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn set_pilot_state(&self, id: &str, enabled: bool, allocation_usd: Option<f64>) -> Result<()> {
        self.lock().execute(
            "INSERT INTO pilot_state(pilot_id, enabled, allocation_usd) VALUES (?1, ?2, ?3)
             ON CONFLICT(pilot_id) DO UPDATE SET enabled = excluded.enabled,
               allocation_usd = COALESCE(excluded.allocation_usd, pilot_state.allocation_usd)",
            params![id, enabled as i64, allocation_usd],
        )?;
        Ok(())
    }

    pub fn positions(&self, pilot_id: Option<&str>) -> Result<Vec<Position>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT pilot_id, ticker, qty, cost_basis FROM positions
             WHERE (?1 IS NULL OR pilot_id = ?1) AND qty > 0 ORDER BY pilot_id, ticker",
        )?;
        let rows = stmt.query_map([pilot_id], |r| {
            Ok(Position {
                pilot_id: r.get(0)?,
                ticker: r.get(1)?,
                qty: r.get(2)?,
                cost_basis: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn position(&self, pilot_id: &str, ticker: &str) -> Result<Option<Position>> {
        Ok(self
            .lock()
            .query_row(
                "SELECT pilot_id, ticker, qty, cost_basis FROM positions WHERE pilot_id = ?1 AND ticker = ?2",
                [pilot_id, ticker],
                |r| {
                    Ok(Position {
                        pilot_id: r.get(0)?,
                        ticker: r.get(1)?,
                        qty: r.get(2)?,
                        cost_basis: r.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    /// Applies a fill to the per-pilot ledger. Buys add quantity at `price`,
    /// sells remove quantity and release cost basis proportionally.
    pub fn apply_fill(&self, pilot_id: &str, ticker: &str, signed_qty: f64, price: f64) -> Result<()> {
        let conn = self.lock();
        let current = conn
            .query_row(
                "SELECT qty, cost_basis FROM positions WHERE pilot_id = ?1 AND ticker = ?2",
                [pilot_id, ticker],
                |r| Ok((r.get::<_, f64>(0)?, r.get::<_, f64>(1)?)),
            )
            .optional()?
            .unwrap_or((0.0, 0.0));
        let (qty, basis) = current;
        let (new_qty, new_basis) = if signed_qty >= 0.0 {
            (qty + signed_qty, basis + signed_qty * price)
        } else {
            let sold = (-signed_qty).min(qty);
            let remaining = qty - sold;
            let released = if qty > 0.0 { basis * sold / qty } else { 0.0 };
            (remaining, (basis - released).max(0.0))
        };
        if new_qty <= 0.0 {
            conn.execute(
                "DELETE FROM positions WHERE pilot_id = ?1 AND ticker = ?2",
                [pilot_id, ticker],
            )?;
        } else {
            conn.execute(
                "INSERT INTO positions(pilot_id, ticker, qty, cost_basis) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(pilot_id, ticker) DO UPDATE SET qty = excluded.qty, cost_basis = excluded.cost_basis",
                params![pilot_id, ticker, new_qty, new_basis],
            )?;
        }
        Ok(())
    }

    /// Clears the simulated book: positions and the order log. Disclosures,
    /// filings and signals stay so nothing is re-traded.
    pub fn reset_ledger(&self) -> Result<()> {
        self.lock().execute_batch("DELETE FROM positions; DELETE FROM orders;")?;
        Ok(())
    }

    /// Total cost basis currently deployed by a pilot.
    pub fn pilot_exposure(&self, pilot_id: &str) -> Result<f64> {
        Ok(self.lock().query_row(
            "SELECT COALESCE(SUM(cost_basis), 0) FROM positions WHERE pilot_id = ?1",
            [pilot_id],
            |r| r.get(0),
        )?)
    }

    // ---- orders -----------------------------------------------------------

    pub fn insert_order(&self, o: &NewOrder<'_>) -> Result<i64> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO orders(ts, pilot_id, ticker, side, qty, price, notional, ib_order_id, status, reason)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                now(),
                o.pilot_id,
                o.ticker,
                o.side,
                o.qty,
                o.price,
                o.notional,
                o.ib_order_id,
                o.status,
                o.reason
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn update_order_status(&self, ib_order_id: i64, status: &str, fill_price: Option<f64>) -> Result<bool> {
        let n = self.lock().execute(
            "UPDATE orders SET status = ?2, fill_price = COALESCE(?3, fill_price) WHERE ib_order_id = ?1",
            params![ib_order_id, status, fill_price],
        )?;
        Ok(n > 0)
    }

    pub fn recent_orders(&self, limit: usize) -> Result<Vec<OrderRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, ts, pilot_id, ticker, side, qty, price, notional, ib_order_id, status, fill_price, reason
             FROM orders ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |r| {
            Ok(OrderRow {
                id: r.get(0)?,
                ts: r.get(1)?,
                pilot_id: r.get(2)?,
                ticker: r.get(3)?,
                side: r.get(4)?,
                qty: r.get(5)?,
                price: r.get(6)?,
                notional: r.get(7)?,
                ib_order_id: r.get(8)?,
                status: r.get(9)?,
                fill_price: r.get(10)?,
                reason: r.get(11)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    // ---- news -------------------------------------------------------------

    /// Stores a headline; returns `true` if it is new.
    pub fn insert_news(&self, item: &NewsItem) -> Result<bool> {
        let n = self.lock().execute(
            "INSERT OR IGNORE INTO news_items(id, ts, source, title, link) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![item.id, now(), item.source, item.title, item.link],
        )?;
        Ok(n == 1)
    }

    pub fn unscored_news(&self, limit: usize) -> Result<Vec<NewsItem>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT id, source, title, link FROM news_items WHERE scored = 0 ORDER BY ts LIMIT ?1")?;
        let rows = stmt.query_map([limit as i64], |r| {
            Ok(NewsItem {
                id: r.get(0)?,
                source: r.get(1)?,
                title: r.get(2)?,
                link: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn mark_news_scored(&self, ids: &[String]) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        for id in ids {
            tx.execute("UPDATE news_items SET scored = 1 WHERE id = ?1", [id])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn insert_signals(&self, signals: &[NewsSignal]) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        for s in signals {
            tx.execute(
                "INSERT INTO signals(ts, news_id, ticker, direction, confidence, rationale) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![now(), s.news_id, s.ticker, s.direction.to_string(), s.confidence, s.rationale],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn unprocessed_signals(&self) -> Result<Vec<SignalRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, ts, news_id, ticker, direction, confidence, rationale FROM signals WHERE processed = 0 ORDER BY id",
        )?;
        let rows = stmt.query_map([], signal_row)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn mark_signal_processed(&self, id: i64) -> Result<()> {
        self.lock().execute("UPDATE signals SET processed = 1 WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn recent_signals(&self, limit: usize) -> Result<Vec<SignalRow>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare("SELECT id, ts, news_id, ticker, direction, confidence, rationale FROM signals ORDER BY id DESC LIMIT ?1")?;
        let rows = stmt.query_map([limit as i64], signal_row)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }
}

fn signal_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SignalRow> {
    let direction = match r.get::<_, String>(4)?.as_str() {
        "buy" => SignalDirection::Buy,
        "sell" => SignalDirection::Sell,
        _ => SignalDirection::None,
    };
    Ok(SignalRow {
        id: r.get(0)?,
        ts: r.get(1)?,
        signal: NewsSignal {
            news_id: r.get(2)?,
            ticker: r.get(3)?,
            direction,
            confidence: r.get(5)?,
            rationale: r.get(6)?,
        },
    })
}

fn parse_date(s: &str) -> NaiveDate {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn sample() -> Disclosure {
        Disclosure {
            source: "house".into(),
            chamber: Chamber::House,
            politician: "Nancy Pelosi".into(),
            ticker: Some("BE".into()),
            asset: "Bloom Energy".into(),
            asset_type: Some("ST".into()),
            side: Side::Purchase,
            tx_date: NaiveDate::from_ymd_opt(2026, 7, 24).unwrap(),
            filed_date: NaiveDate::from_ymd_opt(2026, 8, 21).unwrap(),
            amount_low: 1_000_001.0,
            amount_high: 5_000_000.0,
            owner: Some("SP".into()),
            url: "https://example".into(),
        }
    }

    #[test]
    fn disclosure_dedupes_across_sources() {
        let db = Db::open_in_memory().unwrap();
        let a = sample();
        let mut b = sample();
        b.source = "bargo".into();
        b.asset = "Bloom Energy Corp - Class A".into();
        assert!(db.insert_disclosure(&a).unwrap());
        assert!(!db.insert_disclosure(&b).unwrap());
        assert_eq!(db.unprocessed_disclosures().unwrap().len(), 1);
    }

    #[test]
    fn ledger_math() {
        let db = Db::open_in_memory().unwrap();
        db.apply_fill("p", "BE", 10.0, 20.0).unwrap();
        db.apply_fill("p", "BE", 10.0, 30.0).unwrap();
        let pos = db.position("p", "BE").unwrap().unwrap();
        assert_eq!(pos.qty, 20.0);
        assert_eq!(pos.cost_basis, 500.0);
        db.apply_fill("p", "BE", -5.0, 100.0).unwrap();
        let pos = db.position("p", "BE").unwrap().unwrap();
        assert_eq!(pos.qty, 15.0);
        assert_eq!(pos.cost_basis, 375.0);
        db.apply_fill("p", "BE", -15.0, 100.0).unwrap();
        assert!(db.position("p", "BE").unwrap().is_none());
        assert_eq!(db.pilot_exposure("p").unwrap(), 0.0);
    }
}
