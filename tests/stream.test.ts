import { describe, it, expect } from "vitest";
import { parseCCLine, formatSSE, formatSSEDone, formatAnthropicSSE, tagStreamError } from "@/stream.js";

describe("parseCCLine", () => {
  it("parses a CC event with data: prefix", () => {
    const result = parseCCLine('data: {"type":"text-delta","data":{"text":"Hello"}}');
    expect(result.type).toBe("event");
    expect(result.event?.type).toBe("text-delta");
    expect(result.event?.data.text).toBe("Hello");
  });

  it("parses a CC event without prefix", () => {
    const result = parseCCLine('{"type":"start","data":{"model":"test"}}');
    expect(result.type).toBe("event");
    expect(result.event?.type).toBe("start");
  });

  it("returns ping for empty lines", () => {
    expect(parseCCLine("").type).toBe("ping");
    expect(parseCCLine("  ").type).toBe("ping");
  });

  it("returns done for [DONE]", () => {
    const result = parseCCLine("data: [DONE]");
    expect(result.type).toBe("done");
  });

  it("returns ping for unparseable lines", () => {
    const result = parseCCLine("data: not-json");
    expect(result.type).toBe("ping");
  });

  it("handles finish event with usage data", () => {
    const line =
      'data: {"type":"finish","data":{"finishReason":"stop","usage":{"promptTokens":10,"completionTokens":20,"totalTokens":30}}}';
    const result = parseCCLine(line);
    expect(result.type).toBe("event");
    expect(result.event?.type).toBe("finish");
    expect(result.event?.data.usage).toEqual({
      promptTokens: 10,
      completionTokens: 20,
      totalTokens: 30,
    });
  });

  it("handles error events", () => {
    const result = parseCCLine('{"type":"error","data":{"message":"Rate limit exceeded"}}');
    expect(result.type).toBe("event");
    expect(result.event?.type).toBe("error");
    expect(result.event?.data.message).toBe("Rate limit exceeded");
  });
});

describe("formatSSE", () => {
  it("formats an object as SSE data", () => {
    const sse = formatSSE({ key: "value" });
    expect(sse).toBe('data: {"key":"value"}\n\n');
  });

  it("formats nested objects correctly", () => {
    const obj = { id: "123", choices: [{ delta: { content: "hi" } }] };
    const sse = formatSSE(obj);
    expect(sse).toContain('"id":"123"');
    expect(sse).toContain('"content":"hi"');
    expect(sse.endsWith("\n\n")).toBe(true);
  });
});

describe("formatSSEDone", () => {
  it("returns [DONE] signal", () => {
    expect(formatSSEDone()).toBe("data: [DONE]\n\n");
  });
});

describe("formatAnthropicSSE", () => {
  it("emits event + data lines", () => {
    const result = formatAnthropicSSE("message_start", {
      type: "message_start",
      message: { id: "msg_1", model: "claude" },
    });
    expect(result).toContain("event: message_start");
    expect(result).toContain("data: ");
    expect(result).toContain("\n\n");
  });

  // Regression: message_stop used to be special-cased to `data: {}`. The SDK
  // validates every record against the same union keyed on `type`, so an empty
  // payload fails as "No matching discriminator" and kills the turn exactly at
  // stream end — which is also why every tool call died before executing.
  it("message_stop carries its type discriminator", () => {
    const result = formatAnthropicSSE("message_stop", { type: "message_stop" });
    expect(result).toContain("event: message_stop");
    expect(result).toContain('data: {"type":"message_stop"}');
  });

  it("content_block_delta formats correctly", () => {
    const result = formatAnthropicSSE("content_block_delta", {
      type: "content_block_delta",
      index: 0,
      delta: { type: "text_delta", text: "Hello" },
    });
    expect(result).toContain("event: content_block_delta");
    const parsed = JSON.parse(result.split("data: ")[1].trimEnd());
    expect(parsed.delta.text).toBe("Hello");
  });
});

// Every mid-stream failure goes out as `overloaded_error` — the only in-band
// type the downstream classifier hardcodes as retryable. The message tag is
// therefore the sole remaining signal of what actually broke, so its mapping
// is part of the contract, not cosmetic.
describe("tagStreamError", () => {
  it("labels an idle timeout", () => {
    const err = new Error("CC upstream idle timeout: no data for 120000ms");
    err.name = "IdleTimeoutError";
    expect(tagStreamError(err)).toMatch(/^\[idle-timeout\]/);
    expect(tagStreamError(err)).toContain("no data for 120000ms");
  });

  it("labels an undici connection reset (terminated with code)", () => {
    const err = new Error("terminated") as NodeJS.ErrnoException;
    err.code = "UND_ERR_SOCKET";
    expect(tagStreamError(err)).toMatch(/^\[connection-reset\]/);
  });

  it("labels a bare 'terminated' with no code", () => {
    expect(tagStreamError(new Error("terminated"))).toMatch(/^\[connection-reset\]/);
  });

  it("labels inconsistent upstream tool data", () => {
    expect(tagStreamError(new Error("Inconsistent upstream tool arguments"))).toMatch(
      /^\[bad-upstream-data\]/,
    );
  });

  it("labels a client-initiated disconnect", () => {
    expect(tagStreamError(new Error("Client disconnected"))).toMatch(/^\[client-gone\]/);
  });

  it("falls back to a generic tag", () => {
    expect(tagStreamError(new Error("something else"))).toBe("[stream-error] something else");
  });
});
