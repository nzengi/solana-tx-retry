//! Transaction status monitoring.
//!
//! Polls the RPC for transaction confirmation status and classifies outcomes
//! into a simple state machine: Pending → Confirmed / Finalized / Failed / Dropped.

use std::{sync::Arc, time::{Duration, Instant}};

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{commitment_config::CommitmentConfig, signature::Signature};
use solana_transaction_status_client_types::TransactionConfirmationStatus;
use tracing::{debug, info, warn};

use crate::{
    config::CommitmentLevel,
    error::{Result, RetryError},
};

/// The lifecycle state of a submitted transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxStatus {
    /// Transaction has been sent but not yet seen on-chain.
    Pending,
    /// Transaction has been processed by the leader (not yet confirmed by supermajority).
    Processed,
    /// Transaction confirmed by supermajority — safe for most use cases.
    Confirmed,
    /// Transaction finalized — irreversible.
    Finalized,
    /// Transaction was rejected on-chain (e.g. program error, insufficient funds).
    Failed { reason: String },
    /// Transaction was never seen — blockhash expired or packet dropped.
    Dropped,
}

/// Monitors a submitted transaction until it reaches a terminal state
/// or the timeout is exceeded.
pub struct TxMonitor {
    rpc_client: Arc<RpcClient>,
    /// How often to poll for status.
    poll_interval: Duration,
}

impl TxMonitor {
    pub fn new(rpc_client: Arc<RpcClient>) -> Self {
        Self {
            rpc_client,
            poll_interval: Duration::from_millis(500), // poll every ~1 slot
        }
    }

    /// Wait for a transaction to reach the desired commitment level or timeout.
    ///
    /// Returns `Ok(TxStatus)` when the transaction reaches a terminal state.
    /// Returns `Err(RetryError::ConfirmationTimeout)` if the timeout is exceeded.
    pub async fn wait_for_confirmation(
        &self,
        signature: &Signature,
        desired_commitment: CommitmentLevel,
        timeout: Duration,
    ) -> Result<TxStatus> {
        let started = Instant::now();
        let sig_str = signature.to_string();
        let commitment_config = desired_commitment.to_solana_commitment();

        info!("Monitoring tx {} for {:?} confirmation", &sig_str[..12], desired_commitment);

        loop {
            if started.elapsed() >= timeout {
                warn!("Confirmation timeout for tx {}", &sig_str[..12]);
                return Err(RetryError::ConfirmationTimeout {
                    timeout_secs: timeout.as_secs(),
                    signature: sig_str,
                });
            }

            match self.check_status(signature, commitment_config).await {
                Ok(status) => {
                    debug!("Tx {} status: {:?}", &sig_str[..12], status);
                    match &status {
                        TxStatus::Pending | TxStatus::Processed => {
                            // Keep waiting
                        }
                        TxStatus::Confirmed | TxStatus::Finalized => {
                            info!("Tx {} confirmed ✓", &sig_str[..12]);
                            return Ok(status);
                        }
                        TxStatus::Failed { reason } => {
                            warn!("Tx {} failed on-chain: {}", &sig_str[..12], reason);
                            return Ok(status);
                        }
                        TxStatus::Dropped => {
                            debug!("Tx {} appears dropped, will retry", &sig_str[..12]);
                            return Ok(status);
                        }
                    }
                }
                Err(e) => {
                    warn!("Error checking tx status: {}. Will retry poll.", e);
                }
            }

            tokio::time::sleep(self.poll_interval).await;
        }
    }

    /// Do a single status check for the given signature.
    pub async fn check_status(
        &self,
        signature: &Signature,
        _commitment: CommitmentConfig,
    ) -> Result<TxStatus> {
        let statuses = self
            .rpc_client
            .get_signature_statuses(&[*signature])
            .await?;

        let status_opt = statuses.value.into_iter().next().flatten();

        let Some(status) = status_opt else {
            return Ok(TxStatus::Pending);
        };

        // Check if there was an on-chain error
        if let Some(err) = status.err {
            return Ok(TxStatus::Failed {
                reason: err.to_string(),
            });
        }

        // Map confirmation status enum
        match status.confirmation_status {
            Some(TransactionConfirmationStatus::Processed) => Ok(TxStatus::Processed),
            Some(TransactionConfirmationStatus::Confirmed) => Ok(TxStatus::Confirmed),
            Some(TransactionConfirmationStatus::Finalized) => Ok(TxStatus::Finalized),
            None => {
                // Has confirmations but no status field — treat as confirmed if count > 0
                if status.confirmations.map_or(false, |c| c > 0) {
                    Ok(TxStatus::Confirmed)
                } else {
                    Ok(TxStatus::Pending)
                }
            }
        }
    }

    /// Quick non-blocking check: is the transaction already confirmed?
    /// Useful to avoid re-sending a transaction that already landed.
    pub async fn is_confirmed(&self, signature: &Signature) -> bool {
        let commitment = CommitmentConfig::confirmed();
        match self.check_status(signature, commitment).await {
            Ok(TxStatus::Confirmed) | Ok(TxStatus::Finalized) => true,
            _ => false,
        }
    }
}