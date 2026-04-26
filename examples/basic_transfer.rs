//! Example: Send a SOL transfer with automatic retry.
//!
//! Run against devnet:
//!   cargo run --example basic_transfer

use std::{path::PathBuf, sync::Arc};

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    signature::{read_keypair_file, Keypair},
    signer::Signer,
    system_instruction,
    transaction::Transaction,
};
use solana_tx_retry::{RetryConfig, RetryEngine};

const DEVNET_RPC: &str = "https://api.devnet.solana.com";

/// Load the default Solana CLI keypair (~/.config/solana/id.json),
/// or fall back to a freshly generated one.
fn load_payer() -> Keypair {
    let default_path = PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".config/solana/id.json");

    if let Ok(kp) = read_keypair_file(&default_path) {
        eprintln!("Using keypair from {}", default_path.display());
        kp
    } else {
        eprintln!("No keypair found at ~/.config/solana/id.json — generating ephemeral keypair");
        Keypair::new()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize structured logging
    tracing_subscriber::fmt()
        .with_env_filter("solana_tx_retry=debug,basic_transfer=info")
        .init();

    println!("=== solana-tx-retry: Basic Transfer Example ===\n");

    // ── 1. Setup RPC client ───────────────────────────────────────────────────
    let rpc = Arc::new(RpcClient::new_with_commitment(
        DEVNET_RPC.to_string(),
        CommitmentConfig::confirmed(),
    ));

    // ── 2. Create the retry engine ────────────────────────────────────────────
    let config = RetryConfig {
        max_retries: 10,
        ..RetryConfig::default()
    };

    let engine = RetryEngine::new(Arc::clone(&rpc), config);
    engine.initialize().await?;
    println!("✓ RetryEngine initialized (leader schedule fetched)\n");

    // ── 3. Load payer keypair (uses ~/.config/solana/id.json if available) ───
    let payer = load_payer();
    println!("Payer: {}", payer.pubkey());

    let balance = rpc.get_balance(&payer.pubkey()).await?;
    println!("Balance: {} lamports ({:.4} SOL)\n", balance, balance as f64 / 1e9);

    if balance < 10_000_000 {
        anyhow::bail!("Insufficient balance — run `solana airdrop 1 --url devnet` first");
    }

    // ── 4. Build the transfer transaction ────────────────────────────────────
    let recipient = Keypair::new().pubkey();
    let transfer_amount = 10_000_000; // 0.01 SOL in lamports

    println!("Sending {} lamports to {}...", transfer_amount, recipient);

    let blockhash = rpc.get_latest_blockhash().await?;
    let ix = system_instruction::transfer(&payer.pubkey(), &recipient, transfer_amount);
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[&payer],
        blockhash,
    );

    // ── 5. Send with retry ────────────────────────────────────────────────────
    println!("Submitting transaction with retry engine...");
    let stats = engine.send_with_retry(tx, &payer).await?;

    // ── 6. Print results ──────────────────────────────────────────────────────
    println!("\n=== Result ===");
    println!("✓ Signature:          {}", stats.signature);
    println!("  Attempts:           {}", stats.attempts);
    println!("  Blockhash refreshes:{}", stats.blockhash_refreshes);
    println!("  Leader routing:     {}", stats.used_leader_routing);
    println!(
        "\nView on explorer: https://explorer.solana.com/tx/{}?cluster=devnet",
        stats.signature
    );

    Ok(())
}