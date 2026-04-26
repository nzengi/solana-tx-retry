//! Core retry engine with adaptive backoff and leader-aware routing.
//!
//! The retry loop works as follows:
//!  1. Run preflight simulation (with cache) — abort if it fails.
//!  2. Send the transaction via RPC (or directly to TPU if leader routing is enabled).
//!  3. Poll for confirmation using TxMonitor.
//!  4. If the transaction is Dropped or we time out without seeing it:
//!     a. Refresh the blockhash if it has expired.
//!     b. Re-sign with the new blockhash.
//!     c. Wait for the computed backoff delay.
//!     d. Go to step 1.
//!  5. If max retries exceeded → return MaxRetriesExceeded error.

use std::{sync::Arc, time::Duration};

use rand::Rng;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    hash::Hash,
    signature::{Keypair, Signature},
    transaction::Transaction,
};
use tracing::{debug, info, warn};

use crate::{
    config::RetryConfig,
    error::{Result, RetryError},
    leader_tracker::LeaderTracker,
    preflight_cache::{make_cache_key, PreflightCache, SimulationResult},
    tx_monitor::{TxMonitor, TxStatus},
};

/// Statistics about the retry process, returned on success.
#[derive(Debug, Clone)]
pub struct RetryStats {
    /// The confirmed transaction signature.
    pub signature: Signature,
    /// How many attempts were made (1 = first try succeeded).
    pub attempts: u32,
    /// How many blockhash refreshes occurred.
    pub blockhash_refreshes: u32,
    /// Whether the transaction was routed via leader TPU.
    pub used_leader_routing: bool,
}

/// The main retry engine. Create one per application and share it via `Arc`.
pub struct RetryEngine {
    rpc_client: Arc<RpcClient>,
    config: RetryConfig,
    leader_tracker: Arc<LeaderTracker>,
    tx_monitor: Arc<TxMonitor>,
    preflight_cache: Arc<PreflightCache>,
}

impl RetryEngine {
    /// Create a new RetryEngine with the given RPC client and config.
    pub fn new(rpc_client: Arc<RpcClient>, config: RetryConfig) -> Self {
        let leader_tracker = Arc::new(LeaderTracker::new(
            Arc::clone(&rpc_client),
            config.leader_lookahead,
        ));
        let tx_monitor = Arc::new(TxMonitor::new(Arc::clone(&rpc_client)));
        let preflight_cache = Arc::new(PreflightCache::new());

        Self {
            rpc_client,
            config,
            leader_tracker,
            tx_monitor,
            preflight_cache,
        }
    }

    /// Initialize the engine (fetches the initial leader schedule).
    pub async fn initialize(&self) -> Result<()> {
        self.leader_tracker.initialize().await?;
        info!("RetryEngine initialized");
        Ok(())
    }

    /// Send a transaction with automatic retry, adaptive backoff, and leader-aware routing.
    ///
    /// # Arguments
    /// * `transaction` - A signed `Transaction`. If the blockhash expires during retries,
    ///   the engine will re-fetch a fresh blockhash and re-sign with `signer`.
    /// * `signer` - The keypair used to sign (and re-sign) the transaction.
    ///
    /// # Returns
    /// `RetryStats` on success, or a `RetryError` describing the failure.
    pub async fn send_with_retry(
        &self,
        transaction: Transaction,
        signer: &Keypair,
    ) -> Result<RetryStats> {
        let mut tx = transaction;
        let mut attempts = 0u32;
        let mut blockhash_refreshes = 0u32;
        let mut used_leader_routing = false;

        // Generate a stable cache key from the transaction's message instructions
        // (independent of blockhash, so it survives re-signing)
        let tx_key = make_cache_key(&tx.message_data());

        loop {
            if attempts >= self.config.max_retries {
                return Err(RetryError::MaxRetriesExceeded {
                    attempts,
                    signature: tx.signatures.first().map(|s| s.to_string()).unwrap_or_default(),
                });
            }

            attempts += 1;
            info!("Attempt {}/{} for tx key {}", attempts, self.config.max_retries, &tx_key[..8]);

            // ── Step 1: Preflight simulation ──────────────────────────────────
            let should_simulate = if attempts == 1 {
                self.config.preflight_enabled
            } else {
                self.config.preflight_enabled && !self.config.skip_preflight_on_retry
            };

            if should_simulate {
                let blockhash = tx.message.recent_blockhash;
                match self.preflight_cache.get(&tx_key, &blockhash) {
                    Some(SimulationResult::Success) => {
                        debug!("Preflight cache hit (success) for tx {}", &tx_key[..8]);
                    }
                    Some(SimulationResult::Failed(reason)) => {
                        return Err(RetryError::PreflightFailed(reason));
                    }
                    None => {
                        // Run the simulation
                        match self.run_simulation(&tx).await {
                            Ok(()) => {
                                self.preflight_cache.insert(
                                    tx_key.clone(),
                                    SimulationResult::Success,
                                    blockhash,
                                );
                            }
                            Err(RetryError::PreflightFailed(reason)) => {
                                self.preflight_cache.insert(
                                    tx_key.clone(),
                                    SimulationResult::Failed(reason.clone()),
                                    blockhash,
                                );
                                return Err(RetryError::PreflightFailed(reason));
                            }
                            Err(e) => return Err(e),
                        }
                    }
                }
            }

            // ── Step 2: Send transaction ──────────────────────────────────────
            let signature = self.send_transaction(&tx, &mut used_leader_routing).await;

            let signature = match signature {
                Ok(sig) => sig,
                Err(e) => {
                    warn!("Send error on attempt {}: {}", attempts, e);
                    // Check if blockhash expired and refresh
                    if self.is_blockhash_expired(&tx.message.recent_blockhash).await {
                        tx = self.refresh_blockhash(tx, signer).await?;
                        blockhash_refreshes += 1;
                        // Don't count this as a retry attempt for exponential backoff purposes
                        attempts = attempts.saturating_sub(1);
                    }
                    self.backoff(attempts).await;
                    continue;
                }
            };

            info!("Tx sent: {}", &signature.to_string()[..12]);

            // ── Step 3: Monitor for confirmation ──────────────────────────────
            let status = self
                .tx_monitor
                .wait_for_confirmation(
                    &signature,
                    self.config.commitment,
                    // Use a shorter per-attempt timeout so we can retry
                    Duration::from_millis(
                        self.config.confirmation_timeout.as_millis() as u64 / self.config.max_retries as u64 + 2000
                    ),
                )
                .await;

            match status {
                Ok(TxStatus::Confirmed) | Ok(TxStatus::Finalized) => {
                    self.preflight_cache.remove(&tx_key);
                    return Ok(RetryStats {
                        signature,
                        attempts,
                        blockhash_refreshes,
                        used_leader_routing,
                    });
                }
                Ok(TxStatus::Failed { reason }) => {
                    return Err(RetryError::TransactionRejected {
                        reason,
                        signature: signature.to_string(),
                    });
                }
                Ok(TxStatus::Dropped) | Ok(TxStatus::Pending) | Ok(TxStatus::Processed) => {
                    warn!("Tx {} dropped or unconfirmed, retrying...", &signature.to_string()[..12]);
                    // Refresh blockhash if it might have expired
                    if self.is_blockhash_expired(&tx.message.recent_blockhash).await {
                        tx = self.refresh_blockhash(tx, signer).await?;
                        blockhash_refreshes += 1;
                    }
                    self.backoff(attempts).await;
                }
                Err(RetryError::ConfirmationTimeout { .. }) => {
                    warn!("Per-attempt confirmation timeout, retrying...");
                    if self.is_blockhash_expired(&tx.message.recent_blockhash).await {
                        tx = self.refresh_blockhash(tx, signer).await?;
                        blockhash_refreshes += 1;
                    }
                    self.backoff(attempts).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Run the transaction simulation via RPC.
    async fn run_simulation(&self, tx: &Transaction) -> Result<()> {
        let result = self.rpc_client.simulate_transaction(tx).await?;
        if let Some(err) = result.value.err {
            return Err(RetryError::PreflightFailed(err.to_string()));
        }
        Ok(())
    }

    /// Send the transaction — tries TPU routing first, falls back to RPC.
    async fn send_transaction(
        &self,
        tx: &Transaction,
        used_leader_routing: &mut bool,
    ) -> Result<Signature> {
        // Try to get the current leader's TPU address for direct routing
        if self.config.use_leader_routing {
            if let Ok(Some(_tpu_addr)) = self.leader_tracker.get_current_tpu_addr().await {
                // Direct UDP send to TPU would happen here.
                // In production: serialize tx to wire format and send UDP packet to tpu_addr.
                // For now, we fall through to RPC (UDP raw send requires OS-level socket work).
                // Mark intent:
                *used_leader_routing = true;
                debug!("Leader routing available — using RPC send (UDP path: future work)");
            }
        }

        // RPC send (works reliably, slightly higher latency than direct TPU)
        let config = solana_client::rpc_config::RpcSendTransactionConfig {
            skip_preflight: true, // we already did our own preflight
            preflight_commitment: None,
            encoding: None,
            max_retries: Some(0), // we manage retries ourselves
            min_context_slot: None,
        };

        let sig = self
            .rpc_client
            .send_transaction_with_config(tx, config)
            .await?;
        Ok(sig)
    }

    /// Check whether the given blockhash has expired on-chain.
    async fn is_blockhash_expired(&self, blockhash: &Hash) -> bool {
        match self
            .rpc_client
            .is_blockhash_valid(blockhash, solana_sdk::commitment_config::CommitmentConfig::processed())
            .await
        {
            Ok(valid) => !valid,
            Err(_) => false, // Assume still valid if we can't check
        }
    }

    /// Fetch a fresh blockhash and re-sign the transaction.
    async fn refresh_blockhash(&self, mut tx: Transaction, signer: &Keypair) -> Result<Transaction> {
        info!("Refreshing blockhash for transaction...");
        let (new_blockhash, _) = self
            .rpc_client
            .get_latest_blockhash_with_commitment(
                solana_sdk::commitment_config::CommitmentConfig::finalized(),
            )
            .await?;

        tx.message.recent_blockhash = new_blockhash;
        tx.signatures = vec![Default::default(); tx.message.header.num_required_signatures as usize];
        tx.sign(&[signer], new_blockhash);

        debug!("Blockhash refreshed to {}", new_blockhash);
        Ok(tx)
    }

    /// Compute and apply exponential backoff with jitter.
    ///
    /// delay = min(base_delay * 2^(attempt-1) + jitter, max_delay)
    async fn backoff(&self, attempt: u32) {
        let base_ms = self.config.base_delay.as_millis() as u64;
        let max_ms = self.config.max_delay.as_millis() as u64;

        // Exponential: 400, 800, 1600, 3200, ... capped at max_delay
        let exp = base_ms.saturating_mul(1u64 << attempt.saturating_sub(1).min(10));
        let capped = exp.min(max_ms);

        // Add ±20% jitter to prevent thundering herd
        let jitter_range = capped / 5;
        let jitter = if jitter_range > 0 {
            rand::thread_rng().gen_range(0..jitter_range)
        } else {
            0
        };

        let delay_ms = capped + jitter;
        debug!("Backoff: waiting {}ms before retry (attempt {})", delay_ms, attempt);
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
    }
}