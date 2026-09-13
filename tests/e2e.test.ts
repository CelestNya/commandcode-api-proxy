import { describe, it, expect, beforeAll, afterAll, afterEach, vi } from "vitest";
import http from "node:http";
import { Readable } from "node:stream";
import { loadConfig } from "@/config.js";
import { createServer } from "@/server.js";
import { __resetCatalogForTests } from "@/translate/catalog.js";
import { UpstreamError } from "@/upstream.js";
import { mockCcModelsFetch } from "./helpers.js";
import type { CCEvent } from "@/translate/types.js";

// ──────────────────────────────────────────
// Mock sendToCC — factory must be self-contained (vi.mock is hoisted)
// ──────────────────────────────────────────

const { sendToCCSpy } = vi.hoisted(() => ({
  sendToCCSpy: vi.fn(),
}));

vi.mock("@/upstream.js", () => ({
  sendToCC: sendToCCSpy,
  collectEvents: vi.fn().mockImplementation(async (stream: NodeJS.ReadableStream) => {
    const events: CCEvent[] = [];
    return new Promise((resolve, reject) => {
      stream.on("data", (e: CCEvent) => events.push(e));
      stream.on("end", () => resolve(events));
      stream.on("error", reject);
    });
  }),
  UpstreamError: class UpstreamError extends Error {
    statusCode: number;
    isRetryable: boolean;
    constructor(message: string, statusCode: number, isRetryable: boolean) {
      super(message);
      this.name = "UpstreamError";
      this.statusCode = statusCode;
      this.isRetryable = isRetryable;
    }
  },
}));

const MOCK_CC_EVENTS: CCEvent[] = [
  { type: "start", data: { model: "deepseek/deepseek-v4-flash" } },
  { type: "text-delta", data: { text: "Hello world" } },
  {
    type: "finish",
    data: {
      finishReason: "stop",
      usage: { promptTokens: 5, completionTokens: 10, totalTokens: 15 },
    },
  },
];

function fakeStream(): NodeJS.ReadableStream {
  return Readable.from(MOCK_CC_EVENTS);
}

// ──────────────────────────────────────────
// E2E: OpenAI /v1/chat/completions
// ──────────────────────────────────────────

describe("E2E: OpenAI /v1/chat/completions", () => {
  let server: http.Server;
  let baseUrl: string;
  const port = 19001;

  beforeAll(async () => {
    const config = { ...loadConfig(), port, apiKey: "test-key", host: "127.0.0.1" };
    server = createServer(config);
    await new Promise<void>((resolve) => server.listen(port, "127.0.0.1", () => resolve()));
    baseUrl = `http://127.0.0.1:${port}`;
  });

  afterAll(() => new Promise<void>((resolve) => server.close(() => resolve())));

  afterEach(() => {
    sendToCCSpy.mockReset();
  });

  it("non-streaming: returns full chat completion", async () => {
    sendToCCSpy.mockResolvedValue({ stream: fakeStream() });

    const res = await fetch(`${baseUrl}/v1/chat/completions`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Authorization: "Bearer test-key",
      },
      body: JSON.stringify({
        model: "deepseek/deepseek-v4-flash",
        messages: [{ role: "user", content: "Hi" }],
        stream: false,
      }),
    });

    expect(res.status).toBe(200);
    const body = (await res.json()) as any;
    expect(body.object).toBe("chat.completion");
    expect(body.choices).toHaveLength(1);
    expect(body.choices[0].message.content).toBe("Hello world");
    expect(body.choices[0].message.role).toBe("assistant");
    expect(body.choices[0].finish_reason).toBe("stop");
    expect(body.usage).toBeDefined();
    expect(body.usage.prompt_tokens).toBe(5);
    expect(body.usage.completion_tokens).toBe(10);
    expect(sendToCCSpy).toHaveBeenCalledOnce();
  });

  it("streaming: returns SSE chunks", async () => {
    sendToCCSpy.mockResolvedValue({ stream: fakeStream() });

    const res = await fetch(`${baseUrl}/v1/chat/completions`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Authorization: "Bearer test-key",
      },
      body: JSON.stringify({
        model: "deepseek/deepseek-v4-flash",
        messages: [{ role: "user", content: "Hi" }],
        stream: true,
      }),
    });

    expect(res.status).toBe(200);
    expect(res.headers.get("content-type")).toContain("text/event-stream");

    const text = await res.text();
    const lines = text.split("\n").filter((l) => l.startsWith("data: ") && !l.includes("[DONE]"));
    expect(lines.length).toBeGreaterThan(0);

    const parsed = lines.map((l) => JSON.parse(l.replace("data: ", "")));

    const firstChunk = parsed[0];
    expect(firstChunk.object).toBe("chat.completion.chunk");
    expect(firstChunk.choices[0].delta.role).toBe("assistant");

    // The finish chunk carries finish_reason (a trailing usage chunk has empty choices).
    const finishChunk = parsed.find((c) => c.choices?.[0]?.finish_reason);
    expect(finishChunk).toBeDefined();
    expect(finishChunk.choices[0].finish_reason).toBe("stop");
  });

  // Regression: when the upstream stream ends without a `finish` event
  // (network drop, CC restart mid-tool-call), the proxy must synthesize a
  // finish_reason chunk so the OpenAI SDK sees a well-formed end-of-stream
  // instead of an abruptly truncated response.
  it("streaming: synthesizes a finish chunk when upstream ends without finish", async () => {
    const truncatedEvents: CCEvent[] = [
      { type: "start", data: {} },
      { type: "text-delta", data: { text: "partial response" } },
      // NO finish event — simulates upstream connection drop
    ];
    sendToCCSpy.mockResolvedValue({ stream: Readable.from(truncatedEvents) });

    const res = await fetch(`${baseUrl}/v1/chat/completions`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Authorization: "Bearer test-key",
      },
      body: JSON.stringify({
        model: "deepseek/deepseek-v4-flash",
        messages: [{ role: "user", content: "Hi" }],
        stream: true,
      }),
    });

    expect(res.status).toBe(200);
    const text = await res.text();
    // Even with no upstream finish, the proxy must close the SSE stream
    // cleanly with a synthesized finish_reason and the [DONE] sentinel.
    expect(text).toContain("[DONE]");
    const lines = text.split("\n").filter((l) => l.startsWith("data: ") && !l.includes("[DONE]"));
    const parsed = lines.map((l) => JSON.parse(l.replace("data: ", "")));
    const finishChunk = parsed.find((c) => c.choices?.[0]?.finish_reason);
    expect(finishChunk).toBeDefined();
    expect(finishChunk.choices[0].finish_reason).toBe("stop");
  });

  it("returns 400 for invalid JSON body", async () => {
    const res = await fetch(`${baseUrl}/v1/chat/completions`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Authorization: "Bearer test-key",
      },
      body: "not json",
    });
    expect(res.status).toBe(400);
    const body = (await res.json()) as any;
    expect(body.error.message).toBe("Invalid JSON body");
  });

  // Regression: an upstream `error` event ends the generation. The server MUST
  // NOT also synthesize a finish chunk on stream end — the error envelope is
  // already terminal, and a trailing finish_reason:"stop" would contradict it
  // by telling the client the turn succeeded.
  it("streaming: upstream error event emits one error envelope and no finish chunk", async () => {
    const errorEvents: CCEvent[] = [
      { type: "start", data: {} },
      { type: "error", data: { message: "CC upstream exploded" } },
      // No finish event; stream just ends after the error.
    ];
    sendToCCSpy.mockResolvedValue({ stream: Readable.from(errorEvents) });

    const res = await fetch(`${baseUrl}/v1/chat/completions`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Authorization: "Bearer test-key",
      },
      body: JSON.stringify({
        model: "deepseek/deepseek-v4-flash",
        messages: [{ role: "user", content: "Hi" }],
        stream: true,
      }),
    });

    expect(res.status).toBe(200);
    const text = await res.text();
    expect(text).toContain("[DONE]");
    const lines = text.split("\n").filter((l) => l.startsWith("data: ") && !l.includes("[DONE]"));
    const parsed = lines.map((l) => JSON.parse(l.replace("data: ", "")));

    // The failure is reported through the error envelope exactly once.
    const envelopes = parsed.filter((c) => c.error);
    expect(envelopes).toHaveLength(1);
    expect(envelopes[0].error.message).toContain("CC upstream exploded");
    // No chunk claims a normal finish, and none leaks the error as content.
    const finishChunks = parsed.filter((c) => c.choices?.[0]?.finish_reason);
    expect(finishChunks).toHaveLength(0);
    const contentText = parsed
      .flatMap((c) => c.choices?.[0]?.delta?.content ?? [])
      .join("");
    expect(contentText).not.toContain("CC upstream exploded");
  });

  // Regression: a stream-level error (TCP failure, idle timeout, encoder
  // throw) must reach the client as a protocol error, not as assistant text.
  // While the failure was wrapped in delta.content the client counted the turn
  // as successful and never retried it.
  it("streaming: stream-level error surfaces as an error envelope", async () => {
    // Build a stream that emits one event, then errors out. Attach an
    // error listener up-front so destroy(err) doesn't trigger Node's
    // unhandled-exception path (pumpStream's for-await handles the read
    // rejection, but the bare 'error' event still propagates).
    const stream = new Readable({ objectMode: true, read() {} });
    stream.on("error", () => {
      /* swallowed — pumpStream catches the read() rejection */
    });
    process.nextTick(() => {
      stream.push({ type: "start", data: {} });
      stream.push({ type: "text-delta", data: { text: "partial" } });
      stream.destroy(new Error("simulated TCP RST"));
    });
    sendToCCSpy.mockResolvedValue({ stream });

    const res = await fetch(`${baseUrl}/v1/chat/completions`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Authorization: "Bearer test-key",
      },
      body: JSON.stringify({
        model: "deepseek/deepseek-v4-flash",
        messages: [{ role: "user", content: "Hi" }],
        stream: true,
      }),
    });

    expect(res.status).toBe(200);
    const text = await res.text();
    const lines = text.split("\n").filter((l) => l.startsWith("data: ") && !l.includes("[DONE]"));
    const parsed = lines.map((l) => JSON.parse(l.replace("data: ", "")));
    // Chunks emitted before the failure stay valid chat.completion.chunks.
    const chunkRecords = parsed.filter((c) => !c.error);
    for (const chunk of chunkRecords) {
      expect(chunk.object).toBe("chat.completion.chunk");
    }
    // The failure arrives as an error envelope so the caller can retry, and it
    // never masquerades as assistant content or as a normal finish.
    const envelopes = parsed.filter((c) => c.error);
    expect(envelopes).toHaveLength(1);
    expect(envelopes[0].error.message).toContain("simulated TCP RST");
    const contentText = parsed
      .flatMap((c) => c.choices?.[0]?.delta?.content ?? [])
      .join("");
    expect(contentText).not.toContain("simulated TCP RST");
    const finishChunks = parsed.filter((c) => c.choices?.[0]?.finish_reason);
    expect(finishChunks).toHaveLength(0);
  });
});

// ──────────────────────────────────────────
// E2E: dynamic model catalog
// ──────────────────────────────────────────

describe("E2E: dynamic model catalog", () => {
  let server: http.Server;
  let baseUrl: string;
  const port = 19008;

  beforeAll(async () => {
    // No server-held key: the startup catalog refresh is skipped, which is
    // exactly the state a proxy lands in when auth is configured after launch.
    const config = { ...loadConfig(), port, apiKey: null as string | null, host: "127.0.0.1" };
    server = createServer(config);
    await new Promise<void>((resolve) => server.listen(port, "127.0.0.1", () => resolve()));
    baseUrl = `http://127.0.0.1:${port}`;
  });

  afterAll(() => new Promise<void>((resolve) => server.close(() => resolve())));

  let catalogFetchSpy: ReturnType<typeof mockCcModelsFetch> | undefined;

  afterEach(() => {
    sendToCCSpy.mockReset();
    // Restore ONLY the fetch spy. A blanket restoreAllMocks() would also wipe
    // the vi.mock factory's collectEvents implementation, breaking every test
    // that runs after this block.
    catalogFetchSpy?.mockRestore();
    catalogFetchSpy = undefined;
    __resetCatalogForTests();
  });

  // Regression: a bare model name present only in the provider API used to be
  // forwarded verbatim, and CC rewrites an unprefixed name to
  // `anthropic:<name>`, failing the request with 403 FORBIDDEN. On exactly that
  // rejection the catalog must be refreshed from the request's own key and the
  // request retried, so the name resolves to its full org-prefixed id.
  it("retries a 403-rejected bare model name after learning it from the provider API", async () => {
    catalogFetchSpy = mockCcModelsFetch([
      { id: "deepseek/deepseek-v4.1-flash", name: "DeepSeek V4.1 Flash", context_length: 1000000 },
    ]);
    // First attempt: CC rejects the unresolved bare name. Retry: succeeds.
    sendToCCSpy
      .mockRejectedValueOnce(
        new UpstreamError(
          'CC API 403: {"message":"Model/provider not recognized: anthropic:deepseek-v4.1-flash"}',
          403,
          false,
        ),
      )
      .mockResolvedValueOnce({ stream: fakeStream() });

    const res = await fetch(`${baseUrl}/v1/chat/completions`, {
      method: "POST",
      headers: { "Content-Type": "application/json", Authorization: "Bearer client-key" },
      body: JSON.stringify({
        model: "deepseek-v4.1-flash",
        messages: [{ role: "user", content: "Hi" }],
        stream: false,
      }),
    });

    expect(res.status).toBe(200);
    expect(sendToCCSpy).toHaveBeenCalledTimes(2);
    // The retry carries the resolved id, not the bare name.
    const retryBody = sendToCCSpy.mock.calls[1][0] as { params: { model: string } };
    expect(retryBody.params.model).toBe("deepseek/deepseek-v4.1-flash");
  });

  // Regression: the retry used to rebuild the whole request, minting a fresh
  // threadId → a second x-session-id upstream, i.e. two billable sessions for
  // one user intent. The session id must be pinned across the retry.
  it("pins threadId across the model-discovery retry", async () => {
    catalogFetchSpy = mockCcModelsFetch([
      { id: "deepseek/deepseek-v4.1-flash", name: "DeepSeek V4.1 Flash", context_length: 1000000 },
    ]);
    sendToCCSpy
      .mockRejectedValueOnce(
        new UpstreamError(
          'CC API 403: {"message":"Model/provider not recognized: anthropic:deepseek-v4.1-flash"}',
          403,
          false,
        ),
      )
      .mockResolvedValueOnce({ stream: fakeStream() });

    const res = await fetch(`${baseUrl}/v1/chat/completions`, {
      method: "POST",
      headers: { "Content-Type": "application/json", Authorization: "Bearer client-key" },
      body: JSON.stringify({
        model: "deepseek-v4.1-flash",
        messages: [{ role: "user", content: "Hi" }],
        stream: false,
      }),
    });

    expect(res.status).toBe(200);
    expect(sendToCCSpy).toHaveBeenCalledTimes(2);
    const first = sendToCCSpy.mock.calls[0][0] as { threadId: string };
    const second = sendToCCSpy.mock.calls[1][0] as { threadId: string };
    expect(first.threadId).toBeTruthy();
    expect(second.threadId).toBe(first.threadId);
  });

  // A model that genuinely does not exist stays unknown after the refresh, so
  // the original 403 must surface rather than being retried forever.
  it("does not retry a 403 for a model that the provider API does not know", async () => {
    catalogFetchSpy = mockCcModelsFetch([
      { id: "deepseek/deepseek-v4.1-flash", name: "DeepSeek V4.1 Flash", context_length: 1000000 },
    ]);
    sendToCCSpy.mockRejectedValue(
      new UpstreamError(
        'CC API 403: {"message":"Model/provider not recognized: anthropic:no-such-model"}',
        403,
        false,
      ),
    );

    const res = await fetch(`${baseUrl}/v1/chat/completions`, {
      method: "POST",
      headers: { "Content-Type": "application/json", Authorization: "Bearer client-key" },
      body: JSON.stringify({
        model: "no-such-model",
        messages: [{ role: "user", content: "Hi" }],
        stream: false,
      }),
    });

    expect(res.status).toBe(403);
    expect(sendToCCSpy).toHaveBeenCalledTimes(1);
  });
});

// ──────────────────────────────────────────
// E2E: Auth (keyless passthrough mode)
// ──────────────────────────────────────────

describe("E2E: Auth", () => {
  let server: http.Server;
  let baseUrl: string;
  const port = 19004;

  beforeAll(async () => {
    const config = { ...loadConfig(), port, apiKey: null, host: "127.0.0.1" };
    server = createServer(config);
    await new Promise<void>((resolve) => server.listen(port, "127.0.0.1", () => resolve()));
    baseUrl = `http://127.0.0.1:${port}`;
  });

  afterAll(() => new Promise<void>((resolve) => server.close(() => resolve())));

  it("returns 401 for /v1/chat/completions without API key", async () => {
    const res = await fetch(`${baseUrl}/v1/chat/completions`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ model: "default", messages: [{ role: "user", content: "Hi" }] }),
    });
    expect(res.status).toBe(401);
  });
});

// ──────────────────────────────────────────
// E2E: health and models
// ──────────────────────────────────────────

describe("E2E: health and models", () => {
  let server: http.Server;
  let baseUrl: string;
  const port = 19003;

  beforeAll(async () => {
    const config = { ...loadConfig(), port, apiKey: null, host: "127.0.0.1" };
    server = createServer(config);
    await new Promise<void>((resolve) => server.listen(port, "127.0.0.1", () => resolve()));
    baseUrl = `http://127.0.0.1:${port}`;
  });

  afterAll(() => new Promise<void>((resolve) => server.close(() => resolve())));

  it("GET /health returns ok", async () => {
    const res = await fetch(`${baseUrl}/health`);
    expect(res.status).toBe(200);
    const body = (await res.json()) as any;
    expect(body.status).toBe("ok");
  });

  it("GET /v1/models returns model list", async () => {
    const res = await fetch(`${baseUrl}/v1/models`);
    expect(res.status).toBe(200);
    const body = (await res.json()) as any;
    expect(body.object).toBe("list");
    expect(body.data.length).toBeGreaterThan(0);
  });

  it("returns 404 for unknown routes", async () => {
    const res = await fetch(`${baseUrl}/v1/unknown`);
    expect(res.status).toBe(404);
  });

  it("supports CORS preflight", async () => {
    const res = await fetch(`${baseUrl}/v1/models`, { method: "OPTIONS" });
    expect(res.status).toBe(204);
    expect(res.headers.get("access-control-allow-origin")).toBe("*");
  });
});

// ──────────────────────────────────────────
// E2E: Anthropic /v1/messages
// ──────────────────────────────────────────

describe("E2E: Anthropic /v1/messages", () => {
  let server: http.Server;
  let baseUrl: string;
  const port = 19005;

  beforeAll(async () => {
    const config = { ...loadConfig(), port, apiKey: "test-key", host: "127.0.0.1" };
    server = createServer(config);
    await new Promise<void>((resolve) => server.listen(port, "127.0.0.1", () => resolve()));
    baseUrl = `http://127.0.0.1:${port}`;
  });

  afterAll(() => new Promise<void>((resolve) => server.close(() => resolve())));

  afterEach(() => {
    sendToCCSpy.mockReset();
  });

  it("non-streaming: returns full Anthropic message", async () => {
    sendToCCSpy.mockResolvedValue({ stream: fakeStream() });

    const res = await fetch(`${baseUrl}/v1/messages`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "x-api-key": "test-key",
      },
      body: JSON.stringify({
        model: "claude-sonnet-4-5-20250929",
        max_tokens: 100,
        messages: [{ role: "user", content: "Hi" }],
        stream: false,
      }),
    });

    expect(res.status).toBe(200);
    const body = (await res.json()) as any;
    expect(body.type).toBe("message");
    expect(body.role).toBe("assistant");
    expect(body.content).toBeInstanceOf(Array);
    expect(body.content.length).toBeGreaterThan(0);
    expect(body.content[0].type).toBe("text");
    expect(body.content[0].text).toBe("Hello world");
    expect(body.stop_reason).toBe("end_turn");
    expect(body.usage).toBeDefined();
    expect(body.usage.input_tokens).toBe(5);
    expect(body.usage.output_tokens).toBe(10);
  });

  it("streaming: returns Anthropic SSE events", async () => {
    sendToCCSpy.mockResolvedValue({ stream: fakeStream() });

    const res = await fetch(`${baseUrl}/v1/messages`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "x-api-key": "test-key",
      },
      body: JSON.stringify({
        model: "claude-sonnet-4-5-20250929",
        max_tokens: 100,
        messages: [{ role: "user", content: "Hi" }],
        stream: true,
      }),
    });

    expect(res.status).toBe(200);
    expect(res.headers.get("content-type")).toContain("text/event-stream");

    const text = await res.text();
    expect(text).toContain("event: message_start");
    expect(text).toContain("event: content_block_start");
    expect(text).toContain("event: content_block_delta");
    expect(text).toContain("event: content_block_stop");
    expect(text).toContain("event: message_delta");
    expect(text).toContain("event: message_stop");
  });

  // Regression: when the upstream stream ends without a `finish` event
  // (network drop, CC restart mid-tool-call), the proxy must synthesize the
  // closing message_delta + message_stop so the Anthropic SDK sees a
  // well-formed end-of-stream instead of a truncated response.
  it("streaming: synthesizes message_stop when upstream ends without finish", async () => {
    const truncatedEvents: CCEvent[] = [
      { type: "start", data: {} },
      { type: "text-delta", data: { text: "partial" } },
      // NO finish event — simulates upstream connection drop
    ];
    sendToCCSpy.mockResolvedValue({ stream: Readable.from(truncatedEvents) });

    const res = await fetch(`${baseUrl}/v1/messages`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "x-api-key": "test-key",
      },
      body: JSON.stringify({
        model: "claude-sonnet-4-5-20250929",
        max_tokens: 100,
        messages: [{ role: "user", content: "Hi" }],
        stream: true,
      }),
    });

    expect(res.status).toBe(200);
    const text = await res.text();
    // Even with no upstream finish, the proxy must close the SSE stream
    // cleanly with a synthesized message_delta + message_stop.
    expect(text).toContain("event: message_start");
    expect(text).toContain("event: message_delta");
    expect(text).toContain("event: message_stop");
    expect(text).toContain('"stop_reason"');
  });

  // Regression: an upstream `error` event already emits message_stop. The
  // server MUST NOT synthesize a second one (Anthropic SDK throws on
  // duplicate message_stop events).
  it("streaming: upstream error event does not produce duplicate message_stop", async () => {
    const errorEvents: CCEvent[] = [
      { type: "start", data: {} },
      { type: "error", data: { message: "CC upstream exploded" } },
    ];
    sendToCCSpy.mockResolvedValue({ stream: Readable.from(errorEvents) });

    const res = await fetch(`${baseUrl}/v1/messages`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "x-api-key": "test-key",
      },
      body: JSON.stringify({
        model: "claude-sonnet-4-5-20250929",
        max_tokens: 100,
        messages: [{ role: "user", content: "Hi" }],
        stream: true,
      }),
    });

    expect(res.status).toBe(200);
    const text = await res.text();
    const stopCount = (text.match(/event: message_stop/g) ?? []).length;
    expect(stopCount).toBe(1);
  });

  it("returns 400 for invalid JSON body with Anthropic error shape", async () => {
    const res = await fetch(`${baseUrl}/v1/messages`, {
      method: "POST",
      headers: { "Content-Type": "application/json", "x-api-key": "test-key" },
      body: "not json",
    });
    expect(res.status).toBe(400);
    const body = (await res.json()) as any;
    expect(body.type).toBe("error");
    expect(body.error.type).toBe("invalid_request_error");
  });

  it("returns 400 for missing max_tokens with Anthropic error shape", async () => {
    const res = await fetch(`${baseUrl}/v1/messages`, {
      method: "POST",
      headers: { "Content-Type": "application/json", "x-api-key": "test-key" },
      body: JSON.stringify({
        model: "claude-sonnet-4-5-20250929",
        messages: [{ role: "user", content: "Hi" }],
      }),
    });
    expect(res.status).toBe(400);
    const body = (await res.json()) as any;
    expect(body.type).toBe("error");
    expect(body.error.type).toBe("invalid_request_error");
  });
});

// ──────────────────────────────────────────
// E2E: Anthropic /v1/messages/count_tokens
// ──────────────────────────────────────────

describe("E2E: Anthropic /v1/messages/count_tokens", () => {
  let server: http.Server;
  let baseUrl: string;
  const port = 19006;

  beforeAll(async () => {
    const config = { ...loadConfig(), port, apiKey: "test-key", host: "127.0.0.1" };
    server = createServer(config);
    await new Promise<void>((resolve) => server.listen(port, "127.0.0.1", () => resolve()));
    baseUrl = `http://127.0.0.1:${port}`;
  });

  afterAll(() => new Promise<void>((resolve) => server.close(() => resolve())));

  it("returns estimated input_tokens", async () => {
    const res = await fetch(`${baseUrl}/v1/messages/count_tokens`, {
      method: "POST",
      headers: { "Content-Type": "application/json", "x-api-key": "test-key" },
      body: JSON.stringify({
        model: "claude-sonnet-4-5-20250929",
        messages: [{ role: "user", content: "Hello World!" }],
      }),
    });

    expect(res.status).toBe(200);
    const body = (await res.json()) as any;
    expect(body.input_tokens).toBeDefined();
    expect(typeof body.input_tokens).toBe("number");
    expect(body.input_tokens).toBeGreaterThan(0);
  });
});

// ──────────────────────────────────────────
// E2E: /v1/models with anthropic-version header
// ──────────────────────────────────────────

describe("E2E: /v1/models Anthropic shape", () => {
  let server: http.Server;
  let baseUrl: string;
  const port = 19007;

  beforeAll(async () => {
    const config = { ...loadConfig(), port, apiKey: null, host: "127.0.0.1" };
    server = createServer(config);
    await new Promise<void>((resolve) => server.listen(port, "127.0.0.1", () => resolve()));
    baseUrl = `http://127.0.0.1:${port}`;
  });

  afterAll(() => new Promise<void>((resolve) => server.close(() => resolve())));

  it("returns Anthropic model shape with anthropic-version header", async () => {
    const res = await fetch(`${baseUrl}/v1/models`, {
      headers: { "anthropic-version": "2023-06-01" },
    });

    expect(res.status).toBe(200);
    const body = (await res.json()) as any;
    expect(body.data).toBeInstanceOf(Array);
    expect(body.data.length).toBeGreaterThan(0);
    expect(body.data[0].type).toBe("model");
    expect(body.data[0]).toHaveProperty("display_name");
    expect(body.data[0]).toHaveProperty("created_at");
    expect(body).toHaveProperty("has_more");
    expect(body).toHaveProperty("first_id");
    expect(body).toHaveProperty("last_id");
  });

  it("returns OpenAI model shape without anthropic-version header", async () => {
    const res = await fetch(`${baseUrl}/v1/models`);

    expect(res.status).toBe(200);
    const body = (await res.json()) as any;
    expect(body.object).toBe("list");
    expect(body.data[0].object).toBe("model");
  });
});
