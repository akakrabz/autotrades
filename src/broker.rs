//! Interactive Brokers execution through the TWS API socket (`ibapi` crate).
//!
//! The connection is lazy and self-healing: every call obtains the current
//! client or reconnects. On connect we verify the account is a paper account
//! (IDs start with `DU`) unless explicitly allowed otherwise, switch to
//! delayed market data (free on paper accounts) and start a background task
//! that mirrors order-status updates into the database.

use anyhow::{Context, Result, bail};
use ibapi::accounts::types::AccountGroup;
use ibapi::orders::OrderId;
use ibapi::prelude::*;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};
use tracing::{info, warn};

use crate::config::IbkrConfig;
use crate::db::Db;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderSide {
    Buy,
    Sell,
}

impl OrderSide {
    pub fn as_str(&self) -> &'static str {
        match self {
            OrderSide::Buy => "BUY",
            OrderSide::Sell => "SELL",
        }
    }
}

#[derive(Debug, Clone)]
pub struct BrokerPosition {
    pub account: String,
    pub symbol: String,
    pub qty: f64,
    pub avg_cost: f64,
}

#[derive(Debug, Clone)]
pub struct AccountSummary {
    pub account: String,
    pub net_liquidation: f64,
    pub cash: f64,
    pub buying_power: f64,
}

pub struct Broker {
    cfg: IbkrConfig,
    db: Arc<Db>,
    client: Mutex<Option<Arc<Client>>>,
    /// Human-readable events (fills, disconnects) for the Telegram channel.
    events: mpsc::Sender<String>,
}

impl Broker {
    pub fn new(cfg: IbkrConfig, db: Arc<Db>, events: mpsc::Sender<String>) -> Self {
        Self {
            cfg,
            db,
            client: Mutex::new(None),
            events,
        }
    }

    pub fn address(&self) -> String {
        format!("{}:{}", self.cfg.host, self.cfg.port)
    }

    pub async fn is_connected(&self) -> bool {
        matches!(&*self.client.lock().await, Some(c) if c.is_connected())
    }

    /// Returns a live client, connecting if necessary.
    pub async fn client(&self) -> Result<Arc<Client>> {
        let mut guard = self.client.lock().await;
        if let Some(c) = &*guard
            && c.is_connected()
        {
            return Ok(c.clone());
        }
        let address = self.address();
        let client = tokio::time::timeout(Duration::from_secs(20), Client::connect(&address, self.cfg.client_id))
            .await
            .with_context(|| format!("timed out connecting to TWS/Gateway at {address}"))?
            .with_context(|| format!("connecting to TWS/Gateway at {address} (is the API enabled and the port right?)"))?;
        let client = Arc::new(client);

        let accounts = client.managed_accounts().await.context("listing managed accounts")?;
        if self.cfg.require_paper_account && !accounts.iter().all(|a| a.starts_with("DU") || a.starts_with("DF")) {
            bail!(
                "refusing to connect: {accounts:?} does not look like a paper account (set ibkr.require_paper_account=false to override)"
            );
        }
        if let Err(e) = client.switch_market_data_type(MarketDataType::Delayed).await {
            warn!("could not switch to delayed market data: {e}");
        }
        info!(address, accounts = ?accounts, server_version = client.server_version(), "connected to IBKR");
        self.spawn_order_monitor(client.clone());
        *guard = Some(client.clone());
        Ok(client)
    }

    fn spawn_order_monitor(&self, client: Arc<Client>) {
        let db = self.db.clone();
        let events = self.events.clone();
        tokio::spawn(async move {
            let stream = match client.order_update_stream().await {
                Ok(s) => s,
                Err(e) => {
                    warn!("order update stream unavailable: {e}");
                    return;
                }
            };
            let mut stream = stream.filter_data();
            while let Some(update) = stream.next().await {
                match update {
                    Ok(OrderUpdate::OrderStatus(s)) => {
                        let status = s.status.as_str().to_ascii_lowercase();
                        let fill = if status == "filled" { s.average_fill_price } else { None };
                        match db.update_order_status(s.order_id as i64, &status, fill) {
                            Ok(true) if status == "filled" || status == "cancelled" || status == "inactive" => {
                                let _ = events
                                    .send(format!(
                                        "IBKR order {} {}{}",
                                        s.order_id,
                                        status,
                                        fill.map(|p| format!(" @ {p:.2}")).unwrap_or_default()
                                    ))
                                    .await;
                            }
                            Ok(_) => {}
                            Err(e) => warn!("updating order status: {e}"),
                        }
                    }
                    Ok(OrderUpdate::ExecutionData(e)) => {
                        info!(
                            order_id = e.execution.order_id,
                            shares = e.execution.shares,
                            price = e.execution.price,
                            "execution"
                        );
                    }
                    Ok(_) => {}
                    Err(e) => {
                        warn!("order update stream ended: {e}");
                        break;
                    }
                }
            }
        });
    }

    fn contract(symbol: &str) -> Contract {
        // IB spells share classes with a space: BRK.B -> "BRK B".
        Contract::stock(symbol.replace('.', " ").as_str()).build()
    }

    /// Latest available price: the close of the most recent daily bar, which
    /// works with delayed data and outside trading hours.
    pub async fn last_price(&self, symbol: &str) -> Result<f64> {
        let client = self.client().await?;
        let contract = Self::contract(symbol);
        let data = tokio::time::timeout(
            Duration::from_secs(30),
            client
                .historical_data(&contract, HistoricalBarSize::Day)
                .duration(5.days())
                .fetch(),
        )
        .await
        .with_context(|| format!("timed out fetching price for {symbol}"))?
        .with_context(|| format!("fetching price for {symbol}"))?;
        let bar = data.bars.last().with_context(|| format!("no price bars for {symbol}"))?;
        if bar.close <= 0.0 {
            bail!("non-positive close for {symbol}");
        }
        Ok(bar.close)
    }

    /// Submits a market order and returns the TWS order id.
    pub async fn place_market(&self, symbol: &str, side: OrderSide, qty: f64) -> Result<i32> {
        anyhow::ensure!(qty > 0.0, "quantity must be positive");
        let client = self.client().await?;
        let contract = Self::contract(symbol);
        let builder = client.order(&contract);
        let builder = match side {
            OrderSide::Buy => builder.buy(qty),
            OrderSide::Sell => builder.sell(qty),
        };
        let OrderId(id) = tokio::time::timeout(Duration::from_secs(30), builder.market().submit())
            .await
            .context("timed out submitting order")?
            .with_context(|| format!("submitting {} {qty} {symbol}", side.as_str()))?;
        info!(order_id = id, symbol, side = side.as_str(), qty, "order submitted");
        Ok(id)
    }

    pub async fn positions(&self) -> Result<Vec<BrokerPosition>> {
        let client = self.client().await?;
        let sub = client.positions().await?;
        let mut sub = sub.filter_data();
        let mut out = Vec::new();
        let collect = async {
            while let Some(item) = sub.next().await {
                match item? {
                    PositionUpdate::Position(p) => out.push(BrokerPosition {
                        account: p.account,
                        symbol: p.contract.symbol.to_string(),
                        qty: p.position,
                        avg_cost: p.average_cost,
                    }),
                    PositionUpdate::PositionEnd => break,
                }
            }
            Ok::<(), ibapi::Error>(())
        };
        tokio::time::timeout(Duration::from_secs(20), collect)
            .await
            .context("timed out reading positions")??;
        Ok(out)
    }

    pub async fn summary(&self) -> Result<AccountSummary> {
        let client = self.client().await?;
        let tags = &[
            AccountSummaryTags::NET_LIQUIDATION,
            AccountSummaryTags::TOTAL_CASH_VALUE,
            AccountSummaryTags::BUYING_POWER,
        ];
        let sub = client.account_summary(&AccountGroup("All".to_string()), tags).await?;
        let mut sub = sub.filter_data();
        let mut s = AccountSummary {
            account: String::new(),
            net_liquidation: 0.0,
            cash: 0.0,
            buying_power: 0.0,
        };
        let collect = async {
            while let Some(item) = sub.next().await {
                match item? {
                    AccountSummaryResult::Summary(v) => {
                        s.account = v.account.to_string();
                        let val: f64 = v.value.parse().unwrap_or(0.0);
                        match v.tag.as_str() {
                            AccountSummaryTags::NET_LIQUIDATION => s.net_liquidation = val,
                            AccountSummaryTags::TOTAL_CASH_VALUE => s.cash = val,
                            AccountSummaryTags::BUYING_POWER => s.buying_power = val,
                            _ => {}
                        }
                    }
                    AccountSummaryResult::End => break,
                }
            }
            Ok::<(), ibapi::Error>(())
        };
        tokio::time::timeout(Duration::from_secs(20), collect)
            .await
            .context("timed out reading account summary")??;
        Ok(s)
    }
}
