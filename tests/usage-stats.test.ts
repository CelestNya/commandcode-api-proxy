// 用量/缓存统计模块的单测：速率计算、累计、异常输入兜底、jsonl 持久化。
import { describe, it, expect, beforeEach } from "vitest";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import {
  recordUsage,
  snapshot,
  usageLine,
  __resetUsageStatsForTests,
} from "@/usage-stats.js";

describe("usage-stats", () => {
  beforeEach(() => __resetUsageStatsForTests());

  it("computes cache rate from prompt/cached tokens", () => {
    const snap = recordUsage("m", { promptTokens: 1000, completionTokens: 50,
                                    promptTokensDetails: { cachedTokens: 250 } },
                             { persist: false });
    expect(snap).not.toBeNull();
    expect(snap!.cacheRate).toBe(25);
    expect(snap!.requests).toBe(1);
  });

  it("accumulates across requests", () => {
    recordUsage("a", { promptTokens: 1000, completionTokens: 10,
                       promptTokensDetails: { cachedTokens: 500 } }, { persist: false });
    recordUsage("b", { promptTokens: 1000, completionTokens: 10,
                       promptTokensDetails: { cachedTokens: 0 } }, { persist: false });
    const snap = snapshot();
    expect(snap.requests).toBe(2);
    expect(snap.promptTokens).toBe(2000);
    expect(snap.cachedTokens).toBe(500);
    expect(snap.cacheRate).toBe(25);
  });

  it("treats missing cached details as zero", () => {
    recordUsage("a", { promptTokens: 500, completionTokens: 5 }, { persist: false });
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
    const snap = recordUsage("a", { promptTokens: 0, completionTokens: 1 }, { persist: false });
    expect(snap!.cacheRate).toBe(0);
  });

  it("usageLine emits a machine-parseable jsonl record", () => {
    const line = usageLine("deepseek-v4-flash", {
      promptTokens: 1323, completionTokens: 40,
      promptTokensDetails: { cachedTokens: 1152 },
    });
    const d = JSON.parse(line);
    expect(d.model).toBe("deepseek-v4-flash");
    expect(d.promptTokens).toBe(1323);
    expect(d.cachedTokens).toBe(1152);
    expect(d.completionTokens).toBe(40);
    expect(() => new Date(d.ts).toISOString()).not.toThrow();
  });

  it("persists usage lines to logs/usage.jsonl when enabled", () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), "cc-usage-"));
    try {
      recordUsage("m", { promptTokens: 100, completionTokens: 5,
                         promptTokensDetails: { cachedTokens: 60 } },
                  { persist: true, dir });
      const file = path.join(dir, "usage.jsonl");
      const lines = fs.readFileSync(file, "utf8").trimEnd().split("\n");
      expect(lines).toHaveLength(1);
      expect(JSON.parse(lines[0]).cachedTokens).toBe(60);
    } finally {
      fs.rmSync(dir, { recursive: true, force: true });
    }
  });
});
