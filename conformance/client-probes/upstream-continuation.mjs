// 断流之后"继续这个会话"能不能走通 —— 上游侧探针。
//
// 与 partial-context.mjs 的分工：那个测客户端留下了什么，这个测留下来的东西
// 能不能真的续上。spec §8.7 的两条方案（预填 vs 指令接续）就在此处比较。
//
// 需要真实上游（默认打本机已运行的代理 http://127.0.0.1:8787）与可用 key。
// key 从 ZCode 的 provider 配置里读，避免在仓库里落任何凭据。
//
// 用法：node conformance/client-probes/upstream-continuation.mjs [--endpoint URL] [--key KEY]

import { readFileSync, mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const OUT = path.join(HERE, "observed", "upstream-continuation.json");
const results = [];
const args = process.argv.slice(2);
const flag = (name, fallback) => {
  const i = args.indexOf(`--${name}`);
  return i >= 0 ? args[i + 1] : fallback;
};

const ENDPOINT = flag("endpoint", "http://127.0.0.1:8787/v1/messages");
const MODEL = flag("model", "deepseek/deepseek-v4-flash");
const ZCODE_CONFIG = "C:/Users/CelestNya/.zcode/v2/config.json";

function loadKey() {
  if (flag("key", null)) return flag("key", null);
  const cfg = JSON.parse(readFileSync(ZCODE_CONFIG, "utf8"));
  const provider = cfg.provider?.["79db332f-fc32-40fb-8935-d6f77b320ca5"];
  const key = provider?.options?.apiKey;
  if (!key) throw new Error(`no key at provider_config; pass --key`);
  return key;
}

const KEY = loadKey();

async function ask(label, messages, extra = {}) {
  const t0 = Date.now();
  try {
    const r = await fetch(ENDPOINT, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-api-key": KEY,
        "anthropic-version": "2023-06-01",
      },
      body: JSON.stringify({ model: MODEL, max_tokens: 1024, messages, ...extra }),
    });
    const j = JSON.parse(await r.text());
    if (j.error) {
      console.log(`${label}\n  status=${r.status} ERROR ${j.error.type}: ${j.error.message}`);
      results.push({ label, status: r.status, error: `${j.error.type}: ${j.error.message}` });
      return;
    }
    const blocks = (j.content ?? []).map((b) => (b.type === "text" ? b.text : `[${b.type} ${b.name ?? ""}]`));
    console.log(
      `${label}\n  status=${r.status} stop=${j.stop_reason} ${Date.now() - t0}ms\n  ${JSON.stringify(blocks).slice(0, 300)}`,
    );
    results.push({
      label,
      status: r.status,
      stopReason: j.stop_reason,
      ms: Date.now() - t0,
      blocks,
      text: blocks.filter((b) => !b.startsWith("[")).join(""),
    });
  } catch (e) {
    console.log(`${label}\n  threw ${String(e?.message ?? e)}`);
    results.push({ label, threw: String(e?.message ?? e) });
  }
}

const TOOLS = [
  {
    name: "get_time",
    description: "Get the current time in a timezone.",
    input_schema: { type: "object", properties: { tz: { type: "string" } }, required: ["tz"] },
  },
];

// 目的：确认末尾 assistant 消息是否被当作"续写起点"（prefill 语义）。
// 若模型从 1 重新开始，说明 CC 的 messages 端点不实现 prefill。
console.log("### 1 对照：assistant 历史可见性");
await ask("visibility", [
  { role: "user", content: "Remember this code: XK-4471" },
  { role: "assistant", content: "Noted, the code is XK-4471." },
  { role: "user", content: "What is the code? Reply with just the code, nothing else." },
]);

console.log("\n### 2 方案 A：预填（末尾 assistant，期望从 5, 之后接着写）");
await ask("prefill-numbers", [
  { role: "user", content: "Count from 1 to 40. Numbers only, comma separated. No thinking, no preamble." },
  { role: "assistant", content: "1, 2, 3, 4, 5," },
]);

console.log("\n### 2b 方案 A 变体：固定句式预填（期望从 GAMMA 接着写）");
await ask("prefill-phrase", [
  { role: "user", content: "Repeat exactly this sequence, nothing else: ALPHA BETA GAMMA DELTA EPSILON ZETA. No thinking." },
  { role: "assistant", content: "ALPHA BETA" },
]);

console.log("\n### 3 方案 B：指令接续（期望从 6, 开始，不重复 1-5）");
await ask("instruct-continue", [
  { role: "user", content: "Count from 1 to 40. Numbers only, comma separated. No thinking, no preamble." },
  { role: "assistant", content: "1, 2, 3, 4, 5," },
  {
    role: "user",
    content:
      'Your previous output was cut off by a network failure. Continue the sequence EXACTLY from where it stopped (after "5,"). Output only the remaining numbers, do not repeat what you already wrote, no thinking.',
  },
]);

console.log("\n### 4 危险面：空参数 tool_use 回传 + 请求重发（半截工具调用的补救）");
await ask(
  "empty-tooluse-retry",
  [
    { role: "user", content: "What time is it in Tokyo? Use the get_time tool." },
    { role: "assistant", content: [{ type: "tool_use", id: "toolu_trunc", name: "get_time", input: {} }] },
    { role: "user", content: "That tool call was cut off by a network failure. Please call it again properly." },
  ],
  { tools: TOOLS },
);

console.log("\n### 5 危险面：空参数 tool_use + is_error 配对结果（另一条补救路）");
await ask(
  "empty-tooluse-rejected",
  [
    { role: "user", content: "What time is it in Tokyo? Use the get_time tool." },
    { role: "assistant", content: [{ type: "tool_use", id: "toolu_trunc", name: "get_time", input: {} }] },
    {
      role: "user",
      content: [{ type: "tool_result", tool_use_id: "toolu_trunc", is_error: true, content: "Tool call was interrupted." }],
    },
  ],
  { tools: TOOLS },
);

console.log("\n### 6 对照：完整工具调用 + 配对结果（应正常）");
await ask(
  "complete-tool-ok",
  [
    { role: "user", content: "What time is it in Tokyo? Use the get_time tool." },
    { role: "assistant", content: [{ type: "tool_use", id: "toolu_ok", name: "get_time", input: { tz: "Asia/Tokyo" } }] },
    { role: "user", content: [{ type: "tool_result", tool_use_id: "toolu_ok", content: "2026-09-14T09:00:00+09:00" }] },
  ],
  { tools: TOOLS },
);

// ── 落盘 ────────────────────────────────────────────────────────────────────
// 判读要点（spec §8.7）：
//   · prefill-* 若文本从 1 重新开始 ⇒ CC 的 messages 端点不实现 prefill，方案 A 不可用
//   · instruct-continue 若从 6 开始 ⇒ 方案 B（指令接续）成立
//   · empty-tooluse-* 若重新发起工具调用 ⇒ 半截工具调用可用"空参数回传 + 重发请求"补救
mkdirSync(path.dirname(OUT), { recursive: true });
writeFileSync(
  OUT,
  JSON.stringify(
    {
      note: "上游侧实测：断流后能否续写。需要真实上游与 key，故不进 golden 自动比对。",
      endpoint: ENDPOINT,
      model: MODEL,
      results,
    },
    null,
    2,
  ),
);
console.log(`\nwrote ${path.relative(process.cwd(), OUT)}`);
