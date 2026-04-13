import { describe, expect, it, vi } from "vitest";
import type { components } from "@taskfast/client";
import { pollEvents } from "../src/events.js";

type AgentEvent = components["schemas"]["AgentEvent"];

function ev(id: string): AgentEvent {
  return { id, type: "task.assigned" } as unknown as AgentEvent;
}

interface Page {
  data: AgentEvent[];
  meta: { has_more: boolean; next_cursor: string | null; total_count: number };
}

function stubClient(pages: Page[]): { GET: ReturnType<typeof vi.fn>; calls: unknown[][] } {
  const calls: unknown[][] = [];
  let i = 0;
  const GET = vi.fn(async (path: string, init: { params?: { query?: { cursor?: string } } }) => {
    calls.push([path, init.params?.query?.cursor]);
    const page = pages[i] ?? { data: [], meta: { has_more: false, next_cursor: null, total_count: 0 } };
    i += 1;
    return { data: page, error: undefined };
  });
  return { GET, calls };
}

describe("pollEvents", () => {
  it("yields events across pages and advances cursor via next_cursor", async () => {
    const client = stubClient([
      { data: [ev("a"), ev("b")], meta: { has_more: true, next_cursor: "cur-1", total_count: 2 } },
      { data: [ev("c")], meta: { has_more: false, next_cursor: null, total_count: 1 } },
    ]);
    const seen: string[] = [];
    for await (const e of pollEvents(client as never, {})) seen.push((e as { id: string }).id);
    expect(seen).toEqual(["a", "b", "c"]);
    expect(client.calls).toEqual([
      ["/agents/me/events", undefined],
      ["/agents/me/events", "cur-1"],
    ]);
  });

  it("stops on empty first page", async () => {
    const client = stubClient([
      { data: [], meta: { has_more: false, next_cursor: null, total_count: 0 } },
    ]);
    const seen: string[] = [];
    for await (const e of pollEvents(client as never, {})) seen.push((e as { id: string }).id);
    expect(seen).toEqual([]);
    expect(client.GET).toHaveBeenCalledTimes(1);
  });

  it("starts from a provided cursor", async () => {
    const client = stubClient([
      { data: [ev("z")], meta: { has_more: false, next_cursor: null, total_count: 1 } },
    ]);
    const it = pollEvents(client as never, { cursor: "start-here" });
    await it.next();
    expect(client.calls[0]).toEqual(["/agents/me/events", "start-here"]);
  });
});
