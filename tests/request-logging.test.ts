// A rejected request must leave a trace.
//
// Background: a downstream client reported "connection failed" while the proxy
// log stayed completely clean. Local validation rejections returned a 400 and
// wrote nothing, so it was impossible to tell apart three very different
// situations — the proxy rejected the request, the request never arrived, or
// the upstream failed. Diagnosing it turned into guesswork across several
// rounds. These tests pin the arrival record and the rejection reason so the
// log alone can answer the question.
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import http from "node:http";
import { once } from "node:events";
import { createServer } from "@/server.js";

const servers: http.Server[] = [];

afterEach(async () => {
  vi.restoreAllMocks();
  for (const server of servers.splice(0)) {
    server.closeAllConnections();
    await new Promise<void>((resolve) => server.close(() => resolve()));
  }
});

async function listen() {
  const server = createServer({
    host: "127.0.0.1",
    port: 0,
    ccApiBase: "https://upstream.invalid",
    ccVersion: "0.0.0",
    logLevel: "debug",
    corsOrigin: "",
    upstreamTimeoutMs: 1000,
    idleTimeoutMs: 0,
    maxBodyBytes: 1024 * 1024,
  });
  servers.push(server);
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  return (server.address() as { port: number }).port;
}

/** POST a raw body, returning the status. */
async function post(port: number, pathname: string, body: string, headers: Record<string, string> = {}) {
  const req = http.request({
    host: "127.0.0.1",
    port,
    path: pathname,
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Authorization: "Bearer fixture-key",
      "x-request-id": "trace-me-1234",
      ...headers,
    },
  });
  req.end(body);
  const [res] = (await once(req, "response")) as [http.IncomingMessage];
  for await (const _ of res) { /* drain */ }
  return res.statusCode;
}

/** Collect everything written to console while fn runs. */
async function captureConsole(fn: () => Promise<void>): Promise<string> {
  const lines: string[] = [];
  const push = (...args: unknown[]) => lines.push(args.map(String).join(" "));
  vi.spyOn(console, "log").mockImplementation(push);
  vi.spyOn(console, "warn").mockImplementation(push);
  vi.spyOn(console, "error").mockImplementation(push);
  try {
    await fn();
  } finally {
    vi.restoreAllMocks();
  }
  return lines.join("\n");
}

describe("request logging", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
  });

  it("logs the arrival of a request, carrying the client's X-Request-Id", async () => {
    const port = await listen();
    const out = await captureConsole(async () => {
      await post(port, "/v1/chat/completions", JSON.stringify({
        model: "m", max_tokens: 8, messages: [{ role: "user", content: "hi" }],
      }));
    });
    expect(out).toContain("trace-me-1234");
    expect(out).toContain("/v1/chat/completions");
  });

  it("logs a locally-rejected request with the reason", async () => {
    const port = await listen();
    const out = await captureConsole(async () => {
      // Anthropic requires max_tokens; omitting it is rejected before any
      // upstream call — exactly the case that used to be invisible.
      await post(port, "/v1/messages", JSON.stringify({
        model: "m", messages: [{ role: "user", content: "hi" }],
      }), { "anthropic-version": "2023-06-01" });
    });
    expect(out).toMatch(/reject/i);
    expect(out).toContain("400");
    expect(out).toMatch(/max_tokens/);
    expect(out).toContain("trace-me-1234");
  });

  it("logs the rejection of a malformed JSON body", async () => {
    const port = await listen();
    const out = await captureConsole(async () => {
      await post(port, "/v1/chat/completions", "{not json");
    });
    expect(out).toMatch(/reject/i);
    expect(out).toContain("400");
  });

  it("logs a 401 for a missing key", async () => {
    const port = await listen();
    const out = await captureConsole(async () => {
      await post(port, "/v1/chat/completions", JSON.stringify({
        model: "m", max_tokens: 8, messages: [{ role: "user", content: "hi" }],
      }), { Authorization: "" });
    });
    expect(out).toMatch(/reject/i);
    expect(out).toContain("401");
  });

  it("logs a 404 for an unknown route", async () => {
    const port = await listen();
    const out = await captureConsole(async () => {
      await post(port, "/no/such/route", "{}");
    });
    expect(out).toContain("404");
  });

  it("records the outcome and duration once the response completes", async () => {
    const port = await listen();
    const out = await captureConsole(async () => {
      await post(port, "/v1/messages", JSON.stringify({
        model: "m", messages: [{ role: "user", content: "hi" }],
      }), { "anthropic-version": "2023-06-01" });
    });
    // The completion line is what distinguishes a slow streaming answer from an
    // instant rejection when reading the log after the fact.
    expect(out).toMatch(/in \d+ms/);
  });

  it("uses one request id across arrival, rejection and completion", async () => {
    const port = await listen();
    const out = await captureConsole(async () => {
      await post(port, "/v1/messages", JSON.stringify({
        model: "m", messages: [{ role: "user", content: "hi" }],
      }), { "anthropic-version": "2023-06-01" });
    });
    // Every line for this request must carry the same id, or the log cannot be
    // followed as a single story.
    const ids = [...out.matchAll(/\[([0-9a-f-]{36}|trace-me-1234)\]/g)].map((m) => m[1]);
    expect(new Set(ids).size).toBe(1);
    expect(ids.length).toBeGreaterThanOrEqual(2);
  });

  it("generates an id when the client sends none", async () => {
    const port = await listen();
    const out = await captureConsole(async () => {
      const req = http.request({
        host: "127.0.0.1",
        port,
        path: "/v1/chat/completions",
        method: "POST",
        headers: { "Content-Type": "application/json", Authorization: "Bearer k" },
      });
      req.end(JSON.stringify({ model: "m", max_tokens: 8, messages: [{ role: "user", content: "hi" }] }));
      const [res] = (await once(req, "response")) as [http.IncomingMessage];
      for await (const _ of res) { /* drain */ }
      void res;
    });
    // A UUID-shaped id, so a request without a client-supplied one is still
    // traceable.
    expect(out).toMatch(/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/);
  });
});
