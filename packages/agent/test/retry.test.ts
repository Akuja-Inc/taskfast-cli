import { describe, expect, it, vi } from "vitest";
import { RateLimited } from "@taskfast/client";
import { withBackoff } from "../src/retry.js";

describe("withBackoff", () => {
  it("returns first success without sleeping", async () => {
    const fn = vi.fn().mockResolvedValue("ok");
    const sleep = vi.fn().mockResolvedValue(undefined);
    await expect(withBackoff(fn, { maxAttempts: 3, baseDelayMs: 10, sleep })).resolves.toBe("ok");
    expect(fn).toHaveBeenCalledTimes(1);
    expect(sleep).not.toHaveBeenCalled();
  });

  it("retries non-RateLimited errors with exponential backoff and succeeds on 3rd try", async () => {
    const fn = vi
      .fn()
      .mockRejectedValueOnce(new Error("transient"))
      .mockRejectedValueOnce(new Error("transient"))
      .mockResolvedValue("ok");
    const sleep = vi.fn().mockResolvedValue(undefined);
    await expect(withBackoff(fn, { maxAttempts: 3, baseDelayMs: 10, sleep })).resolves.toBe("ok");
    expect(fn).toHaveBeenCalledTimes(3);
    expect(sleep).toHaveBeenNthCalledWith(1, 10);
    expect(sleep).toHaveBeenNthCalledWith(2, 20);
  });

  it("honors RateLimited.retryAfterSeconds instead of exponential", async () => {
    const fn = vi
      .fn()
      .mockRejectedValueOnce(new RateLimited(429, null, 7))
      .mockResolvedValue("ok");
    const sleep = vi.fn().mockResolvedValue(undefined);
    await expect(withBackoff(fn, { maxAttempts: 3, baseDelayMs: 10, sleep })).resolves.toBe("ok");
    expect(sleep).toHaveBeenCalledWith(7000);
  });

  it("throws after exhausting maxAttempts", async () => {
    const err = new Error("permanent");
    const fn = vi.fn().mockRejectedValue(err);
    const sleep = vi.fn().mockResolvedValue(undefined);
    await expect(
      withBackoff(fn, { maxAttempts: 3, baseDelayMs: 1, sleep }),
    ).rejects.toBe(err);
    expect(fn).toHaveBeenCalledTimes(3);
  });
});
