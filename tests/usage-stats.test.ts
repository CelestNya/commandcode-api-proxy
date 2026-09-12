// 用量/缓存统计模块的单测：速率计算、累计、异常输入兜底。
import { describe, it, expect, beforeEach } from "vitest";
import { recordUsage, snapshot, __resetUsageStatsForTests } from "@/usage-stats.js";

describe("usage-stats", () => {
  beforeEach(() => __resetUsageStatsForTests());

  it("computes cache rate from prompt/cached tokens", () => {
    const snap = recordUsage("m", { promptTokens: 1000, completionTokens: 50,
                                    promptTokensDetails: { cachedTokens: 250 } });
    expect(snap).not.toBeNull();
    expect(snap!.cacheRate).toBe(25);
    expect(snap!.requests).toBe(1);
  });

  it("accumulates across requests", () => {
    recordUsage("a", { promptTokens: 1000, completionTokens: 10,
                       promptTokensDetails: { cachedTokens: 500 } });
    recordUsage("b", { promptTokens: 1000, completionTokens: 10,
                       promptTokensDetails: { cachedTokens: 0 } });
    const snap = snapshot();
    expect(snap.requests).toBe(2);
    expect(snap.promptTokens).toBe(2000);
    expect(snap.cachedTokens).toBe(500);
    expect(snap.cacheRate).toBe(25);
  });

  it("treats missing cached details as zero", () => {
    recordUsage("a", { promptTokens: 500, completionTokens: 5 });
    const snap = snapshot();
    expect(snap.cachedTokens).toBe(0);
    expect(snap.cacheRate).toBe(0);
  });

  it("returns null when there is no usage data", () => {
    expect(recordUsage("a", undefined)).toBeNull();
    expect(recordUsage("a", {})).toBeNull();
    expect(snapshot().requests).toBe(0);
  });

  it("handles zero prompt tokens without dividing by zero", () => {
    const snap = recordUsage("a", { promptTokens: 0, completionTokens: 1 });
    expect(snap!.cacheRate).toBe(0);
  });
});
