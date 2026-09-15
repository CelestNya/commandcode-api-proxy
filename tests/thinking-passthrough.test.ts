// Anthropic 思考参数的透传：thinking.type 的三种取值 + output_config.effort。
//
// 真实请求体（从 ZCode 的 rollout model-io 记录里逐条解析，1160 条 v4.1-flash）：
//   thinking=enabled  effort=low      n=224    ← 绝大多数请求
//   thinking=enabled  effort=high     n=132
//   thinking=disabled (无 effort)     n=35     ← 这是"关闭思考"的真实形状
//   thinking=enabled  effort=max      n=10
//   thinking=enabled  effort=off      n=1      ← 被 400 拒的那次
//   thinking=enabled  effort=xlow     n=1
// 关键结论：UI 选"关闭"时 ZCode 发的是 `thinking.type:"disabled"` 且**不带
// output_config**；`off`/`xlow` 是手工测试值，不是 UI 路径。但两者都必须被
// 正确接住——客户端确实会发出它们，而任何非法值都会被本地校验或上游拒掉。
//
// 上游能力（CC 网关自身的 schema 错误信息，实测）：
//   · `params.reasoning_effort` 是 Zod enum，**只认五个离散字符串**：
//       Invalid option: expected one of "low"|"medium"|"high"|"xhigh"|"max"
//   · `none`/`disabled`/`off`/`minimal` 一律 400；数字（0-100）与数字字符串
//     同样 400 —— CC 不接受 DeepSeek 文档里那种连续标度。
//   · CC 的 schema 是 non-strict：未知字段（thinking / reasoning_budget /
//     thinking_budget …）被**静默丢弃**，返回 200 但完全不生效。判别方法是喂
//     非法值：仍 200 即证明该字段根本没被识别。
//     ⇒ 因此**无法**借别的字段名真正关闭思考，只能落到最低档近似。
//   · 官方 CLI 自己也把"关思考"表达为省略字段
//     （`thinkingHook: if(!n||"off"===n) return;`）。
//
// 生产 bug（2026-09-15，v0.4.3）：两层叠加，症状都是"关了思考还在大量思考"。
//   1. 校验层：`output_config.effort:"off"` 被本地校验拒成 400
//      （`must be one of: low, medium, high, xhigh, max`）。
//   2. 档位表层：models.json 里 v4.1-flash 等写成 ["high","max"]，**缺 "low"**，
//      于是最低档请求被裁剪逻辑**升档**成 high。官方 CLI 的表是 ["low","high","max"]。
//   "明明关了思考，还在大量思考"。
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

describe("effort=\"off\"：生产里被拒的那条请求", () => {
  // 这是 2026-09-15 生产日志里的原样请求形状：
  //   [reject] 400 invalid_request_error: Field 'output_config.effort' must be one of: ...
  // ZCode 的"关闭思考"档位就是这个字面值 "off"。
  const offReq = () => base({ output_config: { effort: "off" } as never });

  it("不报错（此前直接 400）", () => {
    expect(() => validateAnthropicRequest(offReq())).not.toThrow();
  });

  it("映射到该模型支持的最低档", () => {
    // base 用的 deepseek-v4-flash 只支持 {high,max}，因此最低档是 high。
    // 为什么不是"省略字段"？交错实测（4 轮 × 每条件，v4.1-flash）：
    //   显式 low = 1416 字符 < 省略 = 1705 < high = 2196 < max = 4199
    // 省略等于把选择权交还上游默认值，反而比显式最低档思考更多 ——
    // 正是用户按"关闭"想避开的东西。
    expect(effortOf(offReq())).toBe("high");
  });

  it("支持 low 档的模型上，off 落到 low（不再升到 high）", () => {
    const req = base({
      model: "deepseek/deepseek-v4.1-flash",
      output_config: { effort: "off" } as never,
    });
    expect(effortOf(req)).toBe("low");
  });

  it("与 thinking.type=\"disabled\" 同时出现时，同样落到最低档", () => {
    const req = base({ thinking: { type: "disabled" }, output_config: { effort: "off" } as never });
    expect(() => validateAnthropicRequest(req)).not.toThrow();
    expect(effortOf(req)).toBe("high");
  });

  // 大小写：客户端可能发 "OFF"。
  it("接受大写 OFF", () => {
    const req = base({ output_config: { effort: "OFF" } as never });
    expect(() => validateAnthropicRequest(req)).not.toThrow();
    expect(effortOf(req)).toBe("high");
  });

  // 仍要拦住真正无意义的值，别把校验放松成"什么都收"。
  // 注意 reasoning_effort 的 none/disabled/minimal 在上游是 400，所以这些
  // 同样按"关闭"处理；"bogus" 这种则必须是错误。
  it("仍然拒绝无意义的值", () => {
    expect(() =>
      validateAnthropicRequest(base({ output_config: { effort: "bogus" } as never })),
    ).toThrow(/output_config\.effort/);
  });
});

describe("档位表与上游官方 CLI 对齐", () => {
  // 之前 models.json 里 deepseek-v4.1-flash / GLM-5.3 等写成 ["high","max"]，
  // 少了 "low"，于是"最低档"请求会被裁剪升档到 high。官方 CLI 的表是
  // ["low","high","max"]。这里锁住实际在用的模型，防止再手抄错。
  const req = (model: string, effort: string) =>
    toCCRequest({
      model,
      max_tokens: 8192,
      messages: [{ role: "user", content: "hi" }],
      output_config: { effort } as never,
    }).params.reasoning_effort;

  it.each([
    ["deepseek/deepseek-v4.1-flash", "low"],
    ["deepseek/deepseek-v4-flash-fast", "low"],
    ["zai-org/GLM-5.3", "low"],
    ["z-ai/glm-5.3-flash", "low"],
  ])("%s 支持 low 档（不再升档成 high）", (model, effort) => {
    expect(req(model, effort)).toBe("low");
  });

  it("xai/grok-4.6 支持 xhigh（不再降档成 high）", () => {
    expect(req("xai/grok-4.6", "xhigh")).toBe("xhigh");
  });

  // deepseek-v4-pro 官方表确实只有 {high,max}，低档请求应当被夹到 high。
  it("deepseek/deepseek-v4-pro 仍把 low 夹到 high", () => {
    expect(req("deepseek/deepseek-v4-pro", "low")).toBe("high");
  });
});
