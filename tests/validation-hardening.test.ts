// 请求校验收紧后的边界用例：把原本会一路透传到上游、再由上游返回
// 400 的畸形请求，拦在本地并给出明确的 ValidationError。
import { describe, it, expect } from "vitest";
import {
  validateOpenAIChatRequest,
  validateAnthropicRequest,
  ValidationError,
} from "@/translate/validation.js";

const baseOpenAI = (overrides: Record<string, unknown> = {}) => ({
  model: "m",
  messages: [{ role: "user", content: "hi" }],
  ...overrides,
});

describe("validateOpenAIChatRequest hardening", () => {
  it("accepts a minimal valid request", () => {
    expect(() => validateOpenAIChatRequest(baseOpenAI())).not.toThrow();
  });

  it("requires tool_call_id on role=tool messages", () => {
    expect(() =>
      validateOpenAIChatRequest(
        baseOpenAI({ messages: [{ role: "tool", content: "result" }] }),
      ),
    ).toThrow(ValidationError);
  });

  it("accepts a tool message that carries tool_call_id", () => {
    expect(() =>
      validateOpenAIChatRequest(
        baseOpenAI({ messages: [{ role: "tool", content: "r", tool_call_id: "call_1" }] }),
      ),
    ).not.toThrow();
  });

  it("rejects a non-numeric temperature", () => {
    expect(() => validateOpenAIChatRequest(baseOpenAI({ temperature: "hot" }))).toThrow(
      /temperature/,
    );
  });

  it("rejects an out-of-range temperature", () => {
    expect(() => validateOpenAIChatRequest(baseOpenAI({ temperature: 3 }))).toThrow(/between/);
  });

  it("rejects an out-of-range top_p", () => {
    expect(() => validateOpenAIChatRequest(baseOpenAI({ top_p: 1.5 }))).toThrow(/between/);
  });

  it("rejects a non-positive max_tokens", () => {
    expect(() => validateOpenAIChatRequest(baseOpenAI({ max_tokens: 0 }))).toThrow(/positive/);
  });

  it("rejects an unknown tool_choice string", () => {
    expect(() => validateOpenAIChatRequest(baseOpenAI({ tool_choice: "whatever" }))).toThrow(
      /tool_choice/,
    );
  });

  it("accepts the documented tool_choice strings", () => {
    for (const tc of ["auto", "none", "required"]) {
      expect(() => validateOpenAIChatRequest(baseOpenAI({ tool_choice: tc }))).not.toThrow();
    }
  });

  it("rejects an object tool_choice that is not a function selector", () => {
    expect(() =>
      validateOpenAIChatRequest(baseOpenAI({ tool_choice: { type: "bogus", name: "x" } })),
    ).toThrow(/function/);
  });
});

const baseAnthropic = (overrides: Record<string, unknown> = {}) => ({
  model: "m",
  max_tokens: 100,
  messages: [{ role: "user", content: "hi" }],
  ...overrides,
});

describe("validateAnthropicRequest hardening", () => {
  it("accepts a minimal valid request", () => {
    expect(() => validateAnthropicRequest(baseAnthropic())).not.toThrow();
  });

  it("rejects a non-numeric temperature", () => {
    expect(() => validateAnthropicRequest(baseAnthropic({ temperature: [] }))).toThrow(
      /temperature/,
    );
  });

  it("rejects a non-numeric top_p / top_k", () => {
    expect(() => validateAnthropicRequest(baseAnthropic({ top_p: "x" }))).toThrow(/top_p/);
    expect(() => validateAnthropicRequest(baseAnthropic({ top_k: "x" }))).toThrow(/top_k/);
  });

  it("rejects thinking.type other than enabled", () => {
    expect(() =>
      validateAnthropicRequest(
        baseAnthropic({ max_tokens: 10000, thinking: { type: "disabled", budget_tokens: 100 } }),
      ),
    ).toThrow(/thinking\.type/);
  });

  it("still enforces budget_tokens < max_tokens", () => {
    expect(() =>
      validateAnthropicRequest(
        baseAnthropic({ max_tokens: 100, thinking: { type: "enabled", budget_tokens: 100 } }),
      ),
    ).toThrow(/budget_tokens/);
  });
});
