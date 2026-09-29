#!/usr/bin/env python3
"""autotrades (python edition) — the minimal version of the Rust app.

Mirrors STOCK Act periodic transaction reports into an Interactive Brokers
paper account. Sources: the House Clerk's daily index + PTR PDFs (parsed with
`pdftotext -layout`) and Bargo's free aggregator API (House + Senate).
Execution goes through `ib_async` (the maintained fork of ib_insync).

Usage:
    python autotrades.py --once            # poll every source once, act, exit
    python autotrades.py --loop            # poll every 15 minutes
    python autotrades.py --selftest        # parser tests against ../tests/fixtures
    python autotrades.py --once --live     # actually send orders (default is dry-run)

Config lives in config.json next to this file (see config.example.json).
"""

from __future__ import annotations

import argparse
import datetime as dt
import io
import json
import math
import os
import re
import sqlite3
import subprocess
import sys
import tempfile
import time
import zipfile
from dataclasses import dataclass
from pathlib import Path

import requests

HERE = Path(__file__).resolve().parent
UA = "autotrades-py/0.1 (+https://github.com/akakrabz/autotrades)"
HOUSE_INDEX = "https://disclosures-clerk.house.gov/public_disc/financial-pdfs/{year}FD.zip"
HOUSE_PTR = "https://disclosures-clerk.house.gov/public_disc/ptr-pdfs/{year}/{doc_id}.pdf"
BARGO = "https://www.bargo.ai/free-apis/congress/v1/trades?limit=100&page=0"

# ---------------------------------------------------------------- model ----


@dataclass
class Disclosure:
    source: str
    chamber: str
    politician: str
    ticker: str | None
    asset: str
    asset_type: str | None
    side: str  # purchase | sale | sale_partial | exchange
    tx_date: dt.date
    filed_date: dt.date
    amount_low: float
    amount_high: float
    url: str

    def key(self) -> str:
        """Cross-source identity (see the Rust `Disclosure::key`)."""
        name = last_name(self.politician).lower()
        direction = {"purchase": "buy", "sale": "sell", "sale_partial": "sell"}.get(self.side, self.side)
        asset = "" if self.ticker else re.sub(r"[^a-z0-9]", "", self.asset.lower())[:32]
        return f"{self.chamber}:{name}:{self.ticker or '-'}:{asset}:{self.tx_date}:{direction}:{int(self.amount_low)}"

    def mirrorable(self) -> bool:
        return bool(self.ticker) and (self.asset_type in (None, "ST"))


SUFFIXES = {"jr", "sr", "ii", "iii", "iv", "hon", "mr", "mrs", "ms", "dr"}


def last_name(full: str) -> str:
    words = [w for w in re.split(r"[\s,]+", full) if w and w.rstrip(".").lower() not in SUFFIXES]
    return words[-1] if words else full


def display_name(first: str, last: str) -> str:
    return " ".join(w for w in re.split(r"[\s,]+", f"{first} {last}") if w and w.rstrip(".").lower() not in SUFFIXES)


def parse_side(raw: str) -> str | None:
    s = raw.strip().lower()
    return {
        "p": "purchase", "purchase": "purchase", "buy": "purchase",
        "s": "sale", "sale": "sale", "sale (full)": "sale", "sell": "sale",
        "s (partial)": "sale_partial", "sale (partial)": "sale_partial",
        "e": "exchange", "exchange": "exchange",
    }.get(s)


def amount_weight(low: float) -> float:
    for bound, w in ((15_000, 1), (50_000, 2), (100_000, 3), (250_000, 4), (500_000, 5), (1_000_000, 6), (5_000_000, 7)):
        if low <= bound:
            return float(w)
    return 8.0


# ---------------------------------------------------------- house parser ----

ROW_START = re.compile(r"^\s*(?:(SP|JT|DC)\s+)?(.*?)\s+(P|S \(partial\)|S|E)\s+(\d{2}/\d{2}/\d{4})\s+(\d{2}/\d{2}/\d{4})\s*(.*)$")
LABEL_LINE = re.compile(r"^\s*[A-Z](?:\s+[A-Z])*\s*:")
TICKER = re.compile(r"\(([A-Z][A-Z0-9.\-]{0,9})\)\s*\[([A-Z]{2,3})\]")
ASSET_TYPE = re.compile(r"\[([A-Z]{2,3})\]")
MONEY = re.compile(r"\$[\d,]+")


def parse_ptr_text(text: str) -> list[dict]:
    """Port of the Rust parser: fixed-width rows with wrapped asset names and
    amount ranges, page headers possibly repeated mid-row."""
    # split on "\n" only: str.splitlines() would also split on the form feed
    # pdftotext emits at each page start, hiding the repeated header lines.
    lines = text.split("\n")
    rows, i = [], 0
    while i < len(lines):
        m = ROW_START.match(lines[i])
        if not m:
            i += 1
            continue
        owner, asset, side, d1, d2, tail = m.groups()
        asset, tail = asset.strip(), tail.strip()
        j = i + 1
        while j < len(lines):
            t = lines[j].strip()
            if not t or ROW_START.match(lines[j]) or t.startswith("* For the complete"):
                break
            if t.startswith("ID ") or t.startswith("Type ") or t == "$200?":
                j += 1
                continue
            if not LABEL_LINE.match(lines[j]):
                left, _, right = t.partition("   ")
                if left.strip():
                    asset += " " + left.strip()
                if right.strip():
                    tail += " " + right.strip()
            j += 1
        money = [float(x.replace("$", "").replace(",", "")) for x in MONEY.findall(f"{tail} {asset}")[:2]]
        low, high = (money + money)[:2] if money else (0.0, 0.0)
        tm = TICKER.search(asset)
        ticker, atype = (tm.group(1), tm.group(2)) if tm else (None, (ASSET_TYPE.search(asset) or [None, None])[1])
        asset = " ".join(MONEY.sub("", asset).split()).removesuffix(" -")
        rows.append({
            "owner": owner, "asset": asset, "ticker": ticker, "asset_type": atype,
            "side": parse_side(side) or "exchange",
            "tx_date": dt.datetime.strptime(d1, "%m/%d/%Y").date(),
            "notified": dt.datetime.strptime(d2, "%m/%d/%Y").date(),
            "amount_low": low, "amount_high": high,
        })
        i = j
    return rows


def pdf_to_text(pdf: bytes) -> str:
    with tempfile.NamedTemporaryFile(suffix=".pdf") as f:
        f.write(pdf)
        f.flush()
        return subprocess.run(["pdftotext", "-layout", f.name, "-"], check=True, capture_output=True, text=True).stdout


def house_index(session: requests.Session, year: int) -> list[dict]:
    z = zipfile.ZipFile(io.BytesIO(session.get(HOUSE_INDEX.format(year=year), timeout=60).content))
    txt = z.read(f"{year}FD.txt").decode("utf-8", "replace")
    out = []
    for line in txt.splitlines()[1:]:
        f = line.rstrip("\r").split("\t")
        if len(f) >= 9 and f[4].strip() == "P":
            out.append({"last": f[1].strip(), "first": f[2].strip(), "year": int(f[6]),
                        "filed": dt.datetime.strptime(f[7].strip(), "%m/%d/%Y").date(), "doc_id": f[8].strip()})
    return out


# ------------------------------------------------------------- storage ----

SCHEMA = """
CREATE TABLE IF NOT EXISTS filings (id TEXT PRIMARY KEY, status TEXT);
CREATE TABLE IF NOT EXISTS disclosures (key TEXT PRIMARY KEY, json TEXT, processed INTEGER DEFAULT 0);
CREATE TABLE IF NOT EXISTS positions (pilot TEXT, ticker TEXT, qty REAL, cost REAL, PRIMARY KEY (pilot, ticker));
CREATE TABLE IF NOT EXISTS orders (id INTEGER PRIMARY KEY, ts TEXT, pilot TEXT, ticker TEXT, side TEXT, qty REAL, price REAL, status TEXT, reason TEXT);
"""


class Store:
    def __init__(self, path: Path):
        self.c = sqlite3.connect(path)
        self.c.executescript(SCHEMA)

    def seen(self, filing_id: str) -> bool:
        return self.c.execute("SELECT 1 FROM filings WHERE id=?", (filing_id,)).fetchone() is not None

    def mark(self, filing_id: str, status: str) -> None:
        self.c.execute("INSERT OR REPLACE INTO filings VALUES (?, ?)", (filing_id, status))
        self.c.commit()

    def add(self, d: Disclosure) -> bool:
        payload = json.dumps({**d.__dict__, "tx_date": str(d.tx_date), "filed_date": str(d.filed_date)})
        cur = self.c.execute("INSERT OR IGNORE INTO disclosures(key, json) VALUES (?, ?)", (d.key(), payload))
        self.c.commit()
        return cur.rowcount == 1

    def unprocessed(self) -> list[tuple[str, Disclosure]]:
        out = []
        for key, raw in self.c.execute("SELECT key, json FROM disclosures WHERE processed=0"):
            j = json.loads(raw)
            j["tx_date"] = dt.date.fromisoformat(j["tx_date"])
            j["filed_date"] = dt.date.fromisoformat(j["filed_date"])
            out.append((key, Disclosure(**j)))
        return out

    def done(self, key: str) -> None:
        self.c.execute("UPDATE disclosures SET processed=1 WHERE key=?", (key,))
        self.c.commit()

    def position(self, pilot: str, ticker: str) -> tuple[float, float]:
        row = self.c.execute("SELECT qty, cost FROM positions WHERE pilot=? AND ticker=?", (pilot, ticker)).fetchone()
        return (row[0], row[1]) if row else (0.0, 0.0)

    def exposure(self, pilot: str) -> float:
        return self.c.execute("SELECT COALESCE(SUM(cost),0) FROM positions WHERE pilot=?", (pilot,)).fetchone()[0]

    def fill(self, pilot: str, ticker: str, signed_qty: float, price: float) -> None:
        qty, cost = self.position(pilot, ticker)
        if signed_qty >= 0:
            qty, cost = qty + signed_qty, cost + signed_qty * price
        else:
            sold = min(-signed_qty, qty)
            cost, qty = (cost * (1 - sold / qty) if qty else 0.0), qty - sold
        if qty <= 0:
            self.c.execute("DELETE FROM positions WHERE pilot=? AND ticker=?", (pilot, ticker))
        else:
            self.c.execute("INSERT OR REPLACE INTO positions VALUES (?, ?, ?, ?)", (pilot, ticker, qty, cost))
        self.c.commit()

    def order(self, pilot: str, ticker: str, side: str, qty: float, price: float | None, status: str, reason: str) -> None:
        self.c.execute("INSERT INTO orders(ts, pilot, ticker, side, qty, price, status, reason) VALUES (?,?,?,?,?,?,?,?)",
                       (dt.datetime.now(dt.UTC).isoformat(timespec="seconds"), pilot, ticker, side, qty, price, status, reason))
        self.c.commit()


# --------------------------------------------------------------- broker ----


class Broker:
    """Thin wrapper over ib_async; created lazily so dry runs work offline."""

    def __init__(self, host: str, port: int, client_id: int):
        self.host, self.port, self.client_id, self.ib = host, port, client_id, None

    def connect(self):
        if self.ib is None:
            from ib_async import IB  # imported here so --selftest needs no IB install

            ib = IB()
            ib.connect(self.host, self.port, clientId=self.client_id, timeout=20)
            accounts = ib.managedAccounts()
            if not all(a.startswith(("DU", "DF")) for a in accounts):
                raise SystemExit(f"refusing to trade non-paper account(s) {accounts}")
            ib.reqMarketDataType(3)  # delayed data is free on paper accounts
            self.ib = ib
        return self.ib

    def contract(self, symbol: str):
        from ib_async import Stock

        return Stock(symbol.replace(".", " "), "SMART", "USD")

    def last_price(self, symbol: str) -> float:
        ib = self.connect()
        bars = ib.reqHistoricalData(self.contract(symbol), endDateTime="", durationStr="5 D",
                                    barSizeSetting="1 day", whatToShow="TRADES", useRTH=True)
        if not bars:
            raise RuntimeError(f"no price bars for {symbol}")
        return float(bars[-1].close)

    def market_order(self, symbol: str, side: str, qty: float) -> int:
        from ib_async import MarketOrder

        ib = self.connect()
        trade = ib.placeOrder(self.contract(symbol), MarketOrder(side, qty))
        ib.sleep(1)
        return trade.order.orderId


# --------------------------------------------------------------- engine ----


def poll_house(session: requests.Session, store: Store, max_age_days: int) -> int:
    today = dt.date.today()
    cutoff = today - dt.timedelta(days=max_age_days)
    first_run = store.c.execute("SELECT COUNT(*) FROM filings").fetchone()[0] == 0
    new = 0
    for row in house_index(session, today.year):
        if store.seen(row["doc_id"]):
            continue
        if first_run and row["filed"] < cutoff:
            store.mark(row["doc_id"], "skipped")
            continue
        url = HOUSE_PTR.format(year=row["year"], doc_id=row["doc_id"])
        try:
            rows = parse_ptr_text(pdf_to_text(session.get(url, timeout=60).content))
        except Exception as e:  # noqa: BLE001 - one bad PDF must not stop the round
            print(f"house {row['doc_id']}: {e}", file=sys.stderr)
            continue
        who = display_name(row["first"], row["last"])
        for r in rows:
            d = Disclosure("house", "house", who, r["ticker"], r["asset"], r["asset_type"], r["side"],
                           r["tx_date"], row["filed"], r["amount_low"], r["amount_high"], url)
            new += store.add(d)
        store.mark(row["doc_id"], "parsed" if rows else "unparsed")
        print(f"house {row['doc_id']} {who}: {len(rows)} rows")
    return new


def poll_bargo(session: requests.Session, store: Store, max_age_days: int) -> int:
    r = session.get(BARGO, timeout=30)
    if r.status_code == 429:
        print("bargo rate limited; skipping", file=sys.stderr)
        return 0
    r.raise_for_status()
    cutoff = dt.date.today() - dt.timedelta(days=max_age_days)
    new = 0
    for t in r.json()["trades"]:
        side = parse_side(t["type"])
        filed = dt.date.fromisoformat(t["disclosure_date"])
        if not side or filed < cutoff:
            continue
        d = Disclosure("bargo", t["chamber"], t["member"].replace("&amp;", "&"), (t.get("ticker") or "").upper() or None,
                       t.get("asset", ""), None, side, dt.date.fromisoformat(t["transaction_date"]), filed,
                       float(t.get("amount_low") or 0), float(t.get("amount_high") or 0), t.get("filing_portal") or "")
        new += store.add(d)
    return new


def act(store: Store, broker: Broker, cfg: dict, live: bool) -> None:
    cutoff = dt.date.today() - dt.timedelta(days=cfg.get("max_disclosure_age_days", 10))
    for key, d in store.unprocessed():
        for p in cfg["pilots"]:
            if not p.get("enabled", True) or p["match"].lower() not in d.politician.lower():
                continue
            if p.get("chamber") and p["chamber"] != d.chamber:
                continue
            label = f"{d.politician} {d.side} {d.ticker or d.asset} ${d.amount_low:.0f}-${d.amount_high:.0f} tx {d.tx_date} [{d.source}]"
            if d.filed_date < cutoff or not d.mirrorable():
                print(f"skip ({'stale' if d.filed_date < cutoff else 'not a plain stock'}): {label}")
                continue
            try:
                execute(store, broker, cfg, p, d, label, live)
            except Exception as e:  # noqa: BLE001
                print(f"{p['id']}: execution failed: {e}", file=sys.stderr)
        store.done(key)


def execute(store: Store, broker: Broker, cfg: dict, p: dict, d: Disclosure, label: str, live: bool) -> None:
    pilot, ticker = p["id"], d.ticker
    qty_held, _ = store.position(pilot, ticker)
    if d.side == "purchase":
        notional = p["per_trade_usd"] * (amount_weight(d.amount_low) if p.get("scale_by_amount", True) else 1)
        notional = min(notional, cfg.get("max_order_usd", 10_000), p["allocation_usd"] - store.exposure(pilot))
        side, qty = "BUY", None
    elif d.side in ("sale", "sale_partial"):
        if qty_held <= 0:
            return
        side, qty, notional = "SELL", (max(1, math.floor(qty_held / 2)) if d.side == "sale_partial" else qty_held), 0
    else:
        return
    try:
        price = broker.last_price(ticker)
    except Exception as e:  # noqa: BLE001
        if live:
            raise
        store.order(pilot, ticker, side, 0, None, "skipped", f"no price ({e}); {label}")
        print(f"[DRY RUN] {pilot} {side} {ticker}: no price ({e})")
        return
    qty = math.floor(notional / price) if qty is None else qty
    if qty < 1:
        store.order(pilot, ticker, side, 0, price, "skipped", f"rounds to 0 shares; {label}")
        print(f"skip: {pilot} {side} {ticker} rounds to 0 shares at {price:.2f}")
        return
    status = "dry_run"
    if live:
        order_id = broker.market_order(ticker, side, qty)
        status = f"submitted IB#{order_id}"
    store.order(pilot, ticker, side, qty, price, status, label)
    store.fill(pilot, ticker, qty if side == "BUY" else -qty, price)
    print(f"[{status}] {pilot} {side} {qty} {ticker} @ ~{price:.2f} — {label}")


# ----------------------------------------------------------------- main ----


def selftest() -> None:
    fixtures = HERE.parent / "tests" / "fixtures"
    rows = parse_ptr_text((fixtures / "house_ptr_20035143.txt").read_text())
    assert len(rows) == 7 and rows[0]["ticker"] == "BE" and rows[0]["asset_type"] == "ST", rows
    assert (rows[0]["amount_low"], rows[0]["amount_high"]) == (1_000_001, 5_000_000)
    assert rows[1]["asset_type"] == "OP" and rows[-1]["ticker"] is None
    rows = parse_ptr_text((fixtures / "house_ptr_20035491.txt").read_text())
    assert len(rows) == 99 and all(r["ticker"] for r in rows if r["asset_type"] == "ST"), "page-break rows lost tickers"
    split = next(r for r in rows if r["tx_date"] == dt.date(2026, 9, 2) and "Devon" in r["asset"])
    assert split["ticker"] == "DVN" and (split["amount_low"], split["amount_high"]) == (100_001, 250_000), split
    rows = parse_ptr_text((fixtures / "house_ptr_partial.txt").read_text())
    assert any(r["ticker"] == "FANG" and r["side"] == "sale_partial" for r in rows)
    d = Disclosure("bargo", "house", "Nancy Pelosi", "BE", "Bloom Energy", None, "purchase",
                   dt.date(2026, 7, 24), dt.date(2026, 8, 21), 1_000_001, 5_000_000, "")
    d2 = Disclosure("house", "house", "Nancy Pelosi", "BE", "Bloom Energy Corporation Class A (BE) [ST]", "ST", "purchase",
                    dt.date(2026, 7, 24), dt.date(2026, 8, 21), 1_000_001, 5_000_000, "")
    assert d.key() == d2.key()
    assert amount_weight(1001) == 1 and amount_weight(5_000_001) == 8
    print("selftest ok")


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--config", default=str(HERE / "config.json"))
    ap.add_argument("--once", action="store_true", help="poll, act, exit")
    ap.add_argument("--loop", action="store_true", help="poll every poll_interval_secs")
    ap.add_argument("--live", action="store_true", help="send orders to IBKR (default: dry-run)")
    ap.add_argument("--selftest", action="store_true")
    a = ap.parse_args()
    if a.selftest:
        return selftest()
    if not (a.once or a.loop):
        ap.error("pass --once or --loop")
    cfg = json.loads(Path(a.config).read_text())
    store = Store(Path(cfg.get("db_path", HERE / "autotrades.db")))
    ib = cfg.get("ibkr", {})
    broker = Broker(ib.get("host", "127.0.0.1"), int(ib.get("port", 7497)), int(ib.get("client_id", 18)))
    session = requests.Session()
    session.headers["User-Agent"] = UA
    while True:
        for name, fn in (("bargo", poll_bargo), ("house", poll_house)):
            try:
                print(f"{name}: {fn(session, store, cfg.get('max_disclosure_age_days', 10))} new disclosures")
            except Exception as e:  # noqa: BLE001
                print(f"{name} failed: {e}", file=sys.stderr)
        act(store, broker, cfg, a.live)
        if a.once:
            break
        time.sleep(int(cfg.get("poll_interval_secs", 900)))


if __name__ == "__main__":
    main()
