# solana-tx-retry

Solana transaction delivery is unreliable by default. `sendAndConfirmTransaction` submits once and polls — if the transaction gets dropped between submission and confirmation, you find out only after the blockhash expires 60 seconds later. This library fixes that.

## Background

Solana's mempool is effectively zero-size. A transaction either lands with the current leader or it doesn't — there's no waiting room. The RPC node you submit to forwards to the leader via gossip, not directly to the TPU port. Under any real load, that path is lossy.

The three failure modes that bite production apps most often:

**Blockhash expiry.** Every transaction commits to a recent blockhash, which is only valid for ~150 slots (~60s). If confirmation takes longer than that — whether because of congestion or a slow leader — the transaction becomes permanently invalid. You have to re-sign with a fresh blockhash and resubmit.

**Leader rotation.** Leaders rotate every 4 slots (~1.6s). Your RPC node may forward to the wrong validator, adding a gossip hop and burning your narrow window. If you know the leader schedule, you can route directly to the TPU port of the next leader.

**Thundering herd on retry.** When network load spikes, every client detects dropped transactions at roughly the same time and retries simultaneously. Without jitter, this produces another congestion spike. Exponential backoff with randomized jitter spreads the load.

## How it works

The retry loop is straightforward:

1. Run preflight simulation. Cache the result keyed on the transaction message bytes (not the blockhash, since re-signed transactions are functionally identical). If the simulation returned a program error, fail immediately — no network conditions will fix a bad instruction.

2. Submit with `skip_preflight=true` and `max_retries=0`. We own the retry cycle; letting the RPC also retry causes double-exponential delays and makes attempt counting unreliable.

3. Poll for confirmation. On `Confirmed` or `Finalized`, return stats. On a terminal on-chain error (insufficient funds, bad signature, program error), return the error without retrying.

4. On timeout or drop, check whether the blockhash has expired. If it has, fetch a new one, re-sign, and go back to step 1 — this does not count against the retry budget. Otherwise apply backoff (`min(400ms × 2^n, 5s) ± 20% jitter`) and retry.

```
send_with_retry(tx, signer)
│
├── preflight (cached)
│     ├── hit+ok   → skip simulation
│     └── hit+err  → return PreflightFailed
│
├── sendRawTransaction(skip_preflight=true, max_retries=0)
│
├── poll status
│     ├── Confirmed   → done ✓
│     ├── Rejected    → done ✗  (terminal, no retry)
│     └── Dropped     → step 4
│
└── backoff / blockhash refresh
      ├── expired? → re-sign, free retry
      └── delay = min(400 × 2^n, 5000)ms ± 20%  → loop
```

## Usage

### Rust

```toml
[dependencies]
solana-tx-retry = "0.1"
```

```rust
use std::sync::Arc;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_tx_retry::{RetryEngine, RetryConfig};

let rpc = Arc::new(RpcClient::new("https://api.devnet.solana.com".to_string()));
let engine = RetryEngine::new(rpc.clone(), RetryConfig::default());
engine.initialize().await?;  // fetches leader schedule (~432k slots)

let stats = engine.send_with_retry(tx, &payer).await?;
println!("confirmed: {} ({} attempt(s))", stats.signature, stats.attempts);
```

**Presets:**

```rust
// Default: 10 retries, 400ms base, preflight on, leader routing on
RetryConfig::default()

// High throughput: 20 retries, 200ms base, preflight off
RetryConfig::high_throughput()

// Safe: 5 retries, always simulate, 60s hard timeout
RetryConfig::safe()

// Custom:
RetryConfig {
    max_retries: 15,
    base_delay: Duration::from_millis(300),
    use_leader_routing: true,
    ..RetryConfig::default()
}
```

### TypeScript

```bash
npm install solana-tx-retry @solana/web3.js
```

```typescript
import { sendWithRetry, HIGH_THROUGHPUT_CONFIG } from "solana-tx-retry";

const stats = await sendWithRetry(connection, tx, [payer]);
// or with a preset:
const stats = await sendWithRetry(connection, tx, [payer], HIGH_THROUGHPUT_CONFIG);

console.log(`${stats.signature} (${stats.attempts} attempt(s), ${stats.elapsedMs}ms)`);
```

**Errors:**

```typescript
import { TransactionRejectedError, MaxRetriesExceededError } from "solana-tx-retry";

try {
    await sendWithRetry(connection, tx, [payer]);
} catch (e) {
    if (e instanceof TransactionRejectedError) {
        // on-chain error — retrying won't help
    } else if (e instanceof MaxRetriesExceededError) {
        // network gave up after e.attempts tries
    }
}
```

## Design notes

**DashMap for the preflight cache.** A plain `Mutex<HashMap>` creates a bottleneck when concurrent calls hit the cache simultaneously. DashMap uses per-shard locking and has an identical API — it's a drop-in that removes the contention.

**`max_retries=0` on submission.** If the RPC client retries internally and we also retry externally, the actual attempt count can be 10× what `stats.attempts` reports. Setting it to zero means we have precise control over timing and counting.

**Leader schedule.** The full epoch schedule (~432k slot → pubkey entries) is fetched once on `initialize()` and refreshed at epoch boundaries. At runtime, looking up the next 4 leaders is a map lookup, not an RPC call. The cost is about 3MB of heap for the schedule; cheaper than an extra RPC round-trip on every send.

**Jitter bounds.** The ±20% range was chosen to spread a cohort of 100 retrying clients across a 200–800ms window without making individual delays unpredictable. Tighter bounds don't help with herd behavior; wider bounds make the p99 latency too high.

## Running locally

```bash
# run unit tests (no network)
cargo test

# run devnet example (requires funded keypair or will airdrop)
cargo run --example basic_transfer

# typescript build
cd npm && npm install && npm run build
```

## License

MIT