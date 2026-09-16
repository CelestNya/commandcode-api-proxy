// 断流重试「怎么拼」—— 定案探针（两个真实 SDK）。
//
// 生产事故：CC 在流中途发 in-band error（reasoning 已下发 7662 字符、
// textDeltaChars=0），两层闸门（started / hasEmittedContent）都已关闭，
// 重试从未发生，整轮报废。
//
// 用户要求「都重试一次」。但「重试」有好几种拼法，客户端容忍度差别很大。
// 本探针逐一实测，因为仓库已有一次教训：基于未验证前提的修复（a3864f6）部署后问题照旧。
//
// 每个用例只改「拼接方式」，其余相同：第一段 thinking 已下发 → CC 出错 →
// 代理重发上游 → 第二段到达 → 正常收尾（重试成功时不发 error 记录）。
//
//   A 基线不重试        ：think + error + message_stop（现状）
//   B 朴素重发整轮      ：重发 message_start，再走完整第二轮
//   C 原地续接·思考也转发：不重发 message_start，第二段思考接着写
//   D 原地续接·抑制思考  ：只转发第二段正文
//   E 正文已下发后重试   ：第一段正文已发出，再续接第二轮
//
// 用法：node conformance/client-probes/retry-splice.mjs

import { createAnthropic } from "@ai-sdk/anthropic";
import { createOpenAICompatible } from "@ai-sdk/openai-compatible";
import { streamText } from "ai";
import http from "node:http";
import { mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));

function serve(raw, ctype) {
  return new Promise((r) => {
    const s = http.createServer((_q, res) => {
      res.writeHead(200, { "Content-Type": ctype });
      res.write(raw);
      res.end();
    });
    s.listen(0, "127.0.0.1", () => r({ s, port: s.address().port }));
  });
}

// ── Anthropic 记录构造 ──
// 每条记录是 [eventName, payload]；带 ... 的常量是「记录数组」。
const an = (recs) => recs.map(([e, d]) => `event: ${e}\ndata: ${JSON.stringify(d)}\n\n`).join("");
const MS = (id) => [["message_start", { type: "message_start", message: { id, type: "message", role: "assistant", model: "m", content: [], stop_reason: null, stop_sequence: null, usage: { input_tokens: 5, output_tokens: 1 } } }]];
const MSTOP = [["message_stop", { type: "message_stop" }]];
const MD = (r) => [["message_delta", { type: "message_delta", delta: { stop_reason: r, stop_sequence: null }, usage: { output_tokens: 9 } }]];
const ERR = [["error", { type: "error", error: { type: "overloaded_error", message: "[upstream-error] Network connection lost." } }]];
const CBSTART = (i, b) => [["content_block_start", { type: "content_block_start", index: i, content_block: b }]];
const CBDELTA = (i, d) => [["content_block_delta", { type: "content_block_delta", index: i, delta: d }]];
const CBSTOP = (i) => [["content_block_stop", { type: "content_block_stop", index: i }]];
const TH_OPEN = (i, t) => [...CBSTART(i, { type: "thinking", thinking: "" }), ...CBDELTA(i, { type: "thinking_delta", thinking: t })];
const TH_MORE = (i, t) => [...CBDELTA(i, { type: "thinking_delta", thinking: t })];
const TXT_OPEN = (i, t) => [...CBSTART(i, { type: "text", text: "" }), ...CBDELTA(i, { type: "text_delta", text: t })];
const TOOL_OPEN = (i, id, name, partial) => [
  ...CBSTART(i, { type: "tool_use", id, name, input: {} }),
  ...CBDELTA(i, { type: "input_json_delta", partial_json: partial }),
];

async function anthropicCase(label, note, recs) {
  const { s, port } = await serve(an(recs), "text/event-stream");
  const c = createAnthropic({ apiKey: "k", baseURL: `http://127.0.0.1:${port}/v1` });
  const out = { dialect: "anthropic", label, note, text: "", think: "", finish: null, errorPart: false, threw: null, kept: null };
  try {
    const r = streamText({ model: c("m"), prompt: "hi" });
    for await (const p of r.fullStream) {
      if (p.type === "text-delta") out.text += p.text;
      else if (p.type === "reasoning-delta") out.think += p.text ?? "";
      else if (p.type === "error") out.errorPart = true;
      else if (p.type === "finish") out.finish = p.finishReason;
    }
    const resp = await r.response;
    out.kept = resp.messages.map((m) => (typeof m.content === "string" ? m.content : JSON.stringify(m.content)).slice(0, 320));
  } catch (e) {
    out.threw = String(e?.message ?? e).slice(0, 140);
  }
  s.close();
  return out;
}

// ── OpenAI 记录构造 ──
const ch = (id, d, f = null) => ({ id, object: "chat.completion.chunk", created: 1, model: "m", choices: [{ index: 0, delta: d, finish_reason: f }] });

async function openaiCase(label, note, chunks) {
  const raw = chunks.map((c) => `data: ${JSON.stringify(c)}\n\n`).join("") + "data: [DONE]\n\n";
  const { s, port } = await serve(raw, "text/event-stream");
  const p = createOpenAICompatible({ name: "p", baseURL: `http://127.0.0.1:${port}/v1` });
  const out = { dialect: "openai", label, note, text: "", think: "", finish: null, errorPart: false, threw: null, kept: null };
  try {
    const r = streamText({ model: p.chatModel("m"), prompt: "hi" });
    for await (const part of r.fullStream) {
      if (part.type === "text-delta") out.text += part.text;
      else if (part.type === "reasoning-delta") out.think += part.text ?? "";
      else if (part.type === "error") out.errorPart = true;
      else if (part.type === "finish") out.finish = part.finishReason;
    }
    const resp = await r.response;
    out.kept = resp.messages.map((m) => (typeof m.content === "string" ? m.content : JSON.stringify(m.content)).slice(0, 320));
  } catch (e) {
    out.threw = String(e?.message ?? e).slice(0, 140);
  }
  s.close();
  return out;
}

const results = [];

// ───────────────────────── Anthropic ─────────────────────────
results.push(await anthropicCase(
  "A-baseline-no-retry",
  "现状：不重试 → 半截思考 + 明确报错",
  [...MS("m1"), ...TH_OPEN(0, "第一次思考"), ...CBSTOP(0), ...ERR, ...MSTOP],
));

results.push(await anthropicCase(
  "B-naive-resend-second-round",
  "朴素重试：重发 message_start，完整第二轮",
  [...MS("m1"), ...TH_OPEN(0, "第一次思考"), ...CBSTOP(0), ...ERR, ...MS("m2"), ...TH_OPEN(1, "第二次思考"), ...CBSTOP(1), ...TXT_OPEN(2, "完整答案"), ...CBSTOP(2), ...MD("end_turn"), ...MSTOP],
));

results.push(await anthropicCase(
  "C-inplace-keep-2nd-thinking",
  "原地续接：不重发 message_start，第二段思考接着写",
  [...MS("m1"), ...TH_OPEN(0, "第一次思考"), ...TH_MORE(0, "第二次思考"), ...CBSTOP(0), ...TXT_OPEN(1, "完整答案"), ...CBSTOP(1), ...MD("end_turn"), ...MSTOP],
));

results.push(await anthropicCase(
  "D-inplace-suppress-2nd-thinking",
  "原地续接：抑制第二段思考，只转发正文",
  [...MS("m1"), ...TH_OPEN(0, "第一次思考"), ...CBSTOP(0), ...TXT_OPEN(1, "完整答案"), ...CBSTOP(1), ...MD("end_turn"), ...MSTOP],
));

results.push(await anthropicCase(
  "E-text-already-emitted",
  "正文已下发后重试：第一段正文 + 第二轮思考 + 完整答案",
  [...MS("m1"), ...TH_OPEN(0, "第一次思考"), ...CBSTOP(0), ...TXT_OPEN(1, "半截正文"), ...CBSTOP(1), ...TH_OPEN(2, "第二次思考"), ...CBSTOP(2), ...TXT_OPEN(3, "完整答案"), ...CBSTOP(3), ...MD("end_turn"), ...MSTOP],
));

results.push(await anthropicCase(
  "F-tool-call-already-emitted",
  "工具调用已下发后重试：半个 tool_use + 第二轮完整 tool_use",
  [...MS("m1"), ...TH_OPEN(0, "第一次思考"), ...CBSTOP(0), ...TOOL_OPEN(1, "toolu_1", "get_time", '{"tz":"UTC'), ...CBSTOP(1), ...TOOL_OPEN(2, "toolu_2", "get_time", '{"tz":"UTC"}'), ...CBSTOP(2), ...MD("tool_use"), ...MSTOP],
));

// ───────────────────────── OpenAI ─────────────────────────
results.push(await openaiCase(
  "A-baseline-no-retry",
  "现状：不重试 → 半截 reasoning + 错误块",
  [ch("a", { role: "assistant" }), ch("a", { reasoning_content: "第一次思考" }), ch("a", { error: { message: "[upstream-error] Network connection lost.", type: "upstream_error", code: "network_error" } })],
));

results.push(await openaiCase(
  "B-naive-resend-second-round",
  "朴素重试：重发 role chunk，完整第二轮",
  [ch("a", { role: "assistant" }), ch("a", { reasoning_content: "第一次思考" }), ch("a", { error: { message: "boom", type: "upstream_error", code: "network_error" } }), ch("b", { role: "assistant" }), ch("b", { reasoning_content: "第二次思考" }), ch("b", { content: "完整答案" }), ch("b", {}, "stop")],
));

results.push(await openaiCase(
  "C-inplace-keep-2nd-reasoning",
  "原地续接：第二段 reasoning 也转发",
  [ch("a", { role: "assistant" }), ch("a", { reasoning_content: "第一次思考" }), ch("a", { reasoning_content: "第二次思考" }), ch("a", { content: "完整答案" }), ch("a", {}, "stop")],
));

results.push(await openaiCase(
  "E-text-already-emitted",
  "正文已下发后重试：半截正文 + 第二轮 reasoning + 完整答案",
  [ch("a", { role: "assistant" }), ch("a", { reasoning_content: "第一次思考" }), ch("a", { content: "半截正文" }), ch("a", { reasoning_content: "第二次思考" }), ch("a", { content: "完整答案" }), ch("a", {}, "stop")],
));

for (const r of results) {
  console.log(`--- ${r.dialect} / ${r.label}`);
  console.log(`    ${r.note}`);
  console.log(`    think : ${JSON.stringify(r.think)}`);
  console.log(`    text  : ${JSON.stringify(r.text)}`);
  console.log(`    finish=${r.finish} errorPart=${r.errorPart}${r.threw ? " threw=" + r.threw : ""}`);
  console.log(`    kept  : ${JSON.stringify(r.kept)}`);
  console.log();
}

mkdirSync(path.join(HERE, "observed"), { recursive: true });
writeFileSync(
  path.join(HERE, "observed", "retry-splice.json"),
  JSON.stringify({
    note: "断流重试的拼接方式 × 两个 SDK。定代理重试实现的依据：朴素重发整轮会让 Anthropic 丢掉答案。",
    results,
  }, null, 2),
);
console.log("written observed/retry-splice.json");
