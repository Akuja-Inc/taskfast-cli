import createFetchClient, { type Client } from "openapi-fetch";
import { responseToError } from "./errors.js";
import { DEFAULT_RETRY, type RetryOptions, withRetry } from "./retry.js";
import type { paths } from "./schema.js";

export interface CreateClientOptions {
  baseUrl: string;
  apiKey: string;
  fetch?: typeof globalThis.fetch;
  retry?: RetryOptions;
  treat409AsSuccess?: boolean;
}

export function createClient(opts: CreateClientOptions): Client<paths> {
  const retry = opts.retry ?? DEFAULT_RETRY;
  const baseFetch = opts.fetch ?? globalThis.fetch;
  const wrappedFetch = withRetry(baseFetch, retry);
  const treat409AsSuccess = opts.treat409AsSuccess ?? false;
  const client = createFetchClient<paths>({
    baseUrl: opts.baseUrl,
    headers: { "X-API-Key": opts.apiKey },
    fetch: wrappedFetch,
  });
  client.use({
    async onResponse({ response }) {
      if (response.ok) return undefined;
      if (treat409AsSuccess && response.status === 409) {
        const text = await response.clone().text();
        return new Response(text, {
          status: 200,
          headers: { "content-type": response.headers.get("content-type") ?? "application/json" },
        });
      }
      const body = await response
        .clone()
        .json()
        .catch(() => null);
      const err = responseToError(response, body);
      if (err) throw err;
      return undefined;
    },
  });
  return client;
}
