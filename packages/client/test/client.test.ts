import { describe, expect, it, vi } from "vitest";
import { http, HttpResponse } from "msw";
import { createClient } from "../src/client.js";
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
});
