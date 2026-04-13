import type { components } from "@taskfast/client";

type AgentEvent = components["schemas"]["AgentEvent"];

export interface EventsClient {
  GET(
    path: "/agents/me/events",
    init: { params?: { query?: { cursor?: string; limit?: number } } },
  ): Promise<{
    data?: components["schemas"]["AgentEventListResponse"];
    error?: unknown;
  }>;
}

export interface PollEventsOptions {
  cursor?: string;
}

export async function* pollEvents(
  client: EventsClient,
  opts: PollEventsOptions,
): AsyncGenerator<AgentEvent, void, void> {
  let cursor = opts.cursor;
  while (true) {
    const query = cursor === undefined ? undefined : { cursor };
    const init = query === undefined ? {} : { params: { query } };
    const { data, error } = await client.GET("/agents/me/events", init);
    if (error || !data) return;
    for (const event of data.data) yield event;
    if (!data.meta.has_more || !data.meta.next_cursor) return;
    cursor = data.meta.next_cursor;
  }
}
