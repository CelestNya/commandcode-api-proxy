// 中途失败到底能不能让客户端重试？实测 AI SDK 的重试判定。
//
// 背景：cc-proxy 的 a3864f6 假设"把流内错误标成 overloaded_error 就能触发下游重试"。
// 但生产日志显示：两次流内 overloaded_error 都被判 retryable:false，整轮死掉；
// 跨 7 天所有 retryable:true 的记录全是传输层错误（没有响应体）。
//
// 本探针直接数请求次数来判定重试是否真的发生：
//   A message_start → error → message_stop（当前代理形状）
//   B error → message_stop（error 是首个事件）
//   C message_start → 正文 → error → message_stop（已吐内容）
//
// 用法：node conformance/client-probes/retry-classification.mjs

import { createAnthropic } from "@ai-sdk/anthropic";
import { createOpenAICompatible } from "@ai-sdk/openai-compatible";
import { streamText } from "ai";
import http from "node:http";
import { mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const OUT = path.join(HERE, "observed", "retry-classification.json");

const sse = (records) =>
  records.map(([ev, data]) => `event: ${ev}\ndata: ${JSON.stringify(data)}\n\n`).join("");

const oaiSSE = (chunks) =>
  chunks.map((c) => `data: ${JSON.stringify(c)}\n\n`).join("") + "data: [DONE]\n\n";

const chunk = (id, delta, finish = null) => ({
  id,
  object: "chat.completion.chunk",
  created: 1,
  model: "m",
  choices: [{ index: 0, delta, finish_reason: finish }],
});

const MSG_START = (id) => [
  "message_start",
  {
    type: "message_start",
    message: { id, type: "message", role: "assistant", model: "m", content: [], stop_reason: null, stop_sequence: null, usage: { input_tokens: 5, output_tokens: 0 } },
  },
];
const ERR = (type, msg) => ["error", { type: "error", error: { type, message: msg } }];
const MSG_DELTA = (stop) => [
  "message_delta",
  { type: "message_delta", delta: { stop_reason: stop, stop_sequence: null }, usage: { output_tokens: 0 } },
];
const STOP = ["message_stop", { type: "message_stop" }];

const VARIANTS = {
  "A-message_start-then-error": (id) => sse([MSG_START(id), ERR("overloaded_error", "[upstream-error] idle timeout"), STOP]),
  // 首块探测读的是流的**前两个事件**：先 message_start 再 error 与直接 error 不同
  "B-error-first": (_id) => sse([ERR("overloaded_error", "[upstream-error] idle timeout"), STOP]),
  "C-content-then-error": (id) => sse([
    MSG_START(id),
    ["content_block_start", { type: "content_block_start", index: 0, content_block: { type: "text", text: "" } }],
    ["content_block_delta", { type: "content_block_delta", index: 0, delta: { type: "text_delta", text: "partial text" } }],
    ERR("overloaded_error", "[upstream-error] idle timeout"),
    STOP,
  ]),
  // 代理当前的真实形状：error 之后还跟 finishRecords("end_turn") 的收尾三件套
  "C2-content-then-error-then-terminators": (id) => sse([
    MSG_START(id),
    ["content_block_start", { type: "content_block_start", index: 0, content_block: { type: "text", text: "" } }],
    ["content_block_delta", { type: "content_block_delta", index: 0, delta: { type: "text_delta", text: "partial text" } }],
    ["content_block_stop", { type: "content_block_stop", index: 0 }],
    ERR("overloaded_error", "[upstream-error] idle timeout"),
    MSG_DELTA("end_turn"),
    STOP,
  ]),
  "D-http-529": () => null, // 特殊处理：非 200
  // 非 overloaded 的 in-band 类型，用来确认「类型完全不影响重试」这个结论
  "H-message_start-then-api_error": (id) => sse([MSG_START(id), ERR("api_error", "[upstream-error] idle timeout"), STOP]),
  "I-message_start-then-rate_limit_error": (id) => sse([MSG_START(id), ERR("rate_limit_error", "slow down"), STOP]),
  // OpenAI 方言：代理发的是纯 {error:{...}} 信封（code: network_error）
  "E-openai-envelope-after-content": () =>
    oaiSSE([
      chunk("e", { role: "assistant" }),
      chunk("e", { content: "partial text" }),
      { error: { message: "[upstream-error] idle timeout", type: "upstream_error", code: "network_error" } },
    ]),
  "F-openai-envelope-first": () =>
    oaiSSE([{ error: { message: "[upstream-error] idle timeout", type: "upstream_error", code: "network_error" } }]),
  "G-openai-http-500": () => null, // 特殊处理
  // HTTP 层状态码矩阵：能否重试是 SDK 写死的分类
  "J-http-429": () => null,
  "K-http-401": () => null,
  "L-http-403": () => null,
  "M-http-400": () => null,
};

// HTTP 状态码用例 → 返回的 JSON 体（按各协议的错误形状）
const HTTP_CASES = {
  "D-http-529": { status: 529, body: { type: "error", error: { type: "overloaded_error", message: "overloaded" } } },
  "G-openai-http-500": { status: 500, body: { error: { message: "server exploded", type: "proxy_error" } } },
  "J-http-429": { status: 429, body: { type: "error", error: { type: "rate_limit_error", message: "slow down" } } },
  "K-http-401": { status: 401, body: { type: "error", error: { type: "authentication_error", message: "bad key" } } },
  "L-http-403": { status: 403, body: { type: "error", error: { type: "permission_error", message: "not recognized" } } },
  "M-http-400": { status: 400, body: { type: "error", error: { type: "invalid_request_error", message: "bad field" } } },
};

const OPENAI_VARIANTS = new Set([
  "E-openai-envelope-after-content",
  "F-openai-envelope-first",
  "G-openai-http-500",
]);
const results = [];

function serve(variantName) {
  let requests = 0;
  return new Promise((resolve) => {
    const srv = http.createServer((_req, res) => {
      requests += 1;
      const httpCase = HTTP_CASES[variantName];
      if (httpCase) {
        res.writeHead(httpCase.status, { "Content-Type": "application/json" });
        res.end(JSON.stringify(httpCase.body));
        return;
      }
      res.writeHead(200, { "Content-Type": "text/event-stream" });
      res.end(VARIANTS[variantName](`msg_${requests}`));
    });
    srv.listen(0, "127.0.0.1", () => resolve({ srv, port: srv.address().port, count: () => requests }));
  });
}

async function run(variantName) {
  const { srv, port, count } = await serve(variantName);
  const openai = OPENAI_VARIANTS.has(variantName);
  const provider = openai
    ? createOpenAICompatible({ name: "probe", baseURL: `http://127.0.0.1:${port}/v1` })
    : createAnthropic({ apiKey: "k", baseURL: `http://127.0.0.1:${port}/v1` });
  const out = { variant: variantName, dialect: openai ? "openai" : "anthropic", upstreamRequests: 0, text: "", errorPart: false, finish: null, threw: null };
  try {
    const model = openai ? provider.chatModel("m") : provider("m");
    const r = streamText({ model, prompt: "hi", maxRetries: 3 });
    for await (const part of r.fullStream) {
      if (part.type === "text-delta") out.text += part.text;
      else if (part.type === "error") out.errorPart = true;
      else if (part.type === "finish") out.finish = part.finishReason;
    }
  } catch (e) {
    out.threw = `${e?.constructor?.name}: ${String(e?.message ?? e).slice(0, 90)}`;
  }
  out.upstreamRequests = count();
  out.retried = out.upstreamRequests > 1;
  srv.close();
  results.push(out);
  console.log(JSON.stringify(out, null, 1));
}

console.log("### 重试判定实测（maxRetries: 3 —— 若上游被请求 >1 次即证明重试发生）\n");
for (const v of Object.keys(VARIANTS)) {
  await run(v);
  console.log();
}

mkdirSync(path.dirname(OUT), { recursive: true });
writeFileSync(
  OUT,
  JSON.stringify(
    {
      note: "AI SDK 重试判定实测。upstreamRequests>1 才代表客户端真的重试了。",
      sdk: "ai + @ai-sdk/anthropic",
      results,
    },
    null,
    2,
  ),
);
console.log(`wrote ${path.relative(process.cwd(), OUT)}`);
