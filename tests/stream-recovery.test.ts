// 流级失败（idle 超时 / TCP 断连）的两条路径，实测驱动。
//
// 背景：下游客户端不会重试"响应头已发出之后"的失败——这是 SDK 层的硬限制，
// 任何 error.type 都一样（见 conformance/client-probes/retry-classification.mjs）。
// 所以代理必须自己扛：
//
//   A. 还没向客户端写出任何内容 → 上游可以安全重发（客户端什么都没看见）。
//   B. 已经写出内容 → 不能重发（会重复），只能如实报告失败。
//
// 两条路径都必须满足 Anthropic 协议：message_start 是第一条记录，失败时不得
// 伪造 message_delta(end_turn) 把错误洗成"正常说完"。
import { afterEach, describe, expect, it, vi } from "vitest";
import http from "node:http";
import { once } from "node:events";
import { createServer } from "@/server.js";
import type { CCEvent } from "@/translate/types.js";

const fixtureBase = "https://upstream.invalid";
const servers: http.Server[] = [];
afterEach(async () => {
  vi.restoreAllMocks();
  for (const server of servers.splice(0)) {
    server.closeAllConnections();
    await new Promise<void>((resolve) => server.close(() => resolve()));
  }
});

async function listen(idleTimeoutMs = 0) {
  const server = createServer({
    host: "127.0.0.1",
    port: 0,
    apiKey: null,
    ccApiBase: fixtureBase,
    ccVersion: "0.0.0",
    logLevel: "error",
    corsOrigin: "",
    upstreamTimeoutMs: 5000,
    idleTimeoutMs,
  });
  servers.push(server);
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  return (server.address() as { port: number }).port;
}

function post(port: number, path: string, body: unknown) {
  const req = http.request({
    host: "127.0.0.1",
    port,
    path,
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Authorization: "Bearer synthetic-fixture-only",
      "anthropic-version": "2023-06-01",
    },
  });
  req.end(JSON.stringify(body));
  return req;
}

/** 读取完整 SSE 响应，返回有序的 [event, data] 列表。 */
async function readSSE(port: number, path: string, body: unknown) {
  const [res] = (await once(post(port, path, body), "response")) as [http.IncomingMessage];
  let text = "";
  for await (const chunk of res) text += chunk as string;
  const records: Array<{ event: string; data: Record<string, unknown> }> = [];
  for (const block of text.split("\n\n")) {
    const evLine = block.split("\n").find((l) => l.startsWith("event: "));
    const dataLine = block.split("\n").find((l) => l.startsWith("data: "));
    if (!dataLine) continue;
    const payload = dataLine.slice(6);
    if (payload === "[DONE]") {
      records.push({ event: "done", data: {} });
      continue;
    }
    try {
      records.push({ event: evLine ? evLine.slice(7) : "data", data: JSON.parse(payload) });
    } catch {
      records.push({ event: "unparseable", data: { raw: payload } });
    }
  }
  return { status: res.statusCode, records };
}

const bytes = (events: CCEvent[]) =>
  new TextEncoder().encode(events.map((e) => JSON.stringify(e)).join("\n") + "\n");

/** 上游每次被调用时返回下一个响应；记录调用次数。 */
function scriptedFetch(responses: Array<() => Response>) {
  const calls = { count: 0 };
  vi.spyOn(globalThis, "fetch").mockImplementation(async (url) => {
    if (String(url) !== `${fixtureBase}/alpha/generate`) throw new Error("Unexpected fixture URL");
    const factory = responses[Math.min(calls.count, responses.length - 1)];
    calls.count += 1;
    return factory();
  });
  return calls;
}

/** 一个立即吐出若干事件、然后按指令失败的流。 */
function streamThatThenFails(events: CCEvent[], failure: "destroy" | "hang" | "close") {
  return new Response(
    new ReadableStream<Uint8Array>({
      start(controller) {
        controller.enqueue(bytes(events));
        if (failure === "hang") return; // 永不关闭：交给 idle 超时
        if (failure === "close") {
          controller.close();
          return;
        }
        // destroy：以错误终止流（模拟 TCP RST）
        setTimeout(() => controller.error(new Error("simulated TCP RST")), 5);
      },
    }),
  );
}

const anthropicBody = {
  model: "fixture-model",
  max_tokens: 16,
  messages: [{ role: "user", content: "fixture" }],
  stream: true,
};

describe("流级失败：零内容时重发上游", () => {
  it("重发一次并完整交付第二次的内容（客户端无感）", async () => {
    const calls = scriptedFetch([
      // 第一次：吐出 start 后立刻断（尚无任何内容产出）
      () => streamThatThenFails([{ type: "start", data: {} }], "destroy"),
      // 第二次：正常完成
      () =>
        new Response(
          bytes([
            { type: "start", data: {} },
            { type: "text-delta", data: { text: "recovered" } },
            { type: "finish", data: { finishReason: "stop" } },
          ]),
        ),
    ]);
    const port = await listen();
    const { records } = await readSSE(port, "/v1/messages", anthropicBody);

    expect(calls.count).toBe(2);
    const text = records
      .filter((r) => r.event === "content_block_delta")
      .map((r) => (r.data.delta as { text?: string })?.text ?? "")
      .join("");
    expect(text).toBe("recovered");
    // 成功的流里不能出现 error
    expect(records.some((r) => r.event === "error")).toBe(false);
    // 且必须是正常收尾
    const last = records.filter((r) => r.event === "message_delta").at(-1);
    expect((last?.data.delta as { stop_reason?: string })?.stop_reason).toBe("end_turn");
  });

  it("重发仍然失败时，如实报错而不是伪造成功", async () => {
    const calls = scriptedFetch([
      () => streamThatThenFails([{ type: "start", data: {} }], "destroy"),
      () => streamThatThenFails([{ type: "start", data: {} }], "destroy"),
    ]);
    const port = await listen();
    const { records } = await readSSE(port, "/v1/messages", anthropicBody);

    // 只重发一次，不无限循环
    expect(calls.count).toBe(2);
    const events = records.map((r) => r.event);
    // 协议：message_start 必须是第一条
    expect(events[0]).toBe("message_start");
    expect(events).toContain("error");
    expect(events).toContain("message_stop");
    // 不得伪造成功收尾
    const deltas = records.filter((r) => r.event === "message_delta");
    expect(deltas).toHaveLength(0);
  });

  it("重发时固定 threadId，不换会话（避免多计费）", async () => {
    const bodies: string[] = [];
    vi.spyOn(globalThis, "fetch").mockImplementation(async (url, init) => {
      if (String(url) !== `${fixtureBase}/alpha/generate`) throw new Error("Unexpected fixture URL");
      bodies.push(String(init?.body ?? ""));
      if (bodies.length === 1) return streamThatThenFails([{ type: "start", data: {} }], "destroy");
      return new Response(
        bytes([
          { type: "start", data: {} },
          { type: "text-delta", data: { text: "ok" } },
          { type: "finish", data: { finishReason: "stop" } },
        ]),
      );
    });
    const port = await listen();
    await readSSE(port, "/v1/messages", anthropicBody);

    expect(bodies).toHaveLength(2);
    const tid = (s: string) => (JSON.parse(s) as { threadId?: string }).threadId;
    expect(tid(bodies[1])).toBe(tid(bodies[0]));
  });
});

describe("流级失败：已有内容时不重发、不伪装成功", () => {
  it("吐出过正文后失败：error + message_stop，不补 message_delta", async () => {
    const calls = scriptedFetch([
      () =>
        streamThatThenFails(
          [
            { type: "start", data: {} },
            { type: "text-delta", data: { text: "partial answer" } },
          ],
          "destroy",
        ),
    ]);
    const port = await listen();
    const { records } = await readSSE(port, "/v1/messages", anthropicBody);

    // 已投递内容 → 绝不重发
    expect(calls.count).toBe(1);
    const events = records.map((r) => r.event);
    expect(events[0]).toBe("message_start");
    expect(events).toContain("error");
    expect(events.at(-1)).toBe("message_stop");
    // 关键：不得有 message_delta —— 那会把 finishReason 洗成 "stop"
    expect(records.some((r) => r.event === "message_delta")).toBe(false);
    // 已产出的正文必须保留
    const text = records
      .filter((r) => r.event === "content_block_delta")
      .map((r) => (r.data.delta as { text?: string })?.text ?? "")
      .join("");
    expect(text).toBe("partial answer");
  });

  it("idle 超时（上游挂死）同样按失败收尾，不伪造成功", async () => {
    scriptedFetch([
      () => streamThatThenFails([{ type: "start", data: {} }], "hang"),
    ]);
    const port = await listen(120); // 120ms 无数据即超时
    const { records } = await readSSE(port, "/v1/messages", anthropicBody);

    const events = records.map((r) => r.event);
    expect(events[0]).toBe("message_start");
    expect(events).toContain("error");
    expect(events.at(-1)).toBe("message_stop");
    expect(records.some((r) => r.event === "message_delta")).toBe(false);
  });

  it("OpenAI 方言：信封里不得混入声称正常结束的 finish chunk", async () => {
    scriptedFetch([
      () =>
        streamThatThenFails(
          [
            { type: "start", data: {} },
            { type: "text-delta", data: { text: "partial" } },
          ],
          "destroy",
        ),
    ]);
    const port = await listen();
    const { records } = await readSSE(port, "/v1/chat/completions", {
      ...anthropicBody,
      model: "deepseek/deepseek-v4-flash",
    });

    const envelopes = records.filter((r) => "error" in r.data);
    expect(envelopes).toHaveLength(1);
    const finishChunks = records.filter(
      (r) => ((r.data.choices as Array<{ finish_reason?: string }>) ?? [])[0]?.finish_reason,
    );
    expect(finishChunks).toHaveLength(0);
  });
});
