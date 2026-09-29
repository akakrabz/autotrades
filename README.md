# autotrades

Mirror publicly disclosed trades into an Interactive Brokers **paper** account,
controlled from Telegram. A single-operator, self-hosted take on the "copy
Pelosi / copy Buffett" apps.

| Pilot kind | Where the trades come from | Delay you should expect |
|---|---|---|
| `politician` | House Clerk PTR PDFs, Senate eFD, Bargo aggregator (STOCK Act filings) | filers have 30–45 days; we act within one poll of the filing going public |
| `thirteen_f` | SEC EDGAR 13F-HR information tables | quarterly, published ≤45 days after quarter end |
| `news` | RSS feeds + Telegram channel posts / forwards, screened by Claude | minutes |

Everything is written to SQLite (`disclosures`, `orders`, per-pilot
`positions`), every decision — including skips — is pushed to your Telegram
chat, and the default mode is **dry-run**.

## How it works

```
 sources (every poll_interval_secs)            engine                      broker
 ┌──────────────┐                                                       ┌───────────┐
 │ bargo API    │──┐   disclosures    ┌──────────────────────────┐  buy │ TWS paper │
 │ house PTRs   │──┼──(dedupe key)───▶│ match pilots, size, cap  │─────▶│  :7497    │
 │ senate eFD   │──┘                  │ (dry-run or market order)│ sell │ (ibapi)   │
 │ edgar 13F    │────holdings────────▶│ rebalance to top-N wts   │      └───────────┘
 │ rss/telegram │──headlines──Claude──▶ signals ≥ min_confidence  │
 └──────────────┘                     └────────────┬─────────────┘
                                                   │ orders + ledger (SQLite)
                                          Telegram ◀┘ notifications / commands
```

Sizing rules (all capped by `max_order_usd`, the pilot's `allocation_usd`
and, if set, `max_position_usd`):

* politician **purchase** → `per_trade_usd` × bucket weight
  (`$1,001–$15,000` = 1×, `$15,001–$50,000` = 2×, … `>$5M` = 8×)
* politician **sale** → sell the pilot's whole position (half on a partial sale)
* 13F → rebalance to the filer's top-N value weights whenever a new filing appears
* news **buy** → `per_trade_usd`; news **sell** → close the pilot's position

Only plain stocks/ETFs are mirrored (House type `[ST]`, Senate type `Stock`);
options, bonds, funds without tickers and exchanges are reported and skipped.
Whole shares only. Each pilot keeps its own ledger, so the same ticker held by
two pilots is tracked separately even though IBKR sees one position.

## Setup

```sh
# 1. system deps: Rust stable, poppler (pdftotext)
sudo pacman -S poppler            # Debian/Ubuntu: apt install poppler-utils

# 2. TWS or IB Gateway, *paper* login, API enabled:
#    Configure → API → Settings: "Enable ActiveX and Socket Clients", port 7497
#    (Gateway paper: 4002), untick "Read-Only API", add 127.0.0.1 to trusted IPs.

# 3. config + secrets
cp autotrades.example.toml autotrades.toml    # pilots, allocations, sources
cp .env.example .env                          # TELEGRAM_*, SEC_USER_AGENT, keys

# 4. build & check
cargo build --release
./target/release/autotrades ibkr              # prints account summary + SPY close
./target/release/autotrades once              # one polling round, dry-run
./target/release/autotrades                   # run forever (poller + engine + bot)
```

Telegram: create a bot with [@BotFather](https://t.me/BotFather), put the
token in `TELEGRAM_BOT_TOKEN`, and your numeric chat id (ask
[@userinfobot](https://t.me/userinfobot)) in `TELEGRAM_OWNER_ID`. Only that
chat can issue commands.

```
/status            connection, mode, pilots, exposure
/follow pelosi 5000   enable a pilot with a $5,000 allocation (persisted in the DB)
/unfollow pelosi
/pause  /resume    global kill switch
/dryrun off        start sending real orders to the paper account
/trades 20         latest disclosures seen     /orders 20   latest orders
/positions         per-pilot ledger            /ibkr        broker account + positions
/signals           latest news signals         /sync        poll now
/reset confirm     wipe ledger + order log (not IBKR)
```

Send or forward any headline to the bot — or add the bot as an admin of a
Telegram news channel — and it enters the news pipeline (`[news] enabled = true`,
`ANTHROPIC_API_KEY` set).

A systemd unit is in `deploy/autotrades.service`.

### Secrets / identifiers

| Variable | Needed for |
|---|---|
| `TELEGRAM_BOT_TOKEN`, `TELEGRAM_OWNER_ID` | the bot |
| `SEC_USER_AGENT` = `"<app> <your@email>"` | EDGAR 13F (`www.sec.gov` returns 403 without an email) |
| `OPENFIGI_API_KEY` | optional; 100 CUSIPs/request instead of 10 |
| `ANTHROPIC_API_KEY` | news scoring |

## Data sources — what is actually fresh in 2026

Verified live while building this (Sept 2026):

* **House** — `https://disclosures-clerk.house.gov/public_disc/financial-pdfs/{year}FD.zip`
  is a tab-separated index refreshed daily (~09:00 ET); `FilingType = P` rows
  are PTRs at `public_disc/ptr-pdfs/{year}/{DocID}.pdf`. Electronic filings
  have a text layer (`pdftotext -layout`) that `sources/house.rs` parses,
  including asset names and amount ranges wrapped across page breaks. Paper
  filings (4–7 digit DocIDs) are scans and are recorded as `unparsed`.
* **Senate** — `efdsearch.senate.gov` is a Django app: fetch the CSRF token,
  POST the prohibition agreement, then `POST /search/report/data/` with
  `report_types=[11]` returns filings as JSON; electronic PTRs are HTML tables
  with a real **Ticker** column. Paper filings are images.
* **Bargo** (`bargo.ai/free-apis/congress/v1/trades`) — free, keyless JSON of
  both chambers with tickers resolved. Usually the earliest signal, but
  rate-limited (one 100-row request per round; a second page 429s). The old
  House/Senate Stock Watcher S3 mirrors are dead (403).
* **EDGAR** — `data.sec.gov/submissions/CIK##########.json` lists filings with
  sub-second processing delay; the 13F information table is the `.xml` in the
  filing directory that is not `primary_doc.xml`. CUSIPs are mapped to tickers
  with OpenFIGI (`api.openfigi.com/v3/mapping`) and cached.
* **Paid/faster alternatives** if you want them: Quiver Quant, Unusual Whales,
  Finnhub `congressional-trading`, Disclosed Capitol — all wrap the same
  filings; none can beat the statutory 30–45 day reporting lag. Real-time
  "expert trader" feeds (Autopilot's fourth category) are people voluntarily
  streaming their own brokerage — not a public data source.

The inherent lag is the important caveat: you are trading a politician's
*disclosure*, not their trade. `max_disclosure_age_days` (default 10) keeps a
fresh install from replaying history and keeps very stale filings out.

## Operating notes

* **Dry-run ledger.** In dry-run the ledger is updated as if fills happened,
  so sells later work. When you switch to live (`/dryrun off`) run
  `/reset confirm` first, or accept that the ledger and IBKR positions
  diverge for the dry-run period.
* **Paper only.** `ibkr.require_paper_account = true` refuses accounts whose
  ids do not start with `DU`/`DF`.
* **Dedupe.** A trade is identified by chamber + family name + ticker +
  transaction date + direction + amount bucket. Two identical-looking lots on
  the same day collapse into one mirrored trade (under-trading is the safe
  failure mode).
* **Prices** come from the last daily bar (delayed data is free on paper
  accounts), so sizing works outside market hours; market orders placed
  outside RTH execute at the open.
* Logs: `RUST_LOG=info,autotrades::engine=debug` etc.

## Development

```sh
cargo test                      # parsers run against tests/fixtures (real PTRs, Senate table, 13F, Bargo)
cargo clippy --all-targets -- -D warnings
cargo run -- parse-ptr some.pdf # debug the House parser on any PTR
```

`python/` contains a ~350-line Python edition with the same House/Bargo
parsing and sizing rules on top of `ib_async` — see `python/README.md`.
