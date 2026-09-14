import type { CCEvent } from "@/translate/types.js";

/**
 * Result from parsing a single line of CC NDJSON data.
 */
export interface ParsedChunk {
  type: "event" | "done" | "ping" | "unknown";
  event?: CCEvent;
}

/**
 * Parse a single NDJSON line from the CC stream.
 */
export function parseCCLine(line: string): ParsedChunk {
  const trimmed = line.trim();

  if (!trimmed || trimmed === "") return { type: "ping" };

  // The CC API sends JSON lines prefixed with "data: "
  // Handle both with and without prefix
  const dataStr = trimmed.startsWith("data: ") ? trimmed.slice(6) : trimmed;

  if (dataStr === "[DONE]") return { type: "done" };

  try {
    const parsed = JSON.parse(dataStr);

    // CC API format 1 (flat): { type: "text-delta", id: "txt-0", text: "4" }
    // CC API format 2 (nested): { type: "text-delta", data: { text: "Hello" } }
    // Merge all non-type fields into data.
    if (parsed && typeof parsed === "object" && parsed.type) {
      const { type, id: _id, ...rest } = parsed;
      const data = parsed.data && typeof parsed.data === "object" ? parsed.data : rest;
      return {
        type: "event",
        event: { type, data },
      };
    }

    return { type: "unknown" };
  } catch {
    // If we can't parse as JSON, it might be a ping or keepalive
    return { type: "ping" };
  }
}

/**
 * Format an object as an SSE message string.
 */
export function formatSSE(data: object): string {
  return `data: ${JSON.stringify(data)}\n\n`;
}

/**
 * Format the [DONE] signal for non-streaming mode.
 */
export function formatSSEDone(): string {
  return "data: [DONE]\n\n";
}

export function formatAnthropicSSE(eventType: string, data: unknown): string {
  // 客户端对每条记录按 type 做联合判别，缺 type 的记录（如曾经的
  // message_stop 空载荷）会让整条流验证失败，因此这里兜底补齐。
  let payload = data as Record<string, unknown>;
  if (!payload || typeof payload !== "object" || payload.type === undefined) {
    payload = { ...(payload as object), type: eventType };
  }
  return `event: ${eventType}\ndata: ${JSON.stringify(payload)}\n\n`;
}

/**
 * Raised when the upstream reports a failure through an in-band `error` event
 * while nothing has been written downstream yet.
 *
 * It travels as an exception so it takes the same recovery path as a transport
 * failure: the proxy can re-send the request and the client sees one clean,
 * successful response. Reported as `[upstream-error]` because the failure came
 * from the upstream's own judgement, not from the transport.
 */
export class UpstreamEventError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "UpstreamEventError";
  }
}

/**
 * Tag a mid-stream failure with its real origin.
 *
 * Every mid-stream failure is reported to the downstream as
 * `overloaded_error` because that is the only in-band error type its
 * classifier hardcodes as retryable; any other type forfeits the whole retry
 * budget. Downstream classifies on the error *type*, never on the message,
 * so tagging is free — and it keeps the true cause visible in logs and in the
 * UI's error detail, which the type alone no longer conveys.
 */
export function tagStreamError(err: Error): string {
  if (err.name === "IdleTimeoutError") return `[idle-timeout] ${err.message}`;
  if (err.name === "UpstreamEventError") return `[upstream-error] ${err.message}`;
  const code = (err as NodeJS.ErrnoException).code;
  if (code === "UND_ERR_SOCKET" || /terminated|socket hang up/i.test(err.message)) {
    return `[connection-reset] ${err.message}`;
  }
  if (/inconsistent/i.test(err.message)) return `[bad-upstream-data] ${err.message}`;
  if (/client disconnected/i.test(err.message)) return `[client-gone] ${err.message}`;
  return `[stream-error] ${err.message}`;
}
