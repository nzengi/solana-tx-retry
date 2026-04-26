//! Leader schedule tracking for intelligent transaction routing.
//!
//! Solana rotates the block producer (leader) every 4 slots (~1.6 seconds).
//! Sending transactions directly to the upcoming leader's TPU port reduces
//! latency and avoids unnecessary hops through the gossip network.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{clock::Slot, pubkey::Pubkey};
use tokio::sync::RwLock;
use tracing::{debug, info};

use crate::error::{Result, RetryError};

/// Tracks the Solana leader schedule and provides TPU addresses
/// for upcoming leaders to enable direct transaction routing.
pub struct LeaderTracker {
    rpc_client: Arc<RpcClient>,
    /// slot -> leader pubkey
    schedule: Arc<RwLock<HashMap<Slot, Pubkey>>>,
    /// pubkey -> TPU socket address (from cluster nodes)
    tpu_map: Arc<RwLock<HashMap<Pubkey, SocketAddr>>>,
    /// When the schedule was last refreshed
    last_refresh: Arc<RwLock<Instant>>,
    /// Refresh interval (default: every epoch = ~2 days, but we refresh more often)
    refresh_interval: Duration,
    /// How many slots ahead to look for leaders
    lookahead: usize,
}

impl LeaderTracker {
    /// Create a new LeaderTracker connected to the given RPC endpoint.
    pub fn new(rpc_client: Arc<RpcClient>, lookahead: usize) -> Self {
        Self {
            rpc_client,
            schedule: Arc::new(RwLock::new(HashMap::new())),
            tpu_map: Arc::new(RwLock::new(HashMap::new())),
            last_refresh: Arc::new(RwLock::new(
                Instant::now() - Duration::from_secs(3600), // force immediate refresh
            )),
            refresh_interval: Duration::from_secs(60), // refresh every minute
            lookahead,
        }
    }

    /// Initialize by fetching the schedule and cluster node info.
    pub async fn initialize(&self) -> Result<()> {
        self.refresh_schedule().await?;
        self.refresh_tpu_map().await?;
        info!("LeaderTracker initialized successfully");
        Ok(())
    }

    /// Get the current slot's leader pubkey.
    pub async fn get_current_leader(&self) -> Result<Pubkey> {
        self.ensure_fresh().await?;
        let slot = self.get_current_slot().await?;
        let schedule = self.schedule.read().await;

        // Look in a small window around the current slot
        for offset in 0..4u64 {
            if let Some(leader) = schedule.get(&(slot + offset)) {
                return Ok(*leader);
            }
        }

        Err(RetryError::LeaderScheduleFetchFailed(
            format!("No leader found for slot {}", slot),
        ))
    }

    /// Get TPU addresses for the next N upcoming leaders.
    /// Returns (slot, pubkey, tpu_addr) tuples.
    pub async fn get_upcoming_leaders(&self) -> Result<Vec<(Slot, Pubkey, Option<SocketAddr>)>> {
        self.ensure_fresh().await?;
        let current_slot = self.get_current_slot().await?;

        let schedule = self.schedule.read().await;
        let tpu_map = self.tpu_map.read().await;

        let mut results = Vec::new();
        let mut found = 0;
        let mut slot = current_slot;

        while found < self.lookahead && slot < current_slot + 1000 {
            if let Some(leader) = schedule.get(&slot) {
                let tpu_addr = tpu_map.get(leader).copied();
                results.push((slot, *leader, tpu_addr));
                found += 1;
            }
            slot += 1;
        }

        debug!("Found {} upcoming leaders starting from slot {}", results.len(), current_slot);
        Ok(results)
    }

    /// Get the TPU address of the current leader (if available).
    /// This is the address to send raw transactions to for minimal latency.
    pub async fn get_current_tpu_addr(&self) -> Result<Option<SocketAddr>> {
        let leader = self.get_current_leader().await?;
        let tpu_map = self.tpu_map.read().await;
        Ok(tpu_map.get(&leader).copied())
    }

    /// Check if the schedule needs refreshing and do so if needed.
    async fn ensure_fresh(&self) -> Result<()> {
        let last = *self.last_refresh.read().await;
        if last.elapsed() > self.refresh_interval {
            self.refresh_schedule().await?;
            self.refresh_tpu_map().await?;
        }
        Ok(())
    }

    /// Fetch the leader schedule from the RPC.
    async fn refresh_schedule(&self) -> Result<()> {
        debug!("Refreshing leader schedule from RPC...");

        let schedule_response = self
            .rpc_client
            .get_leader_schedule(None)
            .await
            .map_err(|e| RetryError::LeaderScheduleFetchFailed(e.to_string()))?;

        let Some(schedule_map) = schedule_response else {
            return Err(RetryError::LeaderScheduleFetchFailed(
                "RPC returned empty leader schedule".to_string(),
            ));
        };

        // Get current epoch info to compute absolute slot numbers
        let epoch_info = self
            .rpc_client
            .get_epoch_info()
            .await
            .map_err(|e| RetryError::LeaderScheduleFetchFailed(e.to_string()))?;

        let epoch_start_slot = epoch_info.absolute_slot - epoch_info.slot_index;

        let mut schedule = self.schedule.write().await;
        schedule.clear();

        for (pubkey_str, slots) in schedule_map {
            if let Ok(pubkey) = pubkey_str.parse::<Pubkey>() {
                for slot_index in slots {
                    let absolute_slot = epoch_start_slot + slot_index as u64;
                    schedule.insert(absolute_slot, pubkey);
                }
            }
        }

        *self.last_refresh.write().await = Instant::now();
        info!("Leader schedule refreshed: {} slots mapped", schedule.len());
        Ok(())
    }

    /// Fetch cluster node info to build the pubkey -> TPU addr mapping.
    async fn refresh_tpu_map(&self) -> Result<()> {
        debug!("Refreshing cluster node TPU map...");

        let cluster_nodes = self
            .rpc_client
            .get_cluster_nodes()
            .await
            .map_err(|e| RetryError::LeaderScheduleFetchFailed(e.to_string()))?;

        let mut tpu_map = self.tpu_map.write().await;
        tpu_map.clear();

        let mut mapped = 0usize;
        for node in cluster_nodes {
            if let Ok(pubkey) = node.pubkey.parse::<Pubkey>() {
                if let Some(addr) = node.tpu {
                    tpu_map.insert(pubkey, addr);
                    mapped += 1;
                }
            }
        }

        info!("TPU map refreshed: {} validators with TPU addresses", mapped);
        Ok(())
    }

    /// Get the current slot number from the RPC.
    async fn get_current_slot(&self) -> Result<Slot> {
        self.rpc_client
            .get_slot()
            .await
            .map_err(|e| RetryError::LeaderScheduleFetchFailed(e.to_string()))
    }
}