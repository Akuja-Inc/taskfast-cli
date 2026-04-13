import { RateLimited } from "@taskfast/client";

export interface BackoffOptions {
  maxAttempts: number;
  baseDelayMs: number;
  sleep?: (ms: number) => Promise<void>;
}

const defaultSleep = (ms: number): Promise<void> =>
  new Promise((resolve) => setTimeout(resolve, ms));

export async function withBackoff<T>(fn: () => Promise<T>, opts: BackoffOptions): Promise<T> {
  const sleep = opts.sleep ?? defaultSleep;
  let lastErr: unknown;
  for (let attempt = 1; attempt <= opts.maxAttempts; attempt += 1) {
    try {
      return await fn();
    } catch (err) {
      lastErr = err;
      if (attempt === opts.maxAttempts) break;
      const delay =
        err instanceof RateLimited && err.retryAfterSeconds !== undefined
          ? err.retryAfterSeconds * 1000
          : opts.baseDelayMs * 2 ** (attempt - 1);
      await sleep(delay);
    }
  }
  throw lastErr;
}
