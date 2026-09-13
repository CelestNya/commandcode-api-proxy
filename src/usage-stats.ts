// 出站用量与缓存统计（个人版可观测性）：上游在 finish 事件里回报
// prompt/completion token 与缓存命中（OpenAI 形状的
// prompt_tokens_details.cached_tokens 或 Anthropic 形状的
// cache_read_input_tokens，extractUsage 已归一为 UsageData）。
// 每个请求落一行日志（缓存量/缓存率），累计值经 /health 暴露。
import { logger } from "@/logger.js";
import type { UsageData } from "@/translate/types.js";

export interface CacheSnapshot {
  requests: number;
  promptTokens: number;
  cachedTokens: number;
  completionTokens: number;
  /** 缓存命中率，百分比，一位小数 */
  cacheRate: number;
}

const totals = {
  requests: 0,
  promptTokens: 0,
  cachedTokens: 0,
  completionTokens: 0,
};

function rate(cached: number, prompt: number): number {
  return prompt > 0 ? Math.round((cached / prompt) * 1000) / 10 : 0;
}

// 持久化：每请求一行 JSON（logs/usage.jsonl），托盘据此聚合最近 24h 缓存率。
// 行内字段固定为 ts/model/promptTokens/cachedTokens/completionTokens。
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const USAGE_LOG_DIR = path.join(
  path.dirname(path.dirname(fileURLToPath(import.meta.url))),
  "logs",
);
const USAGE_LOG_MAX_BYTES = 5 * 1024 * 1024;
const USAGE_LOG_KEEP_LINES = 2000;

export function usageLine(model: string, usage: UsageData): string {
  return JSON.stringify({
    ts: new Date().toISOString(),
    model,
    promptTokens: usage.promptTokens ?? 0,
    cachedTokens: usage.promptTokensDetails?.cachedTokens ?? 0,
    completionTokens: usage.completionTokens ?? 0,
  });
}

function persistUsageLine(line: string, dir?: string): void {
  const dirAbs = dir ?? USAGE_LOG_DIR;
  const file = path.join(dirAbs, "usage.jsonl");
  try {
    fs.mkdirSync(dirAbs, { recursive: true });
    // Fire-and-forget rotation check: best-effort, never blocks request.
    void fs.promises.stat(file).then(async (stat) => {
      if (stat.size > USAGE_LOG_MAX_BYTES) {
        try {
          const raw = await fs.promises.readFile(file, "utf8");
          const lines = raw.trimEnd().split("\n");
          const keep = lines.slice(-USAGE_LOG_KEEP_LINES).join("\n") + "\n";
          await fs.promises.writeFile(file, keep);
        } catch { /* 轮转失败不影响记录 */ }
      }
    }).catch(() => {});
    // Synchronous append ensures tests can read immediately; cheap for 1 line.
    fs.appendFileSync(file, line + "\n");
  } catch (err) {
    logger.debug(`[usage] persist failed: ${(err as Error).message}`);
  }
}

/** 记录一次请求的用量并落日志。无用量数据（上游未回报）时返回 null。 */
export function recordUsage(
  model: string,
  usage: UsageData | undefined,
  opts?: { persist?: boolean; dir?: string },
): CacheSnapshot | null {
  if (!usage || typeof usage.promptTokens !== "number") return null;
  const cached = usage.promptTokensDetails?.cachedTokens ?? 0;
  const out = usage.completionTokens ?? 0;
  totals.requests += 1;
  totals.promptTokens += usage.promptTokens;
  totals.cachedTokens += cached;
  totals.completionTokens += out;
  const snap = snapshot();
  logger.info(
    `[usage] ${model} 缓存 ${cached}/${usage.promptTokens} tokens（${rate(cached, usage.promptTokens)}%）` +
      ` 输出 ${out} | 累计 ${snap.cachedTokens}/${snap.promptTokens} tokens` +
      `（${snap.cacheRate}%，${snap.requests} 次请求）`,
  );
  if (opts?.persist !== false) persistUsageLine(usageLine(model, usage), opts?.dir);
  return snap;
}

export function snapshot(): CacheSnapshot {
  return {
    requests: totals.requests,
    promptTokens: totals.promptTokens,
    cachedTokens: totals.cachedTokens,
    completionTokens: totals.completionTokens,
    cacheRate: rate(totals.cachedTokens, totals.promptTokens),
  };
}

export function __resetUsageStatsForTests(): void {
  totals.requests = 0;
  totals.promptTokens = 0;
  totals.cachedTokens = 0;
  totals.completionTokens = 0;
}
