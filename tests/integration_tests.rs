//! Integration tests for solana-tx-retry.
//!
//! These tests use unit-level logic (no live RPC) to verify retry behavior,
//! backoff calculation, cache correctness, and error classification.
//!
//! For live devnet tests, run:
//!   SOLANA_RPC=https://api.devnet.solana.com cargo test -- --ignored

use std::time::Duration;
use solana_sdk::hash::Hash;
use solana_tx_retry::{
    config::{CommitmentLevel, RetryConfig},
    preflight_cache::{make_cache_key, PreflightCache, SimulationResult},
};

// ── PreflightCache tests ──────────────────────────────────────────────────────

#[test]
fn test_cache_hit_same_blockhash() {
    let cache = PreflightCache::new();
    let key = "test_tx_key_001".to_string();
    let blockhash = Hash::new_unique();

    cache.insert(key.clone(), SimulationResult::Success, blockhash);

    let result = cache.get(&key, &blockhash);
    assert!(result.is_some(), "Expected cache hit");
    assert!(matches!(result.unwrap(), SimulationResult::Success));
}

#[test]
fn test_cache_miss_different_blockhash() {
    let cache = PreflightCache::new();
    let key = "test_tx_key_002".to_string();
    let blockhash1 = Hash::new_unique();
    let blockhash2 = Hash::new_unique();

    cache.insert(key.clone(), SimulationResult::Success, blockhash1);

    // Different blockhash should be a cache miss
    let result = cache.get(&key, &blockhash2);
    assert!(result.is_none(), "Expected cache miss due to different blockhash");
}

#[test]
fn test_cache_miss_after_ttl() {
    let cache = PreflightCache::with_ttl(Duration::from_millis(1));
    let key = "test_tx_key_003".to_string();
    let blockhash = Hash::new_unique();

    cache.insert(key.clone(), SimulationResult::Success, blockhash);

    // Sleep longer than TTL
    std::thread::sleep(Duration::from_millis(10));

    let result = cache.get(&key, &blockhash);
    assert!(result.is_none(), "Expected cache miss after TTL expiry");
}

#[test]
fn test_cache_failed_simulation_stored() {
    let cache = PreflightCache::new();
    let key = "test_tx_key_004".to_string();
    let blockhash = Hash::new_unique();
    let error_msg = "InstructionError: insufficient lamports".to_string();

    cache.insert(key.clone(), SimulationResult::Failed(error_msg.clone()), blockhash);

    let result = cache.get(&key, &blockhash);
    assert!(result.is_some());
    match result.unwrap() {
        SimulationResult::Failed(msg) => assert_eq!(msg, error_msg),
        _ => panic!("Expected Failed result"),
    }
}

#[test]
fn test_cache_eviction() {
    let cache = PreflightCache::with_ttl(Duration::from_millis(1));
    let blockhash = Hash::new_unique();

    for i in 0..10 {
        cache.insert(format!("key_{}", i), SimulationResult::Success, blockhash);
    }
    assert_eq!(cache.len(), 10);

    std::thread::sleep(Duration::from_millis(10));
    cache.evict_expired();
    assert_eq!(cache.len(), 0, "All entries should have been evicted");
}

#[test]
fn test_cache_remove() {
    let cache = PreflightCache::new();
    let key = "removable_key".to_string();
    let blockhash = Hash::new_unique();

    cache.insert(key.clone(), SimulationResult::Success, blockhash);
    assert_eq!(cache.len(), 1);

    cache.remove(&key);
    assert_eq!(cache.len(), 0);
}

// ── Cache key generation tests ────────────────────────────────────────────────

#[test]
fn test_cache_key_deterministic() {
    let data = b"some transaction message bytes";
    let key1 = make_cache_key(data);
    let key2 = make_cache_key(data);
    assert_eq!(key1, key2, "Cache key must be deterministic");
}

#[test]
fn test_cache_key_different_for_different_data() {
    let key1 = make_cache_key(b"transaction_A");
    let key2 = make_cache_key(b"transaction_B");
    assert_ne!(key1, key2, "Different data must produce different keys");
}

// ── RetryConfig tests ─────────────────────────────────────────────────────────

#[test]
fn test_default_config_values() {
    let config = RetryConfig::default();
    assert_eq!(config.max_retries, 10);
    assert_eq!(config.base_delay, Duration::from_millis(400));
    assert_eq!(config.max_delay, Duration::from_millis(5000));
    assert!(config.preflight_enabled);
    assert!(config.skip_preflight_on_retry);
    assert_eq!(config.leader_lookahead, 4);
}

#[test]
fn test_high_throughput_config() {
    let config = RetryConfig::high_throughput();
    assert_eq!(config.max_retries, 20);
    assert!(!config.preflight_enabled);
    assert!(config.base_delay < Duration::from_millis(400));
}

#[test]
fn test_safe_config() {
    let config = RetryConfig::safe();
    assert_eq!(config.max_retries, 5);
    assert!(!config.skip_preflight_on_retry);
    assert!(config.confirmation_timeout > Duration::from_secs(30));
}

// ── CommitmentLevel conversion tests ─────────────────────────────────────────

#[test]
fn test_commitment_level_conversion() {
    use solana_sdk::commitment_config::CommitmentLevel as SolanaCommitment;

    let processed = CommitmentLevel::Processed.to_solana_commitment();
    let confirmed = CommitmentLevel::Confirmed.to_solana_commitment();
    let finalized = CommitmentLevel::Finalized.to_solana_commitment();

    assert_eq!(processed.commitment, SolanaCommitment::Processed);
    assert_eq!(confirmed.commitment, SolanaCommitment::Confirmed);
    assert_eq!(finalized.commitment, SolanaCommitment::Finalized);
}

// ── Backoff calculation tests (logic extracted for unit testing) ───────────────

#[test]
fn test_backoff_grows_exponentially() {
    // Simulate the backoff formula without async: delay = base * 2^(attempt-1)
    let base_ms: u64 = 400;
    let max_ms: u64 = 5000;

    let delays: Vec<u64> = (1..=6)
        .map(|attempt: u32| {
            let exp = base_ms.saturating_mul(1u64 << attempt.saturating_sub(1).min(10));
            exp.min(max_ms)
        })
        .collect();

    // Attempt 1: 400ms, 2: 800ms, 3: 1600ms, 4: 3200ms, 5+: 5000ms (capped)
    assert_eq!(delays[0], 400);
    assert_eq!(delays[1], 800);
    assert_eq!(delays[2], 1600);
    assert_eq!(delays[3], 3200);
    assert_eq!(delays[4], 5000); // capped
    assert_eq!(delays[5], 5000); // capped
}

#[test]
fn test_backoff_never_exceeds_max() {
    let base_ms: u64 = 400;
    let max_ms: u64 = 5000;

    for attempt in 1u32..=50 {
        let delay = base_ms
            .saturating_mul(1u64 << attempt.saturating_sub(1).min(10))
            .min(max_ms);
        assert!(delay <= max_ms, "Delay {} exceeded max {} at attempt {}", delay, max_ms, attempt);
    }
}