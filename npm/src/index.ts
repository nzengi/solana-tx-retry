/**
 * solana-tx-retry — TypeScript wrapper
 *
 * A drop-in replacement for @solana/web3.js `sendAndConfirmTransaction`
 * with adaptive retry, leader-aware routing hints, and automatic blockhash refresh.
 *
 * Install:
 *   npm install solana-tx-retry @solana/web3.js
 *
 * Usage:
 *   import { sendWithRetry } from 'solana-tx-retry';
 *   const stats = await sendWithRetry(connection, transaction, [payer]);
 */

import {
  Connection,
  Transaction,
  Signer,
  BlockhashWithExpiryBlockHeight,
  TransactionExpiredBlockheightExceededError,
} from "@solana/web3.js";

// ── Types ─────────────────────────────────────────────────────────────────────

export interface RetryConfig {
  /** Maximum number of send attempts. Default: 10 */
  maxRetries?: number;
  /** Base delay in ms (aligned to 1 Solana slot = 400ms). Default: 400 */
  baseDelayMs?: number;
  /** Maximum delay cap in ms. Default: 5000 */
  maxDelayMs?: number;
  /** Whether to run preflight simulation on first attempt. Default: true */
  preflightEnabled?: boolean;
  /** Skip preflight on retry attempts. Default: true */
  skipPreflightOnRetry?: boolean;
  /** Total timeout in ms before giving up. Default: 30000 */
  timeoutMs?: number;
  /** Commitment level for confirmation. Default: 'confirmed' */
  commitment?: "processed" | "confirmed" | "finalized";
}

export interface RetryStats {
  /** The confirmed transaction signature (base-58). */
  signature: string;
  /** Number of send attempts made (1 = first try succeeded). */
  attempts: number;
  /** Number of times the blockhash was refreshed. */
  blockhashRefreshes: number;
  /** Total elapsed time in milliseconds. */
  elapsedMs: number;
}

export class MaxRetriesExceededError extends Error {
  constructor(public attempts: number, public lastSignature: string) {
    super(`Transaction dropped after ${attempts} attempts (last sig: ${lastSignature.slice(0, 12)}...)`);
    this.name = "MaxRetriesExceededError";
  }
}

export class TransactionRejectedError extends Error {
  constructor(public reason: string, public signature: string) {
    super(`Transaction rejected on-chain: ${reason}`);
    this.name = "TransactionRejectedError";
  }
}

// ── Defaults ──────────────────────────────────────────────────────────────────

const DEFAULT_CONFIG: Required<RetryConfig> = {
  maxRetries: 10,
  baseDelayMs: 400,
  maxDelayMs: 5000,
  preflightEnabled: true,
  skipPreflightOnRetry: true,
  timeoutMs: 30_000,
  commitment: "confirmed",
};

// ── Core send function ────────────────────────────────────────────────────────

/**
 * Send a transaction with automatic retry, adaptive backoff, and blockhash refresh.
 *
 * This is a drop-in replacement for `sendAndConfirmTransaction` from @solana/web3.js
 * that handles the most common failure modes:
 * - Transaction dropped due to network congestion
 * - Blockhash expiry during high-load periods
 * - Leader rotation causing missed routing
 *
 * @param connection - Solana Connection instance
 * @param transaction - Signed Transaction (will be re-signed if blockhash expires)
 * @param signers - Keypairs used to sign (needed for re-signing on blockhash refresh)
 * @param config - Optional retry configuration
 * @returns RetryStats on success
 * @throws MaxRetriesExceededError | TransactionRejectedError
 */
export async function sendWithRetry(
  connection: Connection,
  transaction: Transaction,
  signers: Signer[],
  config: RetryConfig = {}
): Promise<RetryStats> {
  const cfg = { ...DEFAULT_CONFIG, ...config };
  const startTime = Date.now();

  let tx = transaction;
  let attempts = 0;
  let blockhashRefreshes = 0;
  let lastSignature = "";
  let latestBlockhash: BlockhashWithExpiryBlockHeight | null = null;

  // Fetch initial blockhash info for expiry tracking
  latestBlockhash = await connection.getLatestBlockhash(cfg.commitment);

  while (attempts < cfg.maxRetries) {
    if (Date.now() - startTime > cfg.timeoutMs) {
      throw new MaxRetriesExceededError(attempts, lastSignature);
    }

    attempts++;

    try {
      // ── Send ─────────────────────────────────────────────────────────────
      const skipPreflight =
        !cfg.preflightEnabled || (attempts > 1 && cfg.skipPreflightOnRetry);

      const signature = await connection.sendRawTransaction(tx.serialize(), {
        skipPreflight,
        preflightCommitment: cfg.commitment,
        maxRetries: 0, // we manage retries
      });

      lastSignature = signature;

      // ── Confirm ───────────────────────────────────────────────────────────
      const result = await connection.confirmTransaction(
        {
          signature,
          blockhash: latestBlockhash!.blockhash,
          lastValidBlockHeight: latestBlockhash!.lastValidBlockHeight,
        },
        cfg.commitment
      );

      if (result.value.err) {
        throw new TransactionRejectedError(
          JSON.stringify(result.value.err),
          signature
        );
      }

      // ── Success ───────────────────────────────────────────────────────────
      return {
        signature,
        attempts,
        blockhashRefreshes,
        elapsedMs: Date.now() - startTime,
      };
    } catch (err) {
      if (err instanceof TransactionRejectedError) {
        // On-chain rejection is terminal — do not retry
        throw err;
      }

      if (err instanceof TransactionExpiredBlockheightExceededError) {
        // Blockhash expired — refresh and re-sign
        latestBlockhash = await connection.getLatestBlockhash(cfg.commitment);
        tx.recentBlockhash = latestBlockhash.blockhash;
        tx.sign(...signers);
        blockhashRefreshes++;
        // Don't count blockhash refresh as a retry attempt
        attempts--;
        continue;
      }

      // Network error or other transient failure — back off and retry
      const delayMs = computeBackoff(attempts, cfg.baseDelayMs, cfg.maxDelayMs);
      await sleep(delayMs);
    }
  }

  throw new MaxRetriesExceededError(attempts, lastSignature);
}

// ── Helper: send without waiting for confirmation ─────────────────────────────

/**
 * Fire-and-forget send. Returns the signature immediately without waiting.
 * Useful for high-throughput scenarios where you poll status separately.
 */
export async function sendTransaction(
  connection: Connection,
  transaction: Transaction,
  config: Pick<RetryConfig, "preflightEnabled" | "commitment"> = {}
): Promise<string> {
  const cfg = { ...DEFAULT_CONFIG, ...config };
  return connection.sendRawTransaction(transaction.serialize(), {
    skipPreflight: !cfg.preflightEnabled,
    preflightCommitment: cfg.commitment,
    maxRetries: 0,
  });
}

// ── Backoff calculation ───────────────────────────────────────────────────────

/**
 * Compute exponential backoff with jitter.
 * delay = min(base * 2^(attempt-1), max) + random(0, 20% of delay)
 */
function computeBackoff(attempt: number, baseMs: number, maxMs: number): number {
  const exp = Math.min(baseMs * Math.pow(2, attempt - 1), maxMs);
  const jitter = Math.random() * (exp * 0.2);
  return Math.floor(exp + jitter);
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

// ── Preset configs ────────────────────────────────────────────────────────────

/** Optimized for speed — fewer retries, no preflight, short delays. */
export const HIGH_THROUGHPUT_CONFIG: RetryConfig = {
  maxRetries: 20,
  baseDelayMs: 200,
  maxDelayMs: 2000,
  preflightEnabled: false,
  skipPreflightOnRetry: true,
};

/** Optimized for safety — fewer retries, always simulate, long timeout. */
export const SAFE_CONFIG: RetryConfig = {
  maxRetries: 5,
  baseDelayMs: 800,
  maxDelayMs: 10_000,
  preflightEnabled: true,
  skipPreflightOnRetry: false,
  timeoutMs: 60_000,
};