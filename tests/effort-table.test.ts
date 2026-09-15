// 档位表必须与官方 CLI 的内置表一致。
//
// 为什么需要这个测试：`src/models.json` 的 reasoningEfforts 是手工维护的，
// 而它决定了一件事的成败——"关闭思考"这类请求会不会被**升档**。
//
// 2026-09-15 的真实事故：表里 deepseek-v4.1-flash / GLM-5.3 等写成
// ["high","max"]，漏了 "low"。于是 `thinking:"disabled"` → "low" 落到裁剪逻辑
// 里被升档成 "high"，用户看到的正是"明明关了思考，还在大量思考"。
// 手抄错一个字就复现，所以把官方表固化成快照来对峙。
//
// 上游为什么不给我们枚举？实测 2026-09-15：`/provider/v1/models` 返回的 69 个
// 模型条目**一律只有** id/object/created/owned_by/name/context_length，
// 没有任何能力字段。客户端遇到新模型无法询问 API，官方 CLI 靠的是自己 bundle
// 里内置的这张表。我们唯一能对齐的权威来源就是它。
//
// 快照由 conformance/extract-official-efforts.mjs 生成（不手抄）。CLI 升级后重跑
// 那个脚本刷新。

import { describe, it, expect } from "vitest";
import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import modelsData from "@/models.json" with { type: "json" };

const HERE = path.dirname(fileURLToPath(import.meta.url));
const snapshot = JSON.parse(
  readFileSync(path.join(HERE, "..", "conformance", "official-efforts.json"), "utf8"),
) as { source: { version: string }; efforts: Record<string, string[]> };

const ours: Record<string, string[]> = modelsData.reasoningEfforts ?? {};
const official: Record<string, string[]> = snapshot.efforts;

describe(`档位表与官方 CLI 对齐（快照 command-code@${snapshot.source.version}）`, () => {
  it("快照本身是可信的：非空、取值合法、无重复", () => {
    const ids = Object.keys(official);
    expect(ids.length).toBeGreaterThan(0);
    const valid = new Set(["low", "medium", "high", "xhigh", "max"]);
    for (const [id, levels] of Object.entries(official)) {
      expect(levels.length, `${id} 档位为空`).toBeGreaterThan(0);
      for (const l of levels) expect(valid.has(l), `${id} 含非法档位 ${l}`).toBe(true);
      // 上游是 Zod enum，重复值会被拒；顺带保证快照抽取没有串位。
      expect(new Set(levels).size, `${id} 档位重复`).toBe(levels.length);
    }
  });

  // 每条官方条目我们都要有，且值完全一致。少一条 = 该类模型的"关闭思考"
  // 会走未编目分支；值不一致 = 低档请求会被裁剪到错误的档位。
  it("官方表里的每个模型，我们的档位完全一致", () => {
    const mismatched: string[] = [];
    for (const [id, levels] of Object.entries(official)) {
      const mine = ours[id];
      if (!mine) {
        mismatched.push(`${id}: 我们表里缺失（官方 ${levels.join(",")}）`);
      } else if (JSON.stringify(mine) !== JSON.stringify(levels)) {
        mismatched.push(`${id}: 我们 ${mine.join(",")} ≠ 官方 ${levels.join(",")}`);
      }
    }
    expect(mismatched, `\n${mismatched.join("\n")}\n`).toEqual([]);
  });

  // 关键不变式（比逐条相等更能说明危害）：官方支持的最低档，我们不能裁掉。
  // "关闭思考" 依赖它作为落点——丢了它，请求就会被升档。
  it("凡官方支持 low 的模型，我们不得丢失 low", () => {
    const lost = Object.entries(official)
      .filter(([, levels]) => levels.includes("low"))
      .filter(([id]) => !(ours[id] ?? []).includes("low"))
      .map(([id]) => `${id}: 我们=${ours[id]?.join(",") ?? "(缺失)"}，官方含 low`);
    expect(lost, `\n${lost.join("\n")}\n`).toEqual([]);
  });

  it("我们的表里不能出现快照之外的档位取值", () => {
    const valid = new Set(["low", "medium", "high", "xhigh", "max"]);
    const bad: string[] = [];
    for (const [id, levels] of Object.entries(ours)) {
      for (const l of levels) if (!valid.has(l)) bad.push(`${id}: ${l}`);
    }
    expect(bad, `\n${bad.join("\n")}\n`).toEqual([]);
  });
});
