import createFetchClient, { type Client } from "openapi-fetch";
import type { paths } from "./schema.js";

export interface CreateClientOptions {
  baseUrl: string;
  apiKey: string;
  fetch?: typeof globalThis.fetch;
}

export function createClient(opts: CreateClientOptions): Client<paths> {
  return createFetchClient<paths>({
    baseUrl: opts.baseUrl,
    headers: { "X-API-Key": opts.apiKey },
    ...(opts.fetch ? { fetch: opts.fetch } : {}),
  });
}
