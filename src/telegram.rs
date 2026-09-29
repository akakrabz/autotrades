//! Telegram control surface. One owner chat is allowed to issue commands;
//! everything else is ignored. Channel posts (add the bot as an admin of a
//! news channel) and non-command messages from the owner (e.g. forwarded
//! headlines) are fed into the news pipeline.

use std::sync::Arc;

use anyhow::Result;
use teloxide::prelude::*;
use teloxide::types::{ChatId, MessageId};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::app::App;
use crate::db::NewsItem;
use crate::sources::news::news_id;

const HELP: &str = "autotrades commands:\n\
/status — connection, mode, pilots, exposure\n\
/pilots — list pilots and their state\n\
/follow <id> [usd] — enable a pilot, optionally set its allocation\n\
/unfollow <id> — disable a pilot (positions are kept)\n\
/pause | /resume — global kill switch for new orders\n\
/dryrun [on|off] — show or set dry-run mode\n\
/trades [n] — latest disclosures seen\n\
/orders [n] — latest orders\n\
/positions — ledger per pilot\n\
/signals [n] — latest news signals\n\
/ibkr — account summary and broker positions\n\
/sync — poll all sources now\n\
/reset confirm — wipe the ledger and order log (not IBKR)\n\
Send or forward a headline to score it as news.";

/// Runs the bot until the process exits. `events` carries outbound
/// notifications produced anywhere in the app.
pub async fn run(app: Arc<App>, token: String, owner: i64, mut events: mpsc::Receiver<String>) -> Result<()> {
    let bot = Bot::new(token);
    let me = bot.get_me().await?;
    info!(username = ?me.username(), owner, "telegram bot online");

    let outbound = bot.clone();
    tokio::spawn(async move {
        while let Some(msg) = events.recv().await {
            for chunk in chunks(&msg, 3900) {
                if let Err(e) = outbound.send_message(ChatId(owner), chunk).await {
                    warn!("telegram send failed: {e}");
                }
            }
        }
    });
    let _ = bot
        .send_message(ChatId(owner), format!("autotrades online ({})\n{HELP}", mode_line(&app)))
        .await;

    let handler = dptree::entry()
        .branch(Update::filter_message().endpoint(on_message))
        .branch(Update::filter_channel_post().endpoint(on_channel_post));
    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![app, Owner(owner)])
        .build()
        .dispatch()
        .await;
    Ok(())
}

#[derive(Clone, Copy)]
struct Owner(i64);

async fn on_message(bot: Bot, msg: Message, app: Arc<App>, owner: Owner) -> Result<()> {
    if msg.chat.id.0 != owner.0 {
        warn!(chat = msg.chat.id.0, "ignoring message from non-owner chat");
        return Ok(());
    }
    let Some(text) = msg.text() else { return Ok(()) };
    let reply = if text.starts_with('/') {
        handle_command(&app, text).await
    } else {
        ingest_headline(&app, "telegram:owner", msg.id, text)
    };
    for chunk in chunks(&reply, 3900) {
        bot.send_message(msg.chat.id, chunk).await?;
    }
    Ok(())
}

async fn on_channel_post(msg: Message, app: Arc<App>) -> Result<()> {
    if let Some(text) = msg.text() {
        let source = format!("telegram:{}", msg.chat.title().unwrap_or("channel"));
        info!(source, "channel post received");
        ingest_headline(&app, &source, msg.id, text);
    }
    Ok(())
}

fn ingest_headline(app: &App, source: &str, id: MessageId, text: &str) -> String {
    if !app.cfg.news.enabled {
        return "news pipeline is disabled (news.enabled = false)".into();
    }
    let title: String = text.lines().next().unwrap_or("").chars().take(300).collect();
    if title.trim().is_empty() {
        return "empty headline".into();
    }
    let item = NewsItem {
        id: news_id(source, &id.0.to_string()),
        source: source.to_string(),
        title,
        link: None,
    };
    match app.db.insert_news(&item) {
        Ok(true) => "queued for scoring on the next round".into(),
        Ok(false) => "already queued".into(),
        Err(e) => format!("failed to store headline: {e}"),
    }
}

async fn handle_command(app: &Arc<App>, text: &str) -> String {
    let mut parts = text.split_whitespace();
    let cmd = parts
        .next()
        .unwrap_or("")
        .split('@')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let args: Vec<&str> = parts.collect();
    let result: Result<String> = match cmd.as_str() {
        "/start" | "/help" => Ok(HELP.to_string()),
        "/status" => status(app).await,
        "/pilots" => Ok(pilots(app)),
        "/follow" => follow(app, &args, true),
        "/unfollow" => follow(app, &args, false),
        "/pause" => app.set_paused(true).map(|_| "paused: no new orders will be placed".into()),
        "/resume" => app.set_paused(false).map(|_| "resumed".into()),
        "/dryrun" => match args.first().map(|s| s.to_ascii_lowercase()).as_deref() {
            None => Ok(mode_line(app)),
            Some("on") => app.set_dry_run(true).map(|_| "dry-run ON: orders are simulated".into()),
            Some("off") => app.set_dry_run(false).map(|_| "dry-run OFF: orders go to IBKR".into()),
            Some(_) => Ok("usage: /dryrun [on|off]".into()),
        },
        "/trades" => app
            .db
            .recent_disclosures(n_arg(&args, 10))
            .map(|v| list_or("no disclosures yet", v)),
        "/orders" => app.db.recent_orders(n_arg(&args, 10)).map(|v| {
            list_or(
                "no orders yet",
                v.iter()
                    .map(|o| {
                        format!(
                            "#{} {} {} {} {:.0} {} @ {} ${:.0} [{}{}]{}\n  {}",
                            o.id,
                            &o.ts[..16],
                            o.pilot_id,
                            o.side,
                            o.qty,
                            o.ticker,
                            o.price.map(|p| format!("{p:.2}")).unwrap_or_else(|| "-".into()),
                            o.notional,
                            o.status,
                            o.fill_price.map(|p| format!(" @ {p:.2}")).unwrap_or_default(),
                            o.ib_order_id.map(|i| format!(" IB#{i}")).unwrap_or_default(),
                            o.reason.chars().take(120).collect::<String>()
                        )
                    })
                    .collect(),
            )
        }),
        "/positions" => positions(app),
        "/signals" => app.db.recent_signals(n_arg(&args, 10)).map(|v| {
            list_or(
                "no signals yet",
                v.iter()
                    .map(|r| {
                        format!(
                            "{} {} {} conf {:.2}: {}",
                            &r.ts[..16],
                            r.signal.direction,
                            r.signal.ticker,
                            r.signal.confidence,
                            r.signal.rationale
                        )
                    })
                    .collect(),
            )
        }),
        "/ibkr" => ibkr(app).await,
        "/sync" => {
            app.sync_now.notify_one();
            Ok("polling all sources now".into())
        }
        "/reset" => {
            if args.first() == Some(&"confirm") {
                reset(app)
            } else {
                Ok("this wipes the ledger and order log (not your IBKR account). Send /reset confirm".into())
            }
        }
        _ => Ok(format!("unknown command {cmd}\n\n{HELP}")),
    };
    result.unwrap_or_else(|e| format!("error: {e:#}"))
}

fn n_arg(args: &[&str], default: usize) -> usize {
    args.first().and_then(|s| s.parse().ok()).unwrap_or(default).clamp(1, 50)
}

fn list_or(empty: &str, lines: Vec<String>) -> String {
    if lines.is_empty() {
        empty.to_string()
    } else {
        lines.join("\n")
    }
}

fn mode_line(app: &App) -> String {
    format!(
        "mode: {}{}",
        if app.dry_run() { "DRY RUN" } else { "LIVE (paper account)" },
        if app.paused() { ", PAUSED" } else { "" }
    )
}

async fn status(app: &App) -> Result<String> {
    let connected = app.broker.is_connected().await;
    let mut out = vec![
        mode_line(app),
        format!(
            "ibkr: {} ({})",
            if connected { "connected" } else { "not connected" },
            app.broker.address()
        ),
        format!(
            "poll every {}s; dedupe window {} days",
            app.cfg.poll_interval_secs, app.cfg.max_disclosure_age_days
        ),
    ];
    out.push(pilots(app));
    Ok(out.join("\n"))
}

fn pilots(app: &App) -> String {
    let lines: Vec<String> = app
        .cfg
        .pilots
        .iter()
        .map(|p| {
            let exposure = app.db.pilot_exposure(&p.id).unwrap_or(0.0);
            format!(
                "{} {} — {} — ${:.0} / ${:.0} deployed",
                if app.pilot_enabled(p) { "✅" } else { "⛔" },
                p.id,
                kind_label(p),
                exposure,
                app.pilot_allocation(p)
            )
        })
        .collect();
    list_or("no pilots configured", lines)
}

fn kind_label(p: &crate::config::PilotConfig) -> String {
    use crate::config::PilotKind::*;
    match &p.kind {
        Politician {
            name_match,
            chamber,
            per_trade_usd,
            ..
        } => format!(
            "politician \"{name_match}\"{} ${per_trade_usd:.0}/trade",
            chamber.map(|c| format!(" ({c})")).unwrap_or_default()
        ),
        ThirteenF { cik, top_n, .. } => format!("13F CIK {cik} top {top_n}"),
        News {
            per_trade_usd,
            min_confidence,
            ..
        } => format!("news ${per_trade_usd:.0}/trade conf≥{min_confidence:.2}"),
    }
}

fn follow(app: &App, args: &[&str], enable: bool) -> Result<String> {
    let Some(id) = args.first() else {
        return Ok("usage: /follow <pilot-id> [usd]".into());
    };
    let Some(p) = app.cfg.pilot(id) else {
        return Ok(format!(
            "unknown pilot {id}. Known: {}",
            app.cfg.pilots.iter().map(|p| p.id.as_str()).collect::<Vec<_>>().join(", ")
        ));
    };
    let usd = match args.get(1) {
        Some(s) => match s.trim_start_matches('$').replace(',', "").parse::<f64>() {
            Ok(v) if v > 0.0 => Some(v),
            _ => return Ok("allocation must be a positive number".into()),
        },
        None => None,
    };
    app.db.set_pilot_state(&p.id, enable, usd)?;
    Ok(format!(
        "{} {} (allocation ${:.0})",
        if enable { "following" } else { "unfollowed" },
        p.id,
        app.pilot_allocation(p)
    ))
}

fn positions(app: &App) -> Result<String> {
    let rows = app.db.positions(None)?;
    Ok(list_or(
        "no positions in the ledger",
        rows.iter()
            .map(|p| {
                format!(
                    "{} {:.0} {} (cost ${:.0}, avg ${:.2})",
                    p.pilot_id,
                    p.qty,
                    p.ticker,
                    p.cost_basis,
                    p.cost_basis / p.qty.max(1.0)
                )
            })
            .collect(),
    ))
}

async fn ibkr(app: &App) -> Result<String> {
    let s = app.broker.summary().await?;
    let mut out = vec![format!(
        "account {}: net liq ${:.0}, cash ${:.0}, buying power ${:.0}",
        s.account, s.net_liquidation, s.cash, s.buying_power
    )];
    let pos = app.broker.positions().await?;
    if pos.is_empty() {
        out.push("no broker positions".into());
    }
    for p in pos {
        out.push(format!("{} {:.0} {} avg ${:.2}", p.account, p.qty, p.symbol, p.avg_cost));
    }
    Ok(out.join("\n"))
}

fn reset(app: &App) -> Result<String> {
    app.db.reset_ledger()?;
    Ok("ledger and order log cleared".into())
}

fn chunks(s: &str, max: usize) -> Vec<String> {
    if s.chars().count() <= max {
        return vec![s.to_string()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    for line in s.lines() {
        if cur.chars().count() + line.chars().count() + 1 > max && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push('\n');
        }
        cur.push_str(line);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}
