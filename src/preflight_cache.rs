//! Preflight simulation cache.
//!
//! Caches the result of transaction simulations to avoid redundant RPC calls
//! on retry attempts. A simulation result is valid as long as the blockhash
//! it was simulated against has not expired (~150 slots / ~60 seconds).

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use dashmap::DashMap;
use solana_sdk::hash::Hash;
use tracing::debug;

/// Result of a preflight simulation.
#[derive(Debug, Clone)]
pub enum SimulationResult {
    /// Simulation succeeded — transaction is safe to send.
    Success,
    /// Simulation failed with an error message — do NOT send.
    Failed(String),
}

/// A cached simulation entry.
#[derive(Debug)]
struct CacheEntry {
    result: SimulationResult,
    blockhash: Hash,
    cached_at: Instant,
}

/// Cache for preflight simulation results.
///
/// Uses `DashMap` for concurrent access without global locking.
/// Entries expire after `ttl` (default: 60 seconds, roughly 150 slots).
pub struct PreflightCache {
    /// tx_hash (serialized bytes hash) -> cache entry
    entries: Arc<DashMap<String, CacheEntry>>,
    /// Time-to-live for cache entries
    ttl: Duration,
}

impl PreflightCache {
    /// Create a new preflight cache with the default TTL (60 seconds).
    pub fn new() -> Self {
        Self {
            entries: Arc::new(DashMap::new()),
            ttl: Duration::from_secs(60),
        }
    }

    /// Create a cache with a custom TTL.
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            entries: Arc::new(DashMap::new()),
            ttl,
        }
    }

    /// Look up a cached simulation result for the given transaction key and blockhash.
    ///
    /// Returns `None` if no valid cache entry exists (expired, wrong blockhash, or missing).
    pub fn get(&self, tx_key: &str, blockhash: &Hash) -> Option<SimulationResult> {
        let entry = self.entries.get(tx_key)?;

        // Invalidate if blockhash changed (transaction was re-signed with new blockhash)
        if entry.blockhash != *blockhash {
            debug!("Cache miss: blockhash changed for tx {}", &tx_key[..8]);
            return None;
        }

        // Invalidate if TTL expired
        if entry.cached_at.elapsed() > self.ttl {
            debug!("Cache miss: entry expired for tx {}", &tx_key[..8]);
            return None;
        }

        debug!("Cache hit for tx {}", &tx_key[..8]);
        Some(entry.result.clone())
    }

    /// Store a simulation result in the cache.
    pub fn insert(&self, tx_key: String, result: SimulationResult, blockhash: Hash) {
        self.entries.insert(
            tx_key,
            CacheEntry {
                result,
                blockhash,
                cached_at: Instant::now(),
            },
        );
    }

    /// Remove a specific entry (e.g. when a transaction is confirmed).
    pub fn remove(&self, tx_key: &str) {
        self.entries.remove(tx_key);
    }

    /// Evict all expired entries. Call this periodically to prevent unbounded growth.
    pub fn evict_expired(&self) {
        let ttl = self.ttl;
        self.entries.retain(|_, v| v.cached_at.elapsed() <= ttl);
        debug!("Preflight cache eviction complete. Remaining: {}", self.entries.len());
    }

    /// Current number of cached entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Default for PreflightCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Generate a cache key from a transaction's message bytes.
/// We hash the serialized transaction message (excluding signatures)
/// so that re-signed transactions with the same instructions produce the same key.
pub fn make_cache_key(tx_message_bytes: &[u8]) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    tx_message_bytes.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}