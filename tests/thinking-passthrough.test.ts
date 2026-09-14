// Anthropic 思考参数的透传：thinking.type 的三种取值 + output_config.effort。
//
// 背景（2026-09-15 实测）：
//   · ZCode 用 `thinking.type` 传开关（enabled / disabled / adaptive），用
//     `output_config.effort` 传强度档位（low|medium|high|xhigh|max）。
//   · 代理原先只认 `type:"enabled"`，把"关闭思考"直接 400 拒绝——用户看到
//     `Field 'thinking.type' must be "enabled" when thinking is set`。
//   · 代理原先只从 `budget_tokens` 推算 effort，**完全不读 `output_config.effort`**，
//     所以无论选哪档，发出去的都是由 budget 推出的同一个值（实测恒为 high）。
//
// 上游能力（直接打 api.commandcode.ai 实测）：
//   · `params.reasoning_effort` 有效：同 prompt 下 max 的 reasoningTokens 是基线的 5 倍。
//   · `none`/`disabled`/`off`/`minimal`/`adaptive` 一律 400 —— 上游**没有**"关闭"
//     这个档位，所以"关闭思考"只能在低档位上做降级近似。
import { describe, it, expect } from "vitest";
import { toCCRequest } from "@/translate/anthropic.js";
import { validateAnthropicRequest } from "@/translate/validation.js";
import type { AnthropicRequest } from "@/translate/anthropic-types.js";

const base = (extra: Partial<AnthropicRequest> = {}): AnthropicRequest => ({
  model: "deepseek/deepseek-v4-flash",
  max_tokens: 8192,
  messages: [{ role: "user", content: "hi" }],
  ...extra,
});

const effortOf = (req: AnthropicRequest) => toCCRequest(req).params.reasoning_effort;

describe("thinking.type 的合法取值", () => {
  it('接受 "enabled"', () => {
    expect(() => validateAnthropicRequest(base({ thinking: { type: "enabled", budget_tokens: 4096 } }))).not.toThrow();
  });

  // Anthropic 新版协议用 `disabled` 表示显式关闭；ZCode 的关闭档位就发这个。
  it('接受 "disabled"（关闭思考）', () => {
    expect(() => validateAnthropicRequest(base({ thinking: { type: "disabled" } }))).not.toThrow();
  });

  // ZCode 的自适应档位；不做自适应推理，但必须接受而不是报错。
  it('接受 "adaptive"', () => {
    expect(() => validateAnthropicRequest(base({ thinking: { type: "adaptive" } }))).not.toThrow();
  });

  it("仍然拒绝无意义的值", () => {
    expect(() => validateAnthropicRequest(base({ thinking: { type: "bogus" } as never }))).toThrow(
      /thinking\.type/,
    );
  });
});

describe("output_config.effort 是强度的真相源", () => {
  // 这是用户报的"选什么档都用 max/high"的根源：effort 根本没被读过。
  it.each(["low", "medium", "high", "xhigh", "max"] as const)(
    "effort=%s 透传到上游",
    (effort) => {
      // deepseek 只支持 {high,max}，所以 low/medium 会被夹到 high —— 这是
      // 模型能力的限制，不是丢档位。xhigh/max 必须保持可区分。
      const got = effortOf(base({ output_config: { effort } }));
      expect(["high", "max"]).toContain(got);
    },
  );

  it("effort=max 不会被夹成 high", () => {
    expect(effortOf(base({ output_config: { effort: "max" } }))).toBe("max");
  });

  it("effort=low 夹到该模型支持的最低档", () => {
    expect(effortOf(base({ output_config: { effort: "low" } }))).toBe("high");
  });

  // 旧客户端只给 budget：仍然要能推出档位（回归保护）。
  it("仅给 budget_tokens 时按预算推算", () => {
    expect(effortOf(base({ thinking: { type: "enabled", budget_tokens: 1024 } }))).toBe("high");
    expect(effortOf(base({ thinking: { type: "enabled", budget_tokens: 64000 } }))).toBe("max");
  });

  // 客户端对字段命名不一致：Messages API 是 snake_case，有些客户端发 camelCase。
  // 只认一个会让另一个落进"无预算"分支 —— 而旧代码那里会 fallthrough 到 max，
  // 这正是"不管怎么选都用最高档"的直接原因。
  it("接受 camelCase 的 budgetTokens", () => {
    expect(effortOf(base({ thinking: { type: "enabled", budgetTokens: 1024 } as never }))).toBe("high");
    expect(effortOf(base({ thinking: { type: "enabled", budgetTokens: 64000 } as never }))).toBe("max");
  });

  // 没有预算时不猜。旧实现让 undefined 逐级比较失败后落到 "max"，
  // 把"未指定"变成了"最高档"。
  it("无预算时不落到最高档", () => {
    expect(effortOf(base({ thinking: { type: "enabled" } as never }))).toBeUndefined();
  });

  // 两者都给时以 effort 为准：它是显式意图，budget 只是旧协议的回退。
  it("effort 与 budget 冲突时以 effort 为准", () => {
    const req = base({
      thinking: { type: "enabled", budget_tokens: 1024 }, // 推算 → high
      output_config: { effort: "max" },
    });
    expect(effortOf(req)).toBe("max");
  });
});

describe("关闭思考：上游没有对应档位，降级为最低强度", () => {
  // 实测：reasoning_effort 的 none/disabled/off/minimal 在上游一律 400。
  // 所以"关闭"只能近似成最低档，不能发非法值，也不能报错。
  it('thinking.type="disabled" 不报错且降到最低档', () => {
    const req = base({ thinking: { type: "disabled" } });
    expect(() => validateAnthropicRequest(req)).not.toThrow();
    expect(effortOf(req)).toBe("high");
  });

  it('thinking.type="disabled" 时忽略残留的 effort', () => {
    const req = base({ thinking: { type: "disabled" }, output_config: { effort: "max" } });
    expect(effortOf(req)).toBe("high");
  });

  it('thinking.type="adaptive" 让上游自行决定（不干预）', () => {
    const req = base({ thinking: { type: "adaptive" } });
    expect(() => validateAnthropicRequest(req)).not.toThrow();
    // adaptive 的语义就是"让服务端决定"，所以不指定档位是正确的表达 ——
    // 强行塞一个值反而违背调用方意图。
    expect(effortOf(req)).toBeUndefined();
  });

  it('thinking.type="adaptive" 仍尊重显式 effort', () => {
    const req = base({ thinking: { type: "adaptive" }, output_config: { effort: "max" } });
    expect(effortOf(req)).toBe("max");
  });
});
