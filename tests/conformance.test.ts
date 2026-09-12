// 协议一致性模糊测试：随机生成大量上游事件序列，把两个编码器的每条输出
// 记录按严格客户端（AI SDK）的 schema 约束做状态机校验。
// 目的：把「某条记录不符合客户端联合判别/块生命周期」这类 bug（如
// signature_delta 顶层事件、message_stop 空载荷）在所有路径上一次性兜住。
import { describe, it, expect } from "vitest";
import { AnthropicStreamEncoder } from "@/translate/anthropic.js";
import { OpenAIStreamEncoder } from "@/translate/openai.js";
import { formatAnthropicSSE } from "@/stream.js";
import type { CCEvent } from "@/translate/types.js";

// ── 可复现随机源 ──
function mulberry32(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a |= 0; a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const ANTHROPIC_EVENTS = new Set([
  "message_start", "content_block_start", "content_block_delta", "content_block_stop",
  "error", "message_delta", "message_stop", "ping",
]);
// thinking 块允许 thinking_delta 与 signature_delta 两种 delta
const DELTA_FOR_BLOCK: Record<string, string[]> = {
  text: ["text_delta"], thinking: ["thinking_delta", "signature_delta"],
  tool_use: ["input_json_delta"],
};

interface ARecord { event: string; data: Record<string, unknown>; }

/** 严格客户端状态机：校验 Anthropic 编码器的全部输出记录。 */
function validateAnthropic(records: ARecord[], label: string): string[] {
  const problems: string[] = [];
  let started = false;
  let stopCount = 0;
  let afterStop = false;
  const open = new Map<number, string>();
  for (const r of records) {
    const d = r.data as Record<string, unknown>;
    if (afterStop) problems.push(`${label}: message_stop 之后仍有记录 ${r.event}`);
    if (!ANTHROPIC_EVENTS.has(r.event)) problems.push(`${label}: 非法事件名 ${r.event}`);
    if (d.type !== r.event) problems.push(`${label}: ${r.event} 的 data.type=${JSON.stringify(d.type)} 缺失或不一致`);
    switch (r.event) {
      case "message_start":
        if (started) problems.push(`${label}: message_start 重复`);
        started = true;
        break;
      case "content_block_start": {
        if (!started) problems.push(`${label}: content_block_start 先于 message_start`);
        const idx = d.index as number;
        if (open.has(idx)) problems.push(`${label}: 块 ${idx} 重复 start`);
        open.set(idx, (d.content_block as { type: string }).type);
        break;
      }
      case "content_block_delta": {
        const idx = d.index as number;
        const dt = (d.delta as { type: string }).type;
        if (!open.has(idx)) { problems.push(`${label}: 块 ${idx} 未开启就收到 delta(${dt})`); break; }
        const bt = open.get(idx)!;
        const allowedDeltas = DELTA_FOR_BLOCK[bt];
        if (allowedDeltas && !allowedDeltas.includes(dt))
          problems.push(`${label}: 块 ${idx}(${bt}) 收到错误 delta 类型 ${dt}`);
        break;
      }
      case "content_block_stop": {
        const idx = d.index as number;
        if (!open.has(idx)) problems.push(`${label}: 块 ${idx} 未开启就收到 stop`);
        open.delete(idx);
        break;
      }
      case "message_stop": {
        stopCount += 1;
        if (open.size > 0) problems.push(`${label}: message_stop 时仍有未关闭块 ${[...open.keys()]}`);
        afterStop = true;
        break;
      }
      case "error":
        // error 是终端事件；其后只允许 message_stop
        break;
    }
  }
  if (stopCount > 1) problems.push(`${label}: message_stop 出现 ${stopCount} 次`);
  return problems;
}

/** 生成随机但「上游合法」的事件序列（工具 delta 只出现在其 final 之前，
 *  且 delta 片段拼接恰好等于 final 的完整参数——与真实上游一致）。 */
function genAnthropicEvents(rng: () => number, id: number): CCEvent[] {
  const evs: CCEvent[] = [{ type: "start", data: { model: "m" } }];
  const toolIds = ["call_a", "call_b"];
  const finalized = new Set<string>();
  const plans = new Map<string, { pieces: string[]; sent: number; full: unknown }>();
  let sawContent = false;
  const nEvents = 1 + Math.floor(rng() * 10);
  for (let i = 0; i < nEvents; i++) {
    const roll = rng();
    if (roll < 0.3) {
      evs.push({ type: "text-delta", data: { text: "t" + i } });
      sawContent = true;
    } else if (roll < 0.55) {
      evs.push({ type: "reasoning-delta", data: { text: "r" + i } });
    } else if (roll < 0.8) {
      const open = toolIds.filter((t) => !finalized.has(t));
      if (open.length === 0) continue;
      const tid = open[Math.floor(rng() * open.length)];
      let plan = plans.get(tid);
      if (!plan) {
        const full = rng() < 0.5 ? { x: "v" + i } : {};
        const args = JSON.stringify(full);
        const cut = 1 + Math.floor(rng() * Math.max(1, args.length - 1));
        plan = { pieces: [args.slice(0, cut), args.slice(cut)].filter((p) => p.length > 0), sent: 0, full };
        plans.set(tid, plan);
      }
      if (plan.sent < plan.pieces.length && rng() < 0.7) {
        evs.push({ type: "tool-call-delta",
                   data: { toolCallId: tid, name: "tool_" + tid, arguments: plan.pieces[plan.sent++] } });
      } else {
        finalized.add(tid);
        evs.push({ type: "tool-call", data: { toolCallId: tid, toolName: "tool_" + tid, input: plan.full } });
      }
    }
  }
  // 收尾：未发完 final 的工具直接补 final（合法上游行为）
  for (const tid of toolIds) {
    const plan = plans.get(tid);
    if (plan && !finalized.has(tid)) {
      finalized.add(tid);
      evs.push({ type: "tool-call", data: { toolCallId: tid, toolName: "tool_" + tid, input: plan.full } });
    }
  }
  if (rng() < 0.85) {
    evs.push({ type: "finish", data: { finishReason: "stop" } });
  } // 否则截断：服务层会合成 finishRecords
  // 行为不端的上游可能在终态后继续发事件——编码器必须吞掉，不得重复收尾
  if ((evs[evs.length - 1] as { type: string }).type === "finish" && rng() < 0.5) {
    evs.push({ type: "text-delta", data: { text: "late" } });
    evs.push({ type: "finish", data: { finishReason: "stop" } });
  }
  void id;
  return evs;
}

describe("Anthropic 编码器协议一致性（模糊）", () => {
  it("300 条随机事件序列的全部输出记录均符合严格客户端 schema", () => {
    const allProblems: string[] = [];
    for (let seed = 1; seed <= 300; seed++) {
      const rng = mulberry32(seed);
      const encoder = new AnthropicStreamEncoder("m");
      const records: ARecord[] = [];
      const evs = genAnthropicEvents(rng, seed);
      let threw: unknown = null;
      for (const e of evs) {
        try { records.push(...(encoder.emit(e) as ARecord[])); }
        catch (err) { threw = err; break; }
      }
      // 镜像服务层 pumpStream onEnd：仅未终态时才合成收尾
      if (!encoder.finished)
        records.push(...(encoder.finishRecords("end_turn") as ARecord[]));
      const problems = validateAnthropic(records, `seed=${seed}`);
      if (threw) {
        // 唯一允许的抛错：上游自相矛盾的工具参数（已有专门测试与错误信封路径）
        const msg = String((threw as Error).message);
        if (!msg.includes("Inconsistent upstream tool arguments"))
          problems.push(`seed=${seed}: 意外抛错 ${msg}`);
      }
      // 格式化器是唯一线上出口：每条记录序列化后必须带 type 判别
      for (const r of records) {
        const wire = formatAnthropicSSE(r.event, r.data);
        if (!/"type":"/.test(wire)) problems.push(`seed=${seed}: 线上记录缺 type -> ${wire.trim()}`);
      }
      allProblems.push(...problems);
    }
    expect(allProblems).toEqual([]);
  });
});

// ── OpenAI 路径 ──

const OPENAI_DELTA_KEYS = new Set(["role", "content", "reasoning_content", "tool_calls"]);

function choices_len(c: Record<string, unknown>): number {
  const ch = c.choices as unknown[] | undefined;
  return Array.isArray(ch) ? ch.length : 0;
}

function validateOpenAI(chunks: Record<string, unknown>[], label: string): string[] {
  const problems: string[] = [];
  let finishCount = 0;
  for (const c of chunks) {
    // error 信封是联合的另一支：先判别，再校验普通 chunk 形态
    if (c.error !== undefined) {
      const e = c.error as { message?: unknown };
      if (typeof e?.message !== "string") problems.push(`${label}: error.message 缺失`);
      if (choices_len(c) > 0) problems.push(`${label}: error 信封不应带 choices`);
      continue;
    }
    if (c.object !== "chat.completion.chunk") problems.push(`${label}: object=${c.object}`);
    if (typeof c.id !== "string" || typeof c.model !== "string")
      problems.push(`${label}: 缺 id/model`);
    const choices = c.choices as Array<Record<string, unknown>> | undefined;
    if (!Array.isArray(choices)) { problems.push(`${label}: choices 缺失`); continue; }
    for (const ch of choices) {
      const delta = (ch.delta ?? {}) as Record<string, unknown>;
      for (const k of Object.keys(delta))
        if (!OPENAI_DELTA_KEYS.has(k)) problems.push(`${label}: 非法 delta 字段 ${k}`);
      const fr = ch.finish_reason;
      if (fr != null && fr !== false && typeof fr !== "string")
        problems.push(`${label}: finish_reason 类型异常`);
      if (typeof fr === "string") finishCount += 1;
      for (const tc of (delta.tool_calls as Array<Record<string, unknown>>) ?? []) {
        if (typeof tc.index !== "number") problems.push(`${label}: tool_call 缺 index`);
        const fn = tc.function as { name?: unknown; arguments?: unknown } | undefined;
        if (fn && typeof fn.arguments !== "string") problems.push(`${label}: arguments 非字符串`);
      }
    }
  }
  if (finishCount > 1) problems.push(`${label}: finish chunk ${finishCount} 个`);
  return problems;
}

function genOpenAIEvents(rng: () => number): CCEvent[] {
  const evs: CCEvent[] = [{ type: "start", data: {} }];
  const tid = "call_a";
  let finalized = false;
  // 完整参数先定好，delta 分块发送，final 用完整对象——保证拼接一致
  const full = { x: 1 };
  const args = JSON.stringify(full);
  const pieces = [args.slice(0, 4), args.slice(4)];
  let sent = 0;
  const n = 1 + Math.floor(rng() * 8);
  for (let i = 0; i < n; i++) {
    const roll = rng();
    if (roll < 0.35) evs.push({ type: "text-delta", data: { text: "t" + i } });
    else if (roll < 0.6) evs.push({ type: "reasoning-delta", data: { text: "r" + i } });
    else if (roll < 0.85 && !finalized) {
      if (sent < pieces.length && rng() < 0.7) {
        evs.push({ type: "tool-call-delta",
                   data: { toolCallId: tid, name: "get_time", arguments: pieces[sent++] } });
      } else {
        finalized = true;
        evs.push({ type: "tool-call", data: { toolCallId: tid, toolName: "get_time", input: full } });
      }
    }
  }
  if (!finalized && rng() < 0.5)
    evs.push({ type: "tool-call", data: { toolCallId: tid, toolName: "get_time", input: full } });
  const tail = rng();
  if (tail < 0.8) evs.push({ type: "finish", data: { finishReason: "stop" } });
  else if (tail < 0.9) evs.push({ type: "error", data: { message: "mock upstream error" } });
  // 行为不端的上游可能在终态后继续发事件——编码器必须吞掉
  const lastType = (evs[evs.length - 1] as { type: string }).type;
  if ((lastType === "finish" || lastType === "error") && rng() < 0.5) {
    evs.push({ type: "text-delta", data: { text: "late" } });
    evs.push({ type: "finish", data: { finishReason: "stop" } });
  }
  return evs;
}

describe("OpenAI 编码器协议一致性（模糊）", () => {
  it("300 条随机事件序列的全部 chunk 均符合严格客户端 schema", () => {
    const allProblems: string[] = [];
    for (let seed = 1; seed <= 300; seed++) {
      const rng = mulberry32(seed);
      const encoder = new OpenAIStreamEncoder("m");
      const chunks: Record<string, unknown>[] = [];
      for (const e of genOpenAIEvents(rng)) {
        for (const c of encoder.emit(e) as Record<string, unknown>[]) chunks.push(c);
      }
      // 镜像服务层 pumpStream onEnd：仅未终态时才合成收尾
      if (!encoder.finished)
        for (const c of encoder.finishChunks("stop") as Record<string, unknown>[]) chunks.push(c);
      allProblems.push(...validateOpenAI(chunks, `seed=${seed}`));
    }
    expect(allProblems).toEqual([]);
  });
});
