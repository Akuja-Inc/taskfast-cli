import { describe, expect, it, vi } from "vitest";
import { http, HttpResponse } from "msw";
import { createClient } from "../src/client.js";
import { AuthError, ValidationError } from "../src/errors.js";
import { TEST_BASE_URL, use } from "./setup.js";

describe("createClient", () => {
  it("injects X-API-Key header on every request", async () => {
    const seen = vi.fn();
    use(
      http.get(`${TEST_BASE_URL}/api/agents/me`, ({ request }) => {
        seen(request.headers.get("x-api-key"));
        return HttpResponse.json({ id: "a", status: "active" });
      }),
    );
    const client = createClient({ baseUrl: TEST_BASE_URL, apiKey: "test-key" });
    await client.GET("/api/agents/me");
    expect(seen).toHaveBeenCalledWith("test-key");
  });

  it("throws AuthError on 401 carrying response body", async () => {
    use(
      http.get(`${TEST_BASE_URL}/api/agents/me`, () =>
        HttpResponse.json({ error: "unauthorized", message: "invalid key" }, { status: 401 }),
      ),
    );
    const client = createClient({ baseUrl: TEST_BASE_URL, apiKey: "bad" });
    await expect(client.GET("/api/agents/me")).rejects.toMatchObject({
      name: "AuthError",
      status: 401,
      body: { error: "unauthorized", message: "invalid key" },
    });
    await expect(client.GET("/api/agents/me")).rejects.toBeInstanceOf(AuthError);
  });

  it("throws ValidationError on 422 carrying server error_code", async () => {
    use(
      http.post(`${TEST_BASE_URL}/api/agents/me/wallet`, () =>
        HttpResponse.json(
          { error_code: "self_bidding", message: "cannot bid on own task" },
          { status: 422 },
        ),
      ),
    );
    const client = createClient({ baseUrl: TEST_BASE_URL, apiKey: "k" });
    await expect(
      client.POST("/api/agents/me/wallet", {
        body: { wallet_address: "0x0" } as never,
      }),
    ).rejects.toMatchObject({
      name: "ValidationError",
      status: 422,
      errorCode: "self_bidding",
    });
    await expect(
      client.POST("/api/agents/me/wallet", {
        body: { wallet_address: "0x0" } as never,
      }),
    ).rejects.toBeInstanceOf(ValidationError);
  });
});
