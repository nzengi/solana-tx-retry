//! # solana-tx-retry
//!
//! A production-grade transaction retry library for Solana with:
//! - **Leader-aware routing**: tracks the validator rotation schedule and routes
//!   transactions to the upcoming block producer for minimal latency.
//! - **Adaptive backoff**: exponential backoff with jitter, aligned to Solana's
//!   400ms slot duration to avoid thundering-herd effects.
//! - **Automatic blockhash refresh**: detects expired blockhashes and re-signs
//!   the transaction with a fresh one, seamlessly continuing retry attempts.
//! - **Preflight simulation cache**: avoids redundant RPC simulation calls on
//!   retry, reducing overhead while still catching invalid transactions early.
//! - **Transaction status monitoring**: polls for confirmation and classifies
//!   outcomes (confirmed, failed, dropped) to drive the retry decision.
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use std::sync::Arc;
//! use solana_client::nonblocking::rpc_client::RpcClient;
//! use solana_sdk::{
//!     signature::Keypair,
//!     signer::Signer,
//!     system_instruction,
//!     transaction::Transaction,
//! };
//! use solana_tx_retry::{RetryEngine, RetryConfig};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let rpc = Arc::new(RpcClient::new("https://api.devnet.solana.com".to_string()));
//!     let engine = RetryEngine::new(rpc.clone(), RetryConfig::default());
//!     engine.initialize().await?;
//!
//!     let payer = Keypair::new();
//!     let recipient = Keypair::new().pubkey();
//!     let blockhash = rpc.get_latest_blockhash().await?;
//!
//!     let ix = system_instruction::transfer(&payer.pubkey(), &recipient, 1_000_000);
//!     let tx = Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[&payer], blockhash);
//!
//!     let stats = engine.send_with_retry(tx, &payer).await?;
//!     println!("Confirmed in {} attempt(s): {}", stats.attempts, stats.signature);
//!     Ok(())
//! }
//! ```

pub mod config;
pub mod error;
pub mod leader_tracker;
pub mod preflight_cache;
pub mod retry_engine;
pub mod tx_monitor;

// Re-export the primary public API surface
pub use config::{CommitmentLevel, RetryConfig};
pub use error::{Result, RetryError};
pub use retry_engine::{RetryEngine, RetryStats};
pub use tx_monitor::TxStatus;