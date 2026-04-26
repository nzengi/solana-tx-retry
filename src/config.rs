//! Configuration structures for the retry engine.

use std::time::Duration;

/// Main configuration for transaction retry behavior.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retry attempts before giving up.
    /// Default: 10
    pub max_retries: u32,

    /// Base delay between retries. Aligned to Solana's slot duration (400ms).
    /// Default: 400ms
    pub base_delay: Duration,

    /// Maximum delay cap for exponential backoff.
    /// Default: 5000ms
    pub max_delay: Duration,

    /// Whether to run preflight simulation before sending.
    /// Default: true
    pub preflight_enabled: bool,

    /// Skip preflight simulation on retry attempts (saves RPC calls).
    /// Default: true
    pub skip_preflight_on_retry: bool,

    /// How long to wait for transaction confirmation before declaring it dropped.
    /// Default: 30s
    pub confirmation_timeout: Duration,

    /// Commitment level required to consider a transaction confirmed.
    /// Default: Confirmed
    pub commitment: CommitmentLevel,

    /// How many upcoming leaders to fetch for routing decisions.
    /// Default: 4
    pub leader_lookahead: usize,

    /// Whether to use leader-aware routing (send directly to TPU).
    /// Default: true
    pub use_leader_routing: bool,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 10,
            base_delay: Duration::from_millis(400), // 1 Solana slot
            max_delay: Duration::from_millis(5000),
            preflight_enabled: true,
            skip_preflight_on_retry: true,
            confirmation_timeout: Duration::from_secs(30),
            commitment: CommitmentLevel::Confirmed,
            leader_lookahead: 4,
            use_leader_routing: true,
        }
    }
}

impl RetryConfig {
    /// Create a config optimized for high-throughput scenarios (less waiting, more retries).
    pub fn high_throughput() -> Self {
        Self {
            max_retries: 20,
            base_delay: Duration::from_millis(200),
            max_delay: Duration::from_millis(2000),
            skip_preflight_on_retry: true,
            preflight_enabled: false, // Skip preflight entirely for speed
            ..Default::default()
        }
    }

    /// Create a config optimized for safety (more validation, longer waits).
    pub fn safe() -> Self {
        Self {
            max_retries: 5,
            base_delay: Duration::from_millis(800),
            max_delay: Duration::from_millis(10000),
            preflight_enabled: true,
            skip_preflight_on_retry: false,
            confirmation_timeout: Duration::from_secs(60),
            ..Default::default()
        }
    }
}

/// Commitment level for transaction confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitmentLevel {
    /// Transaction is processed by the current leader. Lowest latency, least safe.
    Processed,
    /// Transaction is confirmed by a supermajority of validators. Recommended.
    Confirmed,
    /// Transaction is finalized and cannot be rolled back. Highest safety.
    Finalized,
}

impl CommitmentLevel {
    /// Convert to Solana SDK CommitmentConfig.
    pub fn to_solana_commitment(&self) -> solana_sdk::commitment_config::CommitmentConfig {
        match self {
            CommitmentLevel::Processed => {
                solana_sdk::commitment_config::CommitmentConfig::processed()
            }
            CommitmentLevel::Confirmed => {
                solana_sdk::commitment_config::CommitmentConfig::confirmed()
            }
            CommitmentLevel::Finalized => {
                solana_sdk::commitment_config::CommitmentConfig::finalized()
            }
        }
    }
}