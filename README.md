# solana-tx-retry

A production-grade transaction retry library for Solana that dramatically improves transaction success rates by handling the most common failure modes automatically.

## Why this exists

Solana's default `sendAndConfirmTransaction` drops transactions silently in several scenarios:

- **Leader rotation** — the validator that should process your transaction is replaced before it does so
- **Network congestion** — UDP packets to the leader's TPU port are dropped under load
- **Blockhash expiry** — transactions are only valid for ~150 slots (~60 seconds); long confirmation waits cause expiry

This library handles all three by combining leader schedule tracking, adaptive backoff, automatic blockhash refresh, and preflight simulation caching.

## Architecture

```
solana-tx-retry/
├── src/
│   ├── lib.rs              # Public API exports
│   ├── config.rs           # RetryConfig, CommitmentLevel
│   ├── error.rs            # RetryError enum
│   ├── leader_tracker.rs   # Leader schedule + TPU address cache
│   ├── retry_engine.rs     # Core retry loop with backoff
│   ├── tx_monitor.rs       # Transaction status polling
│   └── preflight_cache.rs  # Simulation result cache (DashMap)
├── examples/
│   └── basic_transfer.rs   # Working devnet example
├── tests/
│   └── integration_tests.rs
└── npm/                    # TypeScript wrapper
    ├── src/index.ts
    ├── package.json
    └── tsconfig.json
```

## How the retry loop works

```
send_with_retry(tx, signer)
│
├── [1] Preflight simulation (with cache)
│       ├── Cache hit (Success)  → skip RPC call
│       ├── Cache hit (Failed)   → return PreflightFailed immediately
│       └── Cache miss           → simulate, store result
│
├── [2] Send transaction
│       ├── Leader routing check (get upcoming TPU addresses)
│       └── RPC send with skip_preflight=true, max_retries=0
│
├── [3] Monitor for confirmation (TxMonitor)
│       ├── Confirmed / Finalized → return RetryStats ✓
│       ├── Failed                → return TransactionRejected (no retry)
│       └── Dropped / Timeout     → go to [4]
│
└── [4] Retry decision
        ├── Blockhash expired?    → refresh + re-sign (free retry)
        └── Apply backoff delay   → back to [1]
                delay = min(400ms × 2^attempt, 5000ms) + jitter(±20%)
```

## Rust usage

```toml
[dependencies]
solana-tx-retry = "0.1"
```

```rust
use std::sync::Arc;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{signature::Keypair, signer::Signer, system_instruction, transaction::Transaction};
use solana_tx_retry::{RetryEngine, RetryConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let rpc = Arc::new(RpcClient::new("https://api.devnet.solana.com".to_string()));
    let engine = RetryEngine::new(rpc.clone(), RetryConfig::default());
    engine.initialize().await?;

    let payer = Keypair::new();
    let blockhash = rpc.get_latest_blockhash().await?;
    let ix = system_instruction::transfer(&payer.pubkey(), &recipient, 1_000_000);
    let tx = Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[&payer], blockhash);

    let stats = engine.send_with_retry(tx, &payer).await?;
    println!("Confirmed in {} attempt(s): {}", stats.attempts, stats.signature);
    Ok(())
}
```

### Preset configurations

```rust
// High throughput — 20 retries, no preflight, short delays
let engine = RetryEngine::new(rpc, RetryConfig::high_throughput());

// Safe — 5 retries, always simulate, 60s timeout
let engine = RetryEngine::new(rpc, RetryConfig::safe());

// Custom
let engine = RetryEngine::new(rpc, RetryConfig {
    max_retries: 15,
    base_delay: Duration::from_millis(300),
    use_leader_routing: true,
    ..RetryConfig::default()
});
```

## TypeScript usage

```bash
cd npm && npm install
```

```typescript
import { Connection, Keypair, SystemProgram, Transaction } from "@solana/web3.js";
import { sendWithRetry, HIGH_THROUGHPUT_CONFIG } from "solana-tx-retry";

const connection = new Connection("https://api.devnet.solana.com", "confirmed");
const payer = Keypair.generate();

const tx = new Transaction().add(
  SystemProgram.transfer({
    fromPubkey: payer.publicKey,
    toPubkey: recipient,
    lamports: 10_000_000,
  })
);

const stats = await sendWithRetry(connection, tx, [payer], HIGH_THROUGHPUT_CONFIG);
console.log(`Confirmed in ${stats.attempts} attempt(s): ${stats.signature}`);
console.log(`Elapsed: ${stats.elapsedMs}ms, blockhash refreshes: ${stats.blockhashRefreshes}`);
```

### Error handling

```typescript
import { sendWithRetry, MaxRetriesExceededError, TransactionRejectedError } from "solana-tx-retry";

try {
  const stats = await sendWithRetry(connection, tx, [payer]);
} catch (err) {
  if (err instanceof TransactionRejectedError) {
    // On-chain error (program error, insufficient funds, etc.) — do NOT retry
    console.error("Transaction rejected:", err.reason);
  } else if (err instanceof MaxRetriesExceededError) {
    // Network failure after all retries exhausted
    console.error(`Gave up after ${err.attempts} attempts`);
  }
}
```

## Running the example

```bash
# Against devnet (requires Solana CLI installed for airdrop)
cargo run --example basic_transfer
```

## Running tests

```bash
# Rust unit tests (no network required)
cargo test

# TypeScript build check
cd npm && npm run build
```

## Key design decisions

**Why DashMap for the preflight cache?** Concurrent retry attempts for different transactions would contend on a `Mutex<HashMap>`. DashMap provides per-shard locking at zero API cost.

**Why exponential backoff with jitter?** Without jitter, all retrying clients would wake up at the same time after a congestion event, causing another wave of congestion. The ±20% jitter spreads them out.

**Why `max_retries=0` on the RPC send call?** We own the retry logic. Letting the RPC client also retry leads to double-exponential wait times and makes the `attempts` counter unreliable.

**Why track the leader schedule?** Solana assigns each validator a 4-slot window (~1.6s) as the leader. Sending to the wrong validator requires a gossip hop, adding ~200-400ms. Knowing who is next lets us route directly to their TPU port.

## License

MIT