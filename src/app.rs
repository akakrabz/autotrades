//! Process-wide state shared by the poller, the engine and the Telegram bot.

use std::sync::Arc;

use tokio::sync::{Notify, mpsc};

use crate::broker::Broker;
use crate::config::{Config, PilotConfig, Secrets};
use crate::db::Db;

pub struct App {
    pub cfg: Config,
    pub secrets: Secrets,
    pub db: Arc<Db>,
    pub broker: Broker,
    /// Outbound operator notifications (delivered to Telegram when enabled).
    pub events: mpsc::Sender<String>,
    /// Wakes the poll loop early (`/sync`).
    pub sync_now: Notify,
}

impl App {
    pub fn dry_run(&self) -> bool {
        self.db.get_flag("dry_run", self.cfg.dry_run)
    }

    pub fn set_dry_run(&self, on: bool) -> anyhow::Result<()> {
        self.db.set_flag("dry_run", on)
    }

    pub fn paused(&self) -> bool {
        self.db.get_flag("paused", false)
    }

    pub fn set_paused(&self, on: bool) -> anyhow::Result<()> {
        self.db.set_flag("paused", on)
    }

    /// Runtime state of a pilot: the database override wins over the config.
    pub fn pilot_enabled(&self, p: &PilotConfig) -> bool {
        self.db
            .pilot_state(&p.id)
            .ok()
            .flatten()
            .map(|s| s.enabled)
            .unwrap_or(p.enabled)
    }

    pub fn pilot_allocation(&self, p: &PilotConfig) -> f64 {
        self.db
            .pilot_state(&p.id)
            .ok()
            .flatten()
            .and_then(|s| s.allocation_usd)
            .unwrap_or(p.allocation_usd)
    }

    pub fn enabled_pilots(&self) -> Vec<&PilotConfig> {
        self.cfg.pilots.iter().filter(|p| self.pilot_enabled(p)).collect()
    }

    /// Sends an operator notification; never blocks the caller for long.
    pub async fn notify(&self, msg: impl Into<String>) {
        let msg = msg.into();
        tracing::info!(target: "autotrades::notify", "{msg}");
        let _ = self.events.send(msg).await;
    }
}
