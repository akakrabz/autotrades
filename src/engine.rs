//! Turns stored disclosures, 13F snapshots and news signals into orders.
//!
//! Sizing rules (all capped by `max_order_usd`, the pilot's allocation and,
//! where set, its per-ticker `max_position_usd`):
//!
//! * politician purchase -> `per_trade_usd` x bucket weight (1x for
//!   `$1,001-$15,000` up to 8x for `>$5M`)
//! * politician sale -> sell the pilot's whole position (half for a partial sale)
//! * 13F -> rebalance to the filer's top-N value weights every new filing
//! * news buy -> `per_trade_usd`; news sell -> close the pilot's position
//!
//! Only whole shares are traded; anything that rounds to zero is skipped and
//! reported. Every decision, including skips, is written to the database and
//! echoed to Telegram.

use anyhow::Result;
use chrono::Utc;
use tracing::{info, warn};

use crate::app::App;
use crate::broker::OrderSide;
use crate::config::{PilotConfig, PilotKind};
use crate::db::NewOrder;
use crate::model::{Disclosure, Side, SignalDirection, amount_weight};

/// What the engine wants to do; `execute` prices and sizes it.
#[derive(Debug, Clone)]
pub struct Intent {
    pub pilot_id: String,
    pub ticker: String,
    pub side: OrderSide,
    /// Exact quantity (sells) ...
    pub qty: Option<f64>,
    /// ... or a USD notional to convert at the current price (buys).
    pub notional: Option<f64>,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Placed,
    DryRun,
    Skipped,
}

/// Runs every stage once. Called after each polling round.
pub async fn run_once(app: &App) -> Result<()> {
    if app.paused() {
        info!("engine paused; leaving new items unprocessed");
        return Ok(());
    }
    process_disclosures(app).await?;
    process_signals(app).await?;
    rebalance_13f(app).await?;
    Ok(())
}

async fn process_disclosures(app: &App) -> Result<()> {
    let cutoff = Utc::now().date_naive() - chrono::Duration::days(app.cfg.max_disclosure_age_days);
    for (key, d) in app.db.unprocessed_disclosures()? {
        let pilots: Vec<&PilotConfig> = app.enabled_pilots().into_iter().filter(|p| pilot_matches(p, &d)).collect();
        if pilots.is_empty() {
            app.db.mark_disclosure_processed(&key)?;
            continue;
        }
        let label = describe(&d);
        if d.filed_date < cutoff {
            app.notify(format!(
                "⏭ stale ({} days old), not mirrored: {label}",
                (Utc::now().date_naive() - d.filed_date).num_days()
            ))
            .await;
            app.db.mark_disclosure_processed(&key)?;
            continue;
        }
        if !d.is_mirrorable_equity() {
            let why = if d.ticker.is_none() {
                "no ticker"
            } else {
                "not a plain stock"
            };
            app.notify(format!("⏭ {why}, not mirrored: {label}")).await;
            app.db.mark_disclosure_processed(&key)?;
            continue;
        }
        let mut failed = false;
        for p in pilots {
            match intent_for_disclosure(app, p, &d).await {
                Ok(Some(intent)) => {
                    if let Err(e) = execute(app, intent).await {
                        warn!(pilot = %p.id, "execution failed: {e:#}");
                        app.notify(format!("❌ {}: execution failed: {e:#}", p.id)).await;
                        failed = true;
                    }
                }
                Ok(None) => {}
                Err(e) => warn!(pilot = %p.id, "deciding on disclosure: {e:#}"),
            }
        }
        if failed {
            // Broker hiccups (disconnected, timeout) are retried on the next
            // rounds; a persistently failing item is dropped after MAX_ATTEMPTS.
            let attempts = app.db.mark_disclosure_failed(&key)?;
            if attempts >= MAX_ATTEMPTS {
                app.notify(format!("⏭ giving up after {attempts} attempts: {label}")).await;
                app.db.mark_disclosure_processed(&key)?;
            }
        } else {
            app.db.mark_disclosure_processed(&key)?;
        }
    }
    Ok(())
}

const MAX_ATTEMPTS: i64 = 3;

fn pilot_matches(p: &PilotConfig, d: &Disclosure) -> bool {
    match &p.kind {
        PilotKind::Politician { name_match, chamber, .. } => {
            chamber.is_none_or(|c| c == d.chamber) && d.politician.to_ascii_lowercase().contains(&name_match.to_ascii_lowercase())
        }
        _ => false,
    }
}

pub fn describe(d: &Disclosure) -> String {
    format!(
        "{} ({}) {} {} ${:.0}-${:.0}, tx {}, filed {} [{}]",
        d.politician,
        d.chamber,
        d.side,
        d.ticker.as_deref().unwrap_or(&d.asset),
        d.amount_low,
        d.amount_high,
        d.tx_date,
        d.filed_date,
        d.source
    )
}

async fn intent_for_disclosure(app: &App, p: &PilotConfig, d: &Disclosure) -> Result<Option<Intent>> {
    let PilotKind::Politician {
        per_trade_usd,
        scale_by_amount,
        max_position_usd,
        ..
    } = &p.kind
    else {
        return Ok(None);
    };
    let ticker = d.ticker.clone().expect("checked by is_mirrorable_equity");
    let reason = describe(d);
    match d.side {
        Side::Purchase => {
            let base = per_trade_usd * if *scale_by_amount { amount_weight(d.amount_low) } else { 1.0 };
            let notional = cap_buy(app, p, &ticker, base, *max_position_usd)?;
            if notional < 1.0 {
                app.notify(format!(
                    "⏭ {}: allocation/position cap exhausted, not buying {ticker} — {reason}",
                    p.id
                ))
                .await;
                return Ok(None);
            }
            Ok(Some(Intent {
                pilot_id: p.id.clone(),
                ticker,
                side: OrderSide::Buy,
                qty: None,
                notional: Some(notional),
                reason,
            }))
        }
        Side::Sale | Side::SalePartial => {
            let Some(pos) = app.db.position(&p.id, &ticker)? else {
                info!(pilot = %p.id, ticker, "sale disclosed but pilot holds nothing; ignoring");
                return Ok(None);
            };
            let qty = if d.side == Side::SalePartial {
                (pos.qty / 2.0).floor().max(1.0)
            } else {
                pos.qty
            };
            Ok(Some(Intent {
                pilot_id: p.id.clone(),
                ticker,
                side: OrderSide::Sell,
                qty: Some(qty),
                notional: None,
                reason,
            }))
        }
        Side::Exchange => Ok(None),
    }
}

/// Applies the pilot allocation, the per-ticker cap and the global order cap.
fn cap_buy(app: &App, p: &PilotConfig, ticker: &str, wanted: f64, max_position_usd: Option<f64>) -> Result<f64> {
    let mut notional = wanted.min(app.cfg.max_order_usd);
    let remaining = app.pilot_allocation(p) - app.db.pilot_exposure(&p.id)?;
    notional = notional.min(remaining);
    if let Some(cap) = max_position_usd {
        let held = app.db.position(&p.id, ticker)?.map(|x| x.cost_basis).unwrap_or(0.0);
        notional = notional.min(cap - held);
    }
    Ok(notional.max(0.0))
}

async fn process_signals(app: &App) -> Result<()> {
    let news_pilots: Vec<&PilotConfig> = app
        .enabled_pilots()
        .into_iter()
        .filter(|p| matches!(p.kind, PilotKind::News { .. }))
        .collect();
    for row in app.db.unprocessed_signals()? {
        let s = &row.signal;
        for p in &news_pilots {
            let PilotKind::News {
                per_trade_usd,
                min_confidence,
                max_position_usd,
            } = &p.kind
            else {
                continue;
            };
            let reason = format!("news {} {} conf {:.2}: {}", s.direction, s.ticker, s.confidence, s.rationale);
            if s.confidence < *min_confidence {
                info!(ticker = %s.ticker, confidence = s.confidence, "signal below threshold");
                continue;
            }
            let intent = match s.direction {
                SignalDirection::Buy => {
                    let notional = cap_buy(app, p, &s.ticker, *per_trade_usd, *max_position_usd)?;
                    if notional < 1.0 {
                        app.notify(format!(
                            "⏭ {}: allocation/position cap exhausted, not buying {} — {reason}",
                            p.id, s.ticker
                        ))
                        .await;
                        continue;
                    }
                    Intent {
                        pilot_id: p.id.clone(),
                        ticker: s.ticker.clone(),
                        side: OrderSide::Buy,
                        qty: None,
                        notional: Some(notional),
                        reason,
                    }
                }
                SignalDirection::Sell => {
                    let Some(pos) = app.db.position(&p.id, &s.ticker)? else {
                        continue;
                    };
                    Intent {
                        pilot_id: p.id.clone(),
                        ticker: s.ticker.clone(),
                        side: OrderSide::Sell,
                        qty: Some(pos.qty),
                        notional: None,
                        reason,
                    }
                }
                SignalDirection::None => continue,
            };
            if let Err(e) = execute(app, intent).await {
                warn!(pilot = %p.id, "execution failed: {e:#}");
                app.notify(format!("❌ {}: execution failed: {e:#}", p.id)).await;
            }
        }
        app.db.mark_signal_processed(row.id)?;
    }
    Ok(())
}

async fn rebalance_13f(app: &App) -> Result<()> {
    for p in app.enabled_pilots() {
        let PilotKind::ThirteenF {
            cik,
            top_n,
            min_trade_usd,
        } = &p.kind
        else {
            continue;
        };
        let pending_key = format!("13f_rebalance_pending:{cik}");
        let Some(accession) = app.db.get_kv(&pending_key)?.filter(|a| !a.is_empty()) else {
            continue;
        };
        let mut holdings: Vec<_> = app
            .db
            .latest_holdings(*cik)?
            .into_iter()
            .filter(|h| h.ticker.is_some())
            .collect();
        holdings.sort_by(|a, b| b.value.total_cmp(&a.value));
        holdings.truncate(*top_n);
        let total: f64 = holdings.iter().map(|h| h.value).sum();
        if total <= 0.0 {
            warn!(pilot = %p.id, "13F snapshot has no mappable holdings; skipping rebalance");
            app.db.del_kv(&pending_key)?;
            continue;
        }
        let allocation = app.pilot_allocation(p);
        let mut unpriced = 0usize;
        app.notify(format!(
            "📊 {}: rebalancing to 13F {} ({} positions, ${allocation:.0})",
            p.id,
            accession,
            holdings.len()
        ))
        .await;

        let mut targets: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
        for h in &holdings {
            targets.insert(h.ticker.clone().unwrap(), allocation * h.value / total);
        }
        // Sell what the filer no longer holds, then adjust the rest.
        for pos in app.db.positions(Some(&p.id))? {
            if !targets.contains_key(&pos.ticker) {
                let intent = Intent {
                    pilot_id: p.id.clone(),
                    ticker: pos.ticker.clone(),
                    side: OrderSide::Sell,
                    qty: Some(pos.qty),
                    notional: None,
                    reason: format!("13F {accession}: no longer held"),
                };
                if let Err(e) = execute(app, intent).await {
                    warn!(pilot = %p.id, ticker = %pos.ticker, "rebalance sell failed: {e:#}");
                }
            }
        }
        for (ticker, target) in targets {
            let held = app.db.position(&p.id, &ticker)?;
            let price = match app.broker.last_price(&ticker).await {
                Ok(px) => px,
                Err(e) => {
                    warn!(pilot = %p.id, ticker, "no price, leg deferred: {e:#}");
                    unpriced += 1;
                    continue;
                }
            };
            let current_value = held.as_ref().map(|h| h.qty * price).unwrap_or(0.0);
            let delta = target - current_value;
            let intent = if delta >= *min_trade_usd {
                let notional = delta.min(app.cfg.max_order_usd);
                Intent {
                    pilot_id: p.id.clone(),
                    ticker,
                    side: OrderSide::Buy,
                    qty: None,
                    notional: Some(notional),
                    reason: format!("13F {accession}: target ${target:.0}"),
                }
            } else if delta <= -min_trade_usd {
                let qty = ((-delta) / price).floor().min(held.map(|h| h.qty).unwrap_or(0.0));
                if qty < 1.0 {
                    continue;
                }
                Intent {
                    pilot_id: p.id.clone(),
                    ticker,
                    side: OrderSide::Sell,
                    qty: Some(qty),
                    notional: None,
                    reason: format!("13F {accession}: target ${target:.0}"),
                }
            } else {
                continue;
            };
            if let Err(e) = execute(app, intent).await {
                warn!(pilot = %p.id, "rebalance leg failed: {e:#}");
                unpriced += 1;
            }
        }
        if unpriced == 0 {
            app.db.del_kv(&pending_key)?;
            app.notify(format!("📊 {}: rebalance to 13F {accession} complete", p.id))
                .await;
        } else {
            // Positions already adjusted are idempotent; retry the rest next round.
            app.notify(format!(
                "⚠️ {}: {unpriced} rebalance leg(s) deferred (no price / broker error); retrying next round",
                p.id
            ))
            .await;
        }
    }
    Ok(())
}

/// Prices, sizes and (unless dry-run) submits an intent, then records it in
/// the order log and the pilot ledger.
pub async fn execute(app: &App, intent: Intent) -> Result<Outcome> {
    let dry_run = app.dry_run();
    let tag = if dry_run { "[DRY RUN] " } else { "" };
    let side_str = intent.side.as_str();

    let price = match app.broker.last_price(&intent.ticker).await {
        Ok(px) => Some(px),
        Err(e) if dry_run => {
            warn!(ticker = %intent.ticker, "no price (IBKR offline?): {e:#}");
            None
        }
        Err(e) => return Err(e),
    };

    let qty = match (intent.qty, intent.notional, price) {
        (Some(q), _, _) => q.floor(),
        (None, Some(n), Some(px)) => (n / px).floor(),
        (None, Some(_), None) => 0.0,
        (None, None, _) => anyhow::bail!("intent without quantity or notional"),
    };
    let notional = match price {
        Some(px) => qty * px,
        None => intent.notional.unwrap_or(0.0),
    };

    if qty < 1.0 {
        let why = match price {
            Some(px) => format!("rounds to 0 shares (${:.0} at ${px:.2})", intent.notional.unwrap_or(0.0)),
            None => "no price available".to_string(),
        };
        app.db.insert_order(&NewOrder {
            pilot_id: &intent.pilot_id,
            ticker: &intent.ticker,
            side: side_str,
            qty: 0.0,
            price,
            notional,
            ib_order_id: None,
            status: "skipped",
            reason: &format!("{why}; {}", intent.reason),
        })?;
        app.notify(format!(
            "⏭ {} {} {}: {why} — {}",
            intent.pilot_id, side_str, intent.ticker, intent.reason
        ))
        .await;
        return Ok(Outcome::Skipped);
    }
    if notional > app.cfg.max_order_usd + 0.01 {
        anyhow::bail!(
            "order notional ${notional:.0} exceeds max_order_usd ${:.0}",
            app.cfg.max_order_usd
        );
    }

    let px = price.expect("qty >= 1 implies a price");
    let (status, ib_order_id) = if dry_run {
        ("dry_run", None)
    } else {
        let id = app.broker.place_market(&intent.ticker, intent.side, qty).await?;
        ("submitted", Some(id as i64))
    };
    app.db.insert_order(&NewOrder {
        pilot_id: &intent.pilot_id,
        ticker: &intent.ticker,
        side: side_str,
        qty,
        price: Some(px),
        notional,
        ib_order_id,
        status,
        reason: &intent.reason,
    })?;
    let signed = if intent.side == OrderSide::Buy { qty } else { -qty };
    app.db.apply_fill(&intent.pilot_id, &intent.ticker, signed, px)?;

    let icon = if intent.side == OrderSide::Buy { "🟢" } else { "🔴" };
    app.notify(format!(
        "{tag}{icon} {side_str} {qty:.0} {} @ ~${px:.2} (${notional:.0}) — pilot {}{}\n{}",
        intent.ticker,
        intent.pilot_id,
        ib_order_id.map(|id| format!(", IB #{id}")).unwrap_or_default(),
        intent.reason
    ))
    .await;
    Ok(if dry_run { Outcome::DryRun } else { Outcome::Placed })
}
