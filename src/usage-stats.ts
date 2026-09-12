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

/** 记录一次请求的用量并落日志。无用量数据（上游未回报）时返回 null。 */
export function recordUsage(model: string, usage: UsageData | undefined): CacheSnapshot | null {
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
