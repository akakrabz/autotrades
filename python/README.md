# autotrades — Python edition

The minimal version of the Rust app, kept as a readable reference (about 350
lines, one file). It mirrors House and Senate STOCK Act trades into an IBKR
paper account; it has no Telegram bot, no 13F tracking and no news scoring —
use the Rust app for those.

```sh
python -m venv .venv && . .venv/bin/activate
pip install -r requirements.txt          # requests + ib_async
sudo pacman -S poppler                   # pdftotext (poppler-utils on Debian/Ubuntu)
cp config.example.json config.json       # edit pilots / allocation
python autotrades.py --selftest          # parser tests against ../tests/fixtures
python autotrades.py --once              # dry run: poll, decide, log
python autotrades.py --loop --live       # send market orders to TWS paper (port 7497)
```

Sources: Bargo's free API (fastest, both chambers, rate-limited to one call
per round) and the House Clerk's daily index + PTR PDFs. Trades seen through
both dedupe on the same canonical key the Rust app uses. Sizing is identical:
`per_trade_usd` × disclosed-amount bucket weight for buys, sell the pilot's
whole position (half on a partial sale) for sells, whole shares only.

The venv at `~/.venvs/sci` (Python 3.12) works too: `~/.venvs/sci/bin/pip install -r requirements.txt`.
