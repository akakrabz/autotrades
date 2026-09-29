//! autotrades — mirror publicly disclosed trades into an IBKR paper account.

mod app;
mod broker;
mod config;
mod db;
mod engine;
mod model;
mod sources;
mod telegram;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokio::sync::{Notify, mpsc};
use tracing::{error, info, warn};

use crate::app::App;
use crate::broker::Broker;
use crate::config::{Config, Secrets};
use crate::db::Db;
use crate::sources::SourceCtx;

#[derive(Parser)]
#[command(
    name = "autotrades",
    version,
    about = "Mirror STOCK Act / 13F / news trades into an IBKR paper account"
)]
struct Cli {
    /// Path to the TOML config (default: $AUTOTRADES_CONFIG or ./autotrades.toml)
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the poller, engine and Telegram bot (default)
    Run,
    /// Poll every source once, run the engine once, and exit
    Once,
    /// Connect to TWS/Gateway and print the account summary and positions
    Ibkr,
    /// Parse a House PTR PDF (or pdftotext output) and print the rows
    ParsePtr { path: PathBuf },
    /// Print the effective configuration and pilot states
    Config,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,ibapi=warn".into()))
        .init();

    let cli = Cli::parse();
    let config_path = cli
        .config
        .or_else(|| std::env::var_os("AUTOTRADES_CONFIG").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("autotrades.toml"));

    if let Some(Cmd::ParsePtr { path }) = &cli.cmd {
        return parse_ptr(path).await;
    }

    let cfg = Config::load(&config_path)?;
    let secrets = Secrets::from_env()?;
    let db = Arc::new(Db::open(&cfg.db_path)?);
    let (events_tx, events_rx) = mpsc::channel::<String>(256);
    let broker = Broker::new(cfg.ibkr.clone(), db.clone(), events_tx.clone());
    let app = Arc::new(App {
        cfg,
        secrets,
        db,
        broker,
        events: events_tx,
        sync_now: Notify::new(),
    });

    match cli.cmd.unwrap_or(Cmd::Run) {
        Cmd::Run => run(app, events_rx).await,
        Cmd::Once => {
            let ctx = SourceCtx::new(&app.cfg, &app.secrets, app.db.clone())?;
            let report = sources::poll_all(&ctx, &app.cfg, &app.secrets).await;
            info!(?report, "poll complete");
            engine::run_once(&app).await?;
            Ok(())
        }
        Cmd::Ibkr => {
            let s = app.broker.summary().await?;
            println!(
                "account {}: net liquidation ${:.2}, cash ${:.2}, buying power ${:.2}",
                s.account, s.net_liquidation, s.cash, s.buying_power
            );
            for p in app.broker.positions().await? {
                println!("{} {:>10.2} {:<8} avg {:.2}", p.account, p.qty, p.symbol, p.avg_cost);
            }
            let px = app.broker.last_price("SPY").await?;
            println!("SPY last close {px:.2}");
            Ok(())
        }
        Cmd::Config => {
            println!("config: {}", config_path.display());
            println!("dry_run: {}  paused: {}", app.dry_run(), app.paused());
            for p in &app.cfg.pilots {
                println!(
                    "{:<12} enabled={:<5} allocation=${:<8.0} deployed=${:.0}",
                    p.id,
                    app.pilot_enabled(p),
                    app.pilot_allocation(p),
                    app.db.pilot_exposure(&p.id).unwrap_or(0.0)
                );
            }
            Ok(())
        }
        Cmd::ParsePtr { .. } => unreachable!(),
    }
}

async fn run(app: Arc<App>, events_rx: mpsc::Receiver<String>) -> Result<()> {
    // Telegram (optional): commands in, notifications out.
    let mut events_rx = Some(events_rx);
    if app.cfg.telegram.enabled {
        match (&app.secrets.telegram_bot_token, app.secrets.telegram_owner_id) {
            (Some(token), Some(owner)) => {
                let bot_app = app.clone();
                let rx = events_rx.take().unwrap();
                let token = token.clone();
                tokio::spawn(async move {
                    if let Err(e) = telegram::run(bot_app, token, owner, rx).await {
                        error!("telegram bot stopped: {e:#}");
                    }
                });
            }
            _ => warn!("telegram.enabled but TELEGRAM_BOT_TOKEN / TELEGRAM_OWNER_ID are not set; running without the bot"),
        }
    }
    if let Some(mut rx) = events_rx {
        // Nobody is listening; drain so senders never block.
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
    }

    // Warm the broker connection so misconfiguration shows up immediately.
    match app.broker.client().await {
        Ok(_) => {}
        Err(e) => {
            warn!("IBKR not reachable at start ({e:#}); will retry on demand");
            app.notify(format!("⚠️ IBKR not reachable: {e:#}")).await;
        }
    }

    let ctx = SourceCtx::new(&app.cfg, &app.secrets, app.db.clone())?;
    let interval = Duration::from_secs(app.cfg.poll_interval_secs);
    info!(
        interval_secs = interval.as_secs(),
        pilots = app.cfg.pilots.len(),
        "autotrades running"
    );

    loop {
        let report = sources::poll_all(&ctx, &app.cfg, &app.secrets).await;
        if !report.is_quiet() {
            app.notify(format!(
                "🔎 poll: {} new filings, {} new disclosures, {} 13F snapshots, {} headlines, {} errors",
                report.filings, report.disclosures, report.holdings, report.news, report.errors
            ))
            .await;
        }
        if let Err(e) = engine::run_once(&app).await {
            error!("engine failed: {e:#}");
            app.notify(format!("❌ engine error: {e:#}")).await;
        }
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = app.sync_now.notified() => info!("manual sync requested"),
            _ = tokio::signal::ctrl_c() => {
                info!("shutting down");
                return Ok(());
            }
        }
    }
}

async fn parse_ptr(path: &PathBuf) -> Result<()> {
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("reading {}", path.display()))?;
    let text = if bytes.starts_with(b"%PDF") {
        sources::house::pdf_to_text(&bytes).await?
    } else {
        String::from_utf8_lossy(&bytes).into_owned()
    };
    let rows = sources::house::parse_ptr_text(&text);
    println!("{} rows", rows.len());
    for r in rows {
        println!(
            "{:<3} {:<12} {:<4} {} {} ${:.0}-${:.0}  {}",
            r.owner.as_deref().unwrap_or("-"),
            r.side.to_string(),
            r.asset_type.as_deref().unwrap_or("-"),
            r.tx_date,
            r.ticker.as_deref().unwrap_or("-"),
            r.amount_low,
            r.amount_high,
            r.asset
        );
    }
    Ok(())
}
