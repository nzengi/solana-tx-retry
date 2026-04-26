//! Error types for the solana-tx-retry library.

use thiserror::Error;

/// All errors that can occur during transaction sending and retrying.
#[derive(Debug, Error)]
pub enum RetryError {
    /// The transaction was not confirmed after all retry attempts.
    #[error("Transaction dropped after {attempts} attempts: {signature}")]
    MaxRetriesExceeded {
        attempts: u32,
        signature: String,
    },

    /// The transaction confirmation timeout was reached.
    #[error("Confirmation timeout reached after {timeout_secs}s for signature {signature}")]
    ConfirmationTimeout {
        timeout_secs: u64,
        signature: String,
    },

    /// The transaction blockhash expired during retry.
    #[error("Blockhash expired, could not refresh in time")]
    BlockhashExpired,

    /// Failed to fetch the leader schedule from the RPC.
    #[error("Failed to fetch leader schedule: {0}")]
    LeaderScheduleFetchFailed(String),

    /// The transaction simulation (preflight) failed.
    #[error("Preflight simulation failed: {0}")]
    PreflightFailed(String),

    /// A Solana RPC client error.
    #[error("RPC client error: {0}")]
    RpcError(#[from] solana_client::client_error::ClientError),

    /// The transaction was explicitly rejected (e.g. insufficient funds, bad signature).
    #[error("Transaction rejected on-chain: {reason}")]
    TransactionRejected {
        reason: String,
        signature: String,
    },

    /// An internal logic error.
    #[error("Internal error: {0}")]
    Internal(String),
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, RetryError>;