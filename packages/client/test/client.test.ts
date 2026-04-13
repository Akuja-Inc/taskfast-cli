import { describe, expect, it, vi } from "vitest";
import { http, HttpResponse } from "msw";
import { createClient } from "../src/client.js";
import { AuthError, RateLimited, ServerError, ValidationError } from "../src/errors.js";
import { TEST_BASE_URL, use } from "./setup.js";

const SDK_BASE_URL = `${TEST_BASE_URL}/api`;

const agentBody = {
  owner_id: "00000000-0000-0000-0000-000000000000",
  name: "t",
  description: "t",
  capabilities: ["research"],
};

describe("createClient", () => {
  it("injects X-API-Key header on every request", async () => {
    const seen = vi.fn();
    use(
      http.get(`${SDK_BASE_URL}/agents/me`, ({ request }) => {
        seen(request.headers.get("x-api-key"));
        return HttpResponse.json({ id: "a", status: "active" });
      }),
    );
    const client = createClient({ baseUrl: SDK_BASE_URL, apiKey: "test-key" });
    await client.GET("/agents/me", {});
    expect(seen).toHaveBeenCalledWith("test-key");
  });

  it("throws AuthError on 401 carrying response body", async () => {
    use(
      http.get(`${SDK_BASE_URL}/agents/me`, () =>
        HttpResponse.json({ error: "unauthorized", message: "invalid key" }, { status: 401 }),
      ),
    );
    const client = createClient({ baseUrl: SDK_BASE_URL, apiKey: "bad" });
    await expect(client.GET("/agents/me", {})).rejects.toMatchObject({
      name: "AuthError",
      status: 401,
      body: { error: "unauthorized", message: "invalid key" },
    });
    await expect(client.GET("/agents/me", {})).rejects.toBeInstanceOf(AuthError);
  });

  it("throws ValidationError on 422 carrying server error_code", async () => {
    use(
      http.post(`${SDK_BASE_URL}/agents`, () =>
        HttpResponse.json(
          { error_code: "self_bidding", message: "cannot bid on own task" },
          { status: 422 },
        ),
      ),
    );
    const client = createClient({ baseUrl: SDK_BASE_URL, apiKey: "k" });
    await expect(client.POST("/agents", { body: agentBody })).rejects.toMatchObject({
      name: "ValidationError",
      status: 422,
      errorCode: "self_bidding",
    });
    await expect(client.POST("/agents", { body: agentBody })).rejects.toBeInstanceOf(
      ValidationError,
    );
  });

  it("throws RateLimited on 429 carrying Retry-After seconds", async () => {
    use(
      http.get(`${SDK_BASE_URL}/agents/me`, () =>
        HttpResponse.json(
          { error: "rate_limited" },
          { status: 429, headers: { "Retry-After": "42" } },
        ),
      ),
    );
    const client = createClient({ baseUrl: SDK_BASE_URL, apiKey: "k" });
    await expect(client.GET("/agents/me", {})).rejects.toMatchObject({
      name: "RateLimited",
      status: 429,
      retryAfterSeconds: 42,
    });
    await expect(client.GET("/agents/me", {})).rejects.toBeInstanceOf(RateLimited);
  });

  it("retries 5xx with backoff and succeeds on recovery", async () => {
    let calls = 0;
    use(
      http.get(`${SDK_BASE_URL}/agents/me`, () => {
        calls += 1;
        if (calls < 3) return HttpResponse.json({ error: "boom" }, { status: 503 });
        return HttpResponse.json({ id: "a", status: "active" });
      }),
    );
    const client = createClient({
      baseUrl: SDK_BASE_URL,
      apiKey: "k",
      retry: { maxAttempts: 4, baseDelayMs: 1 },
    });
    const { data, error } = await client.GET("/agents/me", {});
    expect(error).toBeUndefined();
    expect(data).toMatchObject({ id: "a", status: "active" });
    expect(calls).toBe(3);
  });

  it("gives up after maxAttempts and throws ServerError", async () => {
    let calls = 0;
    use(
      http.get(`${SDK_BASE_URL}/agents/me`, () => {
        calls += 1;
        return HttpResponse.json({ error: "boom" }, { status: 503 });
      }),
    );
    const client = createClient({
      baseUrl: SDK_BASE_URL,
      apiKey: "k",
      retry: { maxAttempts: 3, baseDelayMs: 1 },
    });
    await expect(client.GET("/agents/me", {})).rejects.toMatchObject({
      name: "ServerError",
      status: 503,
    });
    await expect(client.GET("/agents/me", {})).rejects.toBeInstanceOf(ServerError);
    expect(calls).toBe(6);
  });

  it("treats 409 as idempotent success when treat409AsSuccess is set", async () => {
    use(
      http.post(`${SDK_BASE_URL}/agents`, () =>
        HttpResponse.json(
          { error_code: "wallet_already_configured", wallet_address: "0xabc" },
          { status: 409 },
        ),
      ),
    );
    const client = createClient({
      baseUrl: SDK_BASE_URL,
      apiKey: "k",
      treat409AsSuccess: true,
    });
    const { data, error } = await client.POST("/agents", { body: agentBody });
    expect(error).toBeUndefined();
    expect(data).toMatchObject({ wallet_address: "0xabc" });
  });

  it("409 without treat409AsSuccess throws ValidationError", async () => {
    use(
      http.post(`${SDK_BASE_URL}/agents`, () =>
        HttpResponse.json({ error_code: "wallet_already_configured" }, { status: 409 }),
      ),
    );
    const client = createClient({ baseUrl: SDK_BASE_URL, apiKey: "k" });
    await expect(client.POST("/agents", { body: agentBody })).rejects.toMatchObject({
      status: 409,
      name: "ValidationError",
    });
  });

  it("RateLimited.retryAfterSeconds is undefined when header missing", async () => {
    use(
      http.get(`${SDK_BASE_URL}/agents/me`, () =>
        HttpResponse.json({ error: "rate_limited" }, { status: 429 }),
      ),
    );
    const client = createClient({ baseUrl: SDK_BASE_URL, apiKey: "k" });
    await expect(client.GET("/agents/me", {})).rejects.toMatchObject({
      name: "RateLimited",
      retryAfterSeconds: undefined,
    });
  });
});
