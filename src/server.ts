import http from "node:http";
import { Readable } from "node:stream";
import { URL } from "node:url";
import type { Config } from "@/config.js";
import { toCCRequest, OpenAIStreamEncoder, buildNonStreamingResponse } from "@/translate/openai.js";
import {
  toCCRequest as anToCCRequest,
  AnthropicStreamEncoder,
  buildAnthropicResponse,
} from "@/translate/anthropic.js";
import { getCatalog } from "@/translate/catalog.js";
import { discoverModel } from "@/translate/models.js";
import { extractUsage } from "@/translate/util.js";
import { recordUsage, snapshot as usageSnapshot } from "@/usage-stats.js";
import type { CCEvent, CCRequestBody } from "@/translate/types.js";
import { formatSSE, formatSSEDone, formatAnthropicSSE } from "@/stream.js";
import { sendToCC, collectEvents, UpstreamError } from "@/upstream.js";
import { logger } from "@/logger.js";
import { getProxyVersion } from "@/version.js";
import {
  validateOpenAIChatRequest,
  validateAnthropicRequest,
  validateCountTokensRequest,
  ValidationError,
} from "@/translate/validation.js";
import type { AnthropicRequest, AnthropicSSERecord } from "@/translate/anthropic-types.js";

// ──────────────────────────────────────────
// Mutable server state
// ──────────────────────────────────────────

let config: Config;
let corsOrigin = "*";

// ──────────────────────────────────────────
// Request body parser
// ──────────────────────────────────────────

const MAX_BODY_BYTES = 10 * 1024 * 1024; // 10 MiB

export class BodyParseError extends Error {
  constructor(
    public readonly status: number,
    message: string,
  ) {
    super(message);
    this.name = "BodyParseError";
  }
}

function parseBody(req: http.IncomingMessage): Promise<unknown> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    let size = 0;
    let tooLarge = false;
    req.on("data", (chunk: Buffer) => {
      if (tooLarge) return;
      size += chunk.length;
      if (size > MAX_BODY_BYTES) {
        tooLarge = true;
        // Tear down the underlying socket so the client stops uploading the
        // rest of an oversized body. Without this the connection lingers
        // until the client finishes (or its own timeout fires) — wasting
        // bandwidth and a request slot.
        req.destroy();
        reject(new BodyParseError(413, "Request body too large"));
        return;
      }
      chunks.push(chunk);
    });
    req.on("end", () => {
      if (tooLarge) return;
      const raw = Buffer.concat(chunks).toString("utf-8");
      if (!raw) return resolve(null);
      try {
        resolve(JSON.parse(raw));
      } catch {
        reject(new BodyParseError(400, "Invalid JSON body"));
      }
    });
    req.on("error", (err) => {
      if (!tooLarge) reject(err);
    });
  });
}

// ──────────────────────────────────────────
// Auth
// ──────────────────────────────────────────

/**
 * Pure passthrough: the caller's own key travels with every request. There is
 * no stored fallback key — a request without one gets a 401.
 */
function extractApiKey(req: http.IncomingMessage): string | null {
  const auth = req.headers.authorization;
  if (auth) {
    const m = auth.match(/^Bearer\s+(.+)$/i);
    if (m) return m[1];
  }
  const xApiKey = req.headers["x-api-key"] as string | undefined;
  if (xApiKey) return xApiKey;
  return null;
}

// ──────────────────────────────────────────
// Response helpers
// ──────────────────────────────────────────

function sendJson(res: http.ServerResponse, status: number, data: unknown): void {
  // Client may have disconnected mid-request; never write to a dead socket.
  if (res.headersSent || res.writableEnded || res.destroyed) return;
  res.writeHead(status, {
    "Content-Type": "application/json",
    ...corsHeaders(),
  });
  res.end(JSON.stringify(data));
}

function sendOpenAIError(res: http.ServerResponse, status: number, message: string): void {
  sendJson(res, status, { error: { message, type: "proxy_error" } });
}

const ANTHROPIC_STATUS_ERROR_MAP: Record<number, string> = {
  400: "invalid_request_error",
  401: "authentication_error",
  403: "permission_error",
  404: "not_found_error",
  429: "rate_limit_error",
  500: "api_error",
  529: "overloaded_error",
};

function sendAnthropicError(
  res: http.ServerResponse,
  status: number,
  type: string,
  message: string,
): void {
  if (res.headersSent || res.writableEnded || res.destroyed) return;
  res.writeHead(status, { "Content-Type": "application/json", ...corsHeaders() });
  res.end(JSON.stringify({ type: "error", error: { type, message } }));
}

function corsHeaders(): Record<string, string> {
  const origin = corsOrigin;
  const headers: Record<string, string> = {
    "Access-Control-Allow-Methods": "GET, POST, OPTIONS",
    "Access-Control-Allow-Headers": "Content-Type, Authorization, x-api-key",
  };
  // Empty CORS_ORIGIN disables the header entirely (browser blocks cross-origin).
  if (origin) headers["Access-Control-Allow-Origin"] = origin;
  return headers;
}

// ──────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────

function abortOnClientDisconnect(res: http.ServerResponse): AbortController {
  const abort = new AbortController();
  // IncomingMessage.close marks a completed upload, not a lost response client.
  const onClose = (): void => {
    if (!res.writableEnded) abort.abort();
  };
  res.once("close", onClose);
  if (res.destroyed) onClose();
  return abort;
}

/**
 * Write a chunk to `res`, returning a Promise that resolves once the
 * underlying socket has drained (when backpressure applies). Returns
 * `false` if the response is no longer writable.
 */
function writeSSE(res: http.ServerResponse, chunk: string): Promise<boolean> {
  if (res.writableEnded || res.destroyed) return Promise.resolve(false);
  if (res.write(chunk)) return Promise.resolve(true);
  return new Promise((resolve) => {
    const settle = (writable: boolean): void => {
      res.off("drain", onDrain);
      res.off("close", onClose);
      res.off("error", onClose);
      resolve(writable);
    };
    const onDrain = (): void => settle(!res.writableEnded && !res.destroyed);
    const onClose = (): void => settle(false);
    res.once("drain", onDrain);
    res.once("close", onClose);
    res.once("error", onClose);
  });
}

/**
 * Drive the upstream CC stream through `encoder`, writing formatted SSE
 * records to `res`. Applies client-side backpressure (pauses the upstream
 * when `res` buffers fill), and isolates encoder errors so they terminate
 * the stream cleanly instead of crashing the process.
 */
async function pumpStream(
  stream: NodeJS.ReadableStream,
  res: http.ServerResponse,
  encode: (event: CCEvent) => string[],
  onEnd: () => string[],
  onError: (err: Error) => string[],
): Promise<void> {
  const writable = (chunk: string): Promise<boolean> => writeSSE(res, chunk);

  try {
    for await (const event of stream) {
      let chunks: string[];
      try {
        chunks = encode(event as unknown as CCEvent);
      } catch (err) {
        // Encoder blew up — turn it into a stream error so the catch below
        // handles it uniformly instead of crashing the proxy.
        (stream as Readable).destroy(err as Error);
        throw err;
      }
      for (const chunk of chunks) {
        if (!(await writable(chunk))) return;
      }
    }
    for (const chunk of onEnd()) {
      if (!(await writable(chunk))) return;
    }
  } catch (err) {
    logger.error("[stream] upstream streaming error:", (err as Error).message);
    for (const chunk of onError(err as Error)) {
      if (!(await writable(chunk))) return;
    }
  }
}

// ──────────────────────────────────────────
// Route handlers
// ──────────────────────────────────────────

/**
 * Whether an upstream failure means "CC did not recognize this model name".
 *
 * CC rewrites an unprefixed name to `anthropic:<name>` and answers 403 with
 * `Model/provider not recognized`. That is the one failure a catalog refresh
 * can actually fix, so it is also the only case worth retrying — matching on
 * the message keeps unrelated 403s (auth, quota) from triggering a retry.
 */
function isUnknownModelError(err: unknown): boolean {
  if (!(err instanceof UpstreamError) || err.statusCode !== 403) return false;
  return /model\/provider not recognized/i.test(err.message);
}

/**
 * Send a generation, retrying once if CC rejects the model name as unknown.
 *
 * A bare name the catalog has not learned yet is forwarded verbatim, and CC
 * answers 403 on the invented `anthropic:` prefix even when the model does
 * exist upstream. On exactly that failure we refresh the catalog and rebuild
 * the request so the name resolves to its full id. The happy path costs
 * nothing extra: no fetch, no rebuild, no second attempt.
 *
 * `buildBody` is a thunk because the retry must re-resolve the model after the
 * catalog changes.
 */
async function sendToCCWithModelDiscovery(
  buildBody: () => CCRequestBody,
  model: string,
  options: {
    apiBase: string;
    apiKey: string;
    ccVersion: string;
    timeoutMs: number;
    idleTimeoutMs: number;
  },
  signal: AbortSignal,
): Promise<NodeJS.ReadableStream> {
  try {
    return (await sendToCC(buildBody(), options, signal)).stream;
  } catch (err) {
    if (!isUnknownModelError(err)) throw err;
    const learned = await discoverModel(model, options.apiBase, options.apiKey);
    // Still unknown → the model genuinely doesn't exist; report the original.
    if (!learned) throw err;
    logger.info(`Model catalog learned "${model}"; retrying with the resolved id`);
    return (await sendToCC(buildBody(), options, signal)).stream;
  }
}

function handleHealth(_req: http.IncomingMessage, res: http.ServerResponse): void {
  sendJson(res, 200, {
    status: "ok",
    version: getProxyVersion(),
    // 自代理启动以来的累计用量/缓存统计（缓存量与缓存率）
    cache: usageSnapshot(),
  });
}

function handleModels(req: http.IncomingMessage, res: http.ServerResponse): void {
  const isAnthropic = req.headers["anthropic-version"] !== undefined;

  if (isAnthropic) {
    const catalog = getCatalog();
    const items = catalog.models;
    const data = {
      data: items.map((m) => ({
        id: m.id,
        type: "model" as const,
        display_name: m.displayName,
        created_at: new Date().toISOString(),
        max_input_tokens: null as number | null,
        max_tokens: null as number | null,
        capabilities: null,
      })),
      has_more: false,
      first_id: items.length > 0 ? items[0].id : null,
      last_id: items.length > 0 ? items[items.length - 1].id : null,
    };
    sendJson(res, 200, data);
    return;
  }

  const data = {
    object: "list",
    data: getCatalog().ids.map((id: string) => ({
      id,
      object: "model",
      created: Math.floor(Date.now() / 1000),
      owned_by: "commandcode",
    })),
  };
  sendJson(res, 200, data);
}

async function handleChatCompletions(
  req: http.IncomingMessage,
  res: http.ServerResponse,
): Promise<void> {
  let rawBody: unknown;
  try {
    rawBody = await parseBody(req);
  } catch (err) {
    const status = err instanceof BodyParseError ? err.status : 400;
    const message = err instanceof BodyParseError ? err.message : "Invalid JSON body";
    return sendOpenAIError(res, status, message);
  }

  let openAIReq;
  try {
    openAIReq = validateOpenAIChatRequest(rawBody);
  } catch (err) {
    if (err instanceof ValidationError) {
      return sendOpenAIError(res, 400, err.message);
    }
    return sendOpenAIError(res, 400, "Invalid request body");
  }

  const apiKey = extractApiKey(req);
  if (!apiKey) {
    return sendOpenAIError(res, 401, "Unauthorized");
  }

  const isStream = openAIReq.stream === true;
  const model = openAIReq.model ?? "default";
  const encoder = new OpenAIStreamEncoder(model);

  logger.info(`[Incoming Request] Model: ${model}`);
  logger.info(`[Incoming Request] Tools count: ${openAIReq.tools ? openAIReq.tools.length : 0}`);
  if (openAIReq.tools && openAIReq.tools.length > 0) {
    logger.info(
      `[Incoming Request] Tools list: ${openAIReq.tools.map((t) => t.function.name).join(", ")}`,
    );
  } else {
    logger.info(`[Incoming Request] No tools were sent by the client!`);
  }

  const abort = abortOnClientDisconnect(res);

  try {
    const stream = await sendToCCWithModelDiscovery(
      // Rebuilt per attempt so a retry re-resolves the model against the
      // refreshed catalog.
      () => toCCRequest(openAIReq),
      model,
      {
        apiBase: config.ccApiBase,
        apiKey,
        ccVersion: config.ccVersion,
        timeoutMs: config.upstreamTimeoutMs,
        idleTimeoutMs: config.idleTimeoutMs,
      },
      abort.signal,
    );

    if (isStream) {
      res.writeHead(200, {
        "Content-Type": "text/event-stream",
        "Cache-Control": "no-cache",
        Connection: "keep-alive",
        ...corsHeaders(),
      });

      await pumpStream(
        stream,
        res,
        (event) => encoder.emit(event).map((c) => formatSSE(c)),
        () => (encoder.finished ? [] : encoder.finishChunks("stop").map((c) => formatSSE(c))),
        // Stream-level error (TCP failure, idle timeout, encoder throw).
        // Always emit a uniform content+finish chunk pair via streamErrorChunks
        // — mixing a non-chunk `{error:...}` envelope with valid chunks
        // confused some clients (treating the envelope as a tool call named
        // "error", or failing JSON parse).
        (err) => encoder.streamErrorChunks(err).map((c) => formatSSE(c)),
      );
      // After pump completes, emit the [DONE] sentinel if we still can.
      // `writableEnded` only flips when end() is called — `res.destroyed`
      // catches the case where the client disconnected mid-stream and the
      // socket was torn down underneath us.
      if (!res.writableEnded && !res.destroyed) {
        res.write(formatSSEDone());
        res.end();
      }
      recordUsage(model, encoder.lastUsage);
      // The response abort signal covers both streaming and JSON clients.
    } else {
      const events = await collectEvents(stream);
      const response = buildNonStreamingResponse(events, model, encoder.id);
      sendJson(res, 200, response);
      recordUsage(model, extractUsage((events.find((e) => e.type === "finish")?.data ??
        {}) as Record<string, unknown>));
    }
  } catch (err) {
    handleUpstreamError(res, err, "openai");
  }
}

async function handleMessages(req: http.IncomingMessage, res: http.ServerResponse): Promise<void> {
  let rawBody: unknown;
  try {
    rawBody = await parseBody(req);
  } catch (err) {
    const status = err instanceof BodyParseError ? err.status : 400;
    const message = err instanceof BodyParseError ? err.message : "Invalid JSON body";
    return sendAnthropicError(
      res,
      status,
      status === 413 ? "api_error" : "invalid_request_error",
      message,
    );
  }

  let anthropicReq: AnthropicRequest;
  try {
    anthropicReq = validateAnthropicRequest(rawBody);
  } catch (err) {
    if (err instanceof ValidationError) {
      return sendAnthropicError(res, 400, "invalid_request_error", err.message);
    }
    return sendAnthropicError(res, 400, "invalid_request_error", "Invalid request body");
  }

  const apiKey = extractApiKey(req);
  if (!apiKey) {
    return sendAnthropicError(res, 401, "authentication_error", "Missing API key");
  }

  const isStream = anthropicReq.stream === true;
  const model = anthropicReq.model;

  const encoder = new AnthropicStreamEncoder(model);

  const abort = abortOnClientDisconnect(res);

  try {
    const stream = await sendToCCWithModelDiscovery(
      // Rebuilt per attempt so a retry re-resolves the model against the
      // refreshed catalog.
      () => anToCCRequest(anthropicReq),
      model,
      {
        apiBase: config.ccApiBase,
        apiKey,
        ccVersion: config.ccVersion,
        timeoutMs: config.upstreamTimeoutMs,
        idleTimeoutMs: config.idleTimeoutMs,
      },
      abort.signal,
    );

    if (isStream) {
      res.writeHead(200, {
        "Content-Type": "text/event-stream",
        "Cache-Control": "no-cache",
        Connection: "keep-alive",
        ...corsHeaders(),
      });

      await pumpStream(
        stream,
        res,
        (event) => encoder.emit(event).map((r) => formatAnthropicSSE(r.event, r.data)),
        () =>
          encoder.finished
            ? []
            : encoder.finishRecords("end_turn").map((r) => formatAnthropicSSE(r.event, r.data)),
        (err) => {
          const records: AnthropicSSERecord[] = [
            {
              event: "error",
              data: { type: "error", error: { type: "api_error", message: err.message } },
            },
          ];
          if (!encoder.finished) records.push(...encoder.finishRecords("end_turn"));
          return records.map((r) => formatAnthropicSSE(r.event, r.data));
        },
      );
      if (!res.writableEnded && !res.destroyed) res.end();
      recordUsage(model, encoder.lastUsage);
      // The response abort signal already covers mid-stream disconnects.
    } else {
      const events = await collectEvents(stream);
      const response = buildAnthropicResponse(events, model, encoder.messageId);
      res.writeHead(200, { "Content-Type": "application/json", ...corsHeaders() });
      res.end(JSON.stringify(response));
      recordUsage(model, extractUsage((events.find((e) => e.type === "finish")?.data ??
        {}) as Record<string, unknown>));
    }
  } catch (err) {
    handleUpstreamError(res, err, "anthropic");
  }
}

async function handleCountTokens(
  req: http.IncomingMessage,
  res: http.ServerResponse,
): Promise<void> {
  let rawBody: unknown;
  try {
    rawBody = await parseBody(req);
  } catch (err) {
    const status = err instanceof BodyParseError ? err.status : 400;
    const message = err instanceof BodyParseError ? err.message : "Invalid JSON body";
    return sendAnthropicError(
      res,
      status,
      status === 413 ? "api_error" : "invalid_request_error",
      message,
    );
  }

  let body: Record<string, unknown>;
  try {
    body = validateCountTokensRequest(rawBody);
  } catch (err) {
    return sendAnthropicError(
      res,
      400,
      "invalid_request_error",
      err instanceof ValidationError ? err.message : "Invalid request body",
    );
  }

  const parts: string[] = [];
  if (typeof body.system === "string") parts.push(body.system);
  else if (Array.isArray(body.system)) {
    for (const b of body.system as { text?: string }[]) {
      if (b.text) parts.push(b.text);
    }
  }
  const msgs = body.messages as { content?: unknown }[] | undefined;
  if (msgs) {
    for (const msg of msgs) {
      parts.push(typeof msg.content === "string" ? msg.content : JSON.stringify(msg.content));
    }
  }
  const tools = body.tools as
    | { name?: string; description?: string; input_schema?: unknown }[]
    | undefined;
  if (tools) {
    for (const t of tools) {
      parts.push(t.name ?? "", t.description ?? "", JSON.stringify(t.input_schema ?? {}));
    }
  }

  const allText = parts.join("");
  let cjk = 0;
  let nonCjk = 0;
  for (const ch of allText) {
    const code = ch.codePointAt(0) ?? 0;
    if (
      (code >= 0x4e00 && code <= 0x9fff) ||
      (code >= 0x3040 && code <= 0x309f) ||
      (code >= 0x30a0 && code <= 0x30ff) ||
      (code >= 0xac00 && code <= 0xd7af)
    ) {
      cjk++;
    } else {
      nonCjk++;
    }
  }

  const estimated = Math.ceil(cjk + nonCjk / 4);

  sendJson(res, 200, { input_tokens: estimated });
}

// ──────────────────────────────────────────
// Error handling
// ──────────────────────────────────────────

function handleUpstreamError(
  res: http.ServerResponse,
  err: unknown,
  format: "openai" | "anthropic",
): void {
  if (format === "anthropic") {
    if (err instanceof UpstreamError) {
      const status = err.statusCode >= 400 && err.statusCode < 500 ? err.statusCode : 502;
      const type = ANTHROPIC_STATUS_ERROR_MAP[status] ?? "api_error";
      sendAnthropicError(res, status, type, err.message);
    } else {
      sendAnthropicError(res, 502, "api_error", (err as Error).message);
    }
    return;
  }

  if (err instanceof UpstreamError) {
    const status = err.statusCode >= 400 && err.statusCode < 500 ? err.statusCode : 502;
    sendOpenAIError(res, status, err.message);
  } else {
    sendOpenAIError(res, 502, (err as Error).message);
  }
}

// ──────────────────────────────────────────
// Server factory
// ──────────────────────────────────────────

interface RouteEntry {
  method: string;
  path: string;
  handler: (req: http.IncomingMessage, res: http.ServerResponse, url: URL) => void | Promise<void>;
}

export function createServer(cfg: Config): http.Server {
  config = cfg;
  corsOrigin = cfg.corsOrigin;

  // No startup catalog refresh: there is no stored key to fetch with. The
  // catalog learns models lazily via sendToCCWithModelDiscovery when CC
  // rejects a name, keyed by the caller's own credentials.

  const routes: RouteEntry[] = [
    { method: "GET", path: "/health", handler: handleHealth },
    { method: "GET", path: "/v1/models", handler: handleModels },
    { method: "POST", path: "/v1/chat/completions", handler: handleChatCompletions },
    { method: "POST", path: "/v1/messages", handler: handleMessages },
    { method: "POST", path: "/v1/messages/count_tokens", handler: handleCountTokens },
  ];

  const server = http.createServer((req, res) => {
    if (req.method === "OPTIONS") {
      res.writeHead(204, corsHeaders());
      return res.end();
    }

    const parsedUrl = new URL(req.url ?? "/", `http://${req.headers.host ?? "localhost"}`);
    const pathname = parsedUrl.pathname;

    const route = routes.find((r) => r.method === req.method && r.path === pathname);

    if (!route) {
      const isAnthropic = req.headers["anthropic-version"] !== undefined;
      if (isAnthropic) {
        return sendAnthropicError(res, 404, "not_found_error", "Not found");
      }
      return sendJson(res, 404, { error: "Not found" });
    }

    try {
      const result = route.handler(req, res, parsedUrl);
      if (result instanceof Promise) {
        result.catch((err) => {
          logger.error("[route] handler promise error:", err);
          if (!res.headersSent) {
            sendJson(res, 500, { error: "Internal server error" });
          }
        });
      }
    } catch (err) {
      logger.error("[route] handler error:", err);
      if (!res.headersSent) {
        sendJson(res, 500, { error: "Internal server error" });
      }
    }
  });

  return server;
}
