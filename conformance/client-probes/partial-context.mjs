// 断流之后，客户端手上还剩什么 —— 以及"继续这个会话"能不能走通。
//
// 背景：spec §2.4.1 原有一句论证 —— "已吐出半截内容再重试会让用户看到重复内容"，
// 据此推出"中途错误不该重试"（结论对）但把部分输出当成了必须丢弃的东西。实测
// 相反：两个 SDK 都把已吐出的正文留在消息里，客户端下一轮会原样带上。所以断流
// 的真实语义是**截断**，不是**作废**。
//
// 本文件是三组探针里最核心的一组：它决定"部分输出"在客户端侧的生命周期。
//   A 携带半截正文的错误信封 → 客户端保留了什么
//   B 把断流当截断收尾     → 客户端保留下来的形态是否等价
//   C 上游能否被续写       → 半截入历史 + 继续指令，是否从断点接上
//
// 用法：node conformance/client-probes/partial-context.mjs
// 输出：conformance/client-probes/observed/partial-context.json

import Anthropic from "@anthropic-ai/sdk";
import { createOpenAICompatible } from "@ai-sdk/openai-compatible";
import { streamText } from "ai";
import http from "node:http";
import { mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const OUT = path.join(HERE, "observed", "partial-context.json");

// ── 一个只会照本宣科的 SSE 服务器 ──────────────────────────────────────────

function serve(raw) {
  return new Promise((resolve) => {
    const srv = http.createServer((_req, res) => {
      res.writeHead(200, { "Content-Type": "text/event-stream" });
      res.write(raw);
      res.end();
    });
    srv.listen(0, "127.0.0.1", () => resolve({ srv, port: srv.address().port }));
  });
}

const openaiSSE = (chunks) =>
  chunks.map((c) => `data: ${JSON.stringify(c)}\n\n`).join("") + "data: [DONE]\n\n";

const anthropicSSE = (records) =>
  records.map(([ev, data]) => `event: ${ev}\ndata: ${JSON.stringify(data)}\n\n`).join("");

const chunk = (id, delta, finish = null) => ({
  id,
  object: "chat.completion.chunk",
  created: 1,
  model: "m",
  choices: [{ index: 0, delta, finish_reason: finish }],
});

const msgStart = (id) => [
  "message_start",
  {
    type: "message_start",
    message: {
      id,
      type: "message",
      role: "assistant",
      model: "m",
      content: [],
      stop_reason: null,
      stop_sequence: null,
      usage: { input_tokens: 5, output_tokens: 1 },
    },
  },
];

const results = [];
const record = (r) => {
  results.push(r);
  console.log(JSON.stringify(r, null, 1));
};

// ── OpenAI 方言（Vercel AI SDK） ───────────────────────────────────────────

async function openaiCase(label, chunks) {
  const { srv, port } = await serve(openaiSSE(chunks));
  const provider = createOpenAICompatible({
    name: "probe",
    baseURL: `http://127.0.0.1:${port}/v1`,
  });
  const out = { dialect: "openai", label, streamedText: "", finish: null, errorPart: false, kept: null, threw: null };
  try {
    const r = streamText({ model: provider.chatModel("m"), prompt: "hi" });
    for await (const part of r.fullStream) {
      if (part.type === "text-delta") out.streamedText += part.text;
      else if (part.type === "error") out.errorPart = true;
      else if (part.type === "finish") out.finish = part.finishReason;
    }
    const resp = await r.response;
    out.kept = resp.messages.map((m) => ({
      role: m.role,
      content: typeof m.content === "string" ? m.content : JSON.stringify(m.content),
    }));
  } catch (e) {
    out.threw = String(e?.message ?? e).slice(0, 200);
  }
  srv.close();
  record(out);
}

// ── Anthropic 方言（@anthropic-ai/sdk） ────────────────────────────────────

async function anthropicCase(label, records) {
  const { srv, port } = await serve(anthropicSSE(records));
  const client = new Anthropic({ apiKey: "k", baseURL: `http://127.0.0.1:${port}` });
  const out = { dialect: "anthropic", label, streamedText: "", finalStop: null, finalBlocks: null, currentBlocks: null, kept: null, iterError: null, finalError: null, finalText: null };
  const stream = client.messages.stream({
    model: "m",
    max_tokens: 64,
    messages: [{ role: "user", content: "hi" }],
  });
  try {
    for await (const ev of stream) {
      if (ev.type === "content_block_delta" && ev.delta.type === "text_delta") out.streamedText += ev.delta.text;
      if (ev.type === "content_block_delta" && ev.delta.type === "input_json_delta") out.streamedText += ev.delta.partial_json;
    }
  } catch (e) {
    out.iterError = String(e?.message ?? e).replace(/\s+/g, " ").slice(0, 140);
  }
  const render = (msg) =>
    msg ? (msg.content ?? []).map((b) =>
      b.type === "text" ? { type: "text", text: b.text }
        : b.type === "tool_use" ? { type: "tool_use", name: b.name, input: b.input }
          : { type: b.type }) : null;
  try {
    out.currentBlocks = render(stream.currentMessage);
  } catch { out.currentBlocks = "ERR"; }
  try {
    const f = await stream.finalMessage();
    out.finalBlocks = render(f);
    out.finalStop = f.stop_reason;
    out.kept = out.finalBlocks;
  } catch (e) {
    out.finalError = String(e?.message ?? e).replace(/\s+/g, " ").slice(0, 160);
    // 出错时客户端只能拿流式累积的那份
    out.kept = out.currentBlocks;
  }
  // finalText() 是 SDK 里更严格的入口：它要求至少有一个 text 块。空回复会在这里
  // 抛错，而 finalMessage() 不会 —— 两条路径的差异是 §2.6 的关键。
  if (stream.finalText) {
    try {
      out.finalText = await stream.finalText();
    } catch (e) {
      out.finalText = `THREW ${e?.constructor?.name}: ${String(e?.message ?? e).replace(/\s+/g, " ").slice(0, 160)}`;
    }
  }
  srv.close();
  record(out);
}

// ── A 两条方言：正文 + 错误信封 ────────────────────────────────────────────

console.log("### A 半截正文 + 错误信封（当前代理的中途失败形状）");
await openaiCase("openai/error-after-content", [
  chunk("a", { role: "assistant" }),
  chunk("a", { content: "Here is the beginning of the answer" }),
  { error: { message: "CC upstream idle timeout", type: "upstream_error", code: "network_error" } },
]);

await anthropicCase("anthropic/error-after-content", [
  msgStart("msg_a"),
  ["content_block_start", { type: "content_block_start", index: 0, content_block: { type: "text", text: "" } }],
  ["content_block_delta", { type: "content_block_delta", index: 0, delta: { type: "text_delta", text: "Here is the beginning of the answer" } }],
  ["content_block_stop", { type: "content_block_stop", index: 0 }],
  ["error", { type: "error", error: { type: "overloaded_error", message: "[upstream-error] idle timeout" } }],
  ["message_stop", { type: "message_stop" }],
]);

// ── B 两条方言：把断流当截断收尾（不发 error） ────────────────────────────

console.log("\n### B 半截正文 + 干净收尾（把断流当截断处理）");
await openaiCase("openai/truncated-as-stop", [
  chunk("b", { role: "assistant" }),
  chunk("b", { content: "Here is the beginning of the answer" }),
  chunk("b", {}, "stop"),
]);

await anthropicCase("anthropic/truncated-as-max-tokens", [
  msgStart("msg_b"),
  ["content_block_start", { type: "content_block_start", index: 0, content_block: { type: "text", text: "" } }],
  ["content_block_delta", { type: "content_block_delta", index: 0, delta: { type: "text_delta", text: "Here is the beginning of the answer" } }],
  ["content_block_stop", { type: "content_block_stop", index: 0 }],
  ["message_delta", { type: "message_delta", delta: { stop_reason: "max_tokens", stop_sequence: null }, usage: { output_tokens: 9 } }],
  ["message_stop", { type: "message_stop" }],
]);

// ── C 危险面：工具调用被截断成非法 JSON ───────────────────────────────────

console.log("\n### C 工具调用半截 JSON（「当截断处理」方案的唯一危险面）");
await anthropicCase("anthropic/tool-use-truncated-clean-stop", [
  msgStart("msg_c"),
  ["content_block_start", { type: "content_block_start", index: 0, content_block: { type: "tool_use", id: "toolu_1", name: "run_shell", input: {} } }],
  ["content_block_delta", { type: "content_block_delta", index: 0, delta: { type: "input_json_delta", partial_json: '{"cmd":"rm -rf /tmp/x' } }],
  ["content_block_stop", { type: "content_block_stop", index: 0 }],
  ["message_delta", { type: "message_delta", delta: { stop_reason: "max_tokens", stop_sequence: null }, usage: { output_tokens: 9 } }],
  ["message_stop", { type: "message_stop" }],
]);

await anthropicCase("anthropic/tool-use-truncated-error", [
  msgStart("msg_d"),
  ["content_block_start", { type: "content_block_start", index: 0, content_block: { type: "tool_use", id: "toolu_1", name: "run_shell", input: {} } }],
  ["content_block_delta", { type: "content_block_delta", index: 0, delta: { type: "input_json_delta", partial_json: '{"cmd":"rm -rf /tmp/x' } }],
  ["content_block_stop", { type: "content_block_stop", index: 0 }],
  ["error", { type: "error", error: { type: "overloaded_error", message: "[upstream-error] idle timeout" } }],
  ["message_stop", { type: "message_stop" }],
]);

// ── D 空消息：spec §2.6 的待验证项 ────────────────────────────────────────

console.log("\n### D 零内容块收尾（spec §2.6 待验证项）");
await anthropicCase("anthropic/empty-no-blocks", [
  msgStart("msg_e"),
  ["message_delta", { type: "message_delta", delta: { stop_reason: "end_turn", stop_sequence: null }, usage: { output_tokens: 0 } }],
  ["message_stop", { type: "message_stop" }],
]);

await anthropicCase("anthropic/empty-text-block", [
  msgStart("msg_f"),
  ["content_block_start", { type: "content_block_start", index: 0, content_block: { type: "text", text: "" } }],
  ["content_block_stop", { type: "content_block_stop", index: 0 }],
  ["message_delta", { type: "message_delta", delta: { stop_reason: "end_turn", stop_sequence: null }, usage: { output_tokens: 0 } }],
  ["message_stop", { type: "message_stop" }],
]);

mkdirSync(path.dirname(OUT), { recursive: true });
writeFileSync(OUT, JSON.stringify({ note: "客户端侧实测；与 golden/ 的代理侧记录互补", results }, null, 2));
console.log(`\nwrote ${path.relative(process.cwd(), OUT)} (${results.length} cases)`);
