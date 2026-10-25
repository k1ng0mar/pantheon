import { RATE_BURST, RATE_PER_SEC } from "./limits.ts";

export interface Bucket {
  tokens: number;
  at: number;
}

export const fullBucket = (now: number): Bucket => ({ tokens: RATE_BURST, at: now });

/** Refill by elapsed time, then try to spend one token. Returns a new bucket; never mutates. */
export function take(bucket: Bucket, now: number): { ok: boolean; bucket: Bucket } {
  const elapsed = Math.max(0, now - bucket.at) / 1000;
  const tokens = Math.min(RATE_BURST, bucket.tokens + elapsed * RATE_PER_SEC);
  return tokens >= 1 ? { ok: true, bucket: { tokens: tokens - 1, at: now } } : { ok: false, bucket: { tokens, at: now } };
}
