import type { OpenAIChatRequest } from "./types.js";
import type { AnthropicRequest } from "./anthropic-types.js";

export class ValidationError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ValidationError";
  }
}

export function validateOpenAIChatRequest(body: unknown): OpenAIChatRequest {
  if (!body || typeof body !== "object") {
    throw new ValidationError("Request body must be a JSON object");
  }

  const req = body as Record<string, unknown>;

  if (req.model !== undefined && typeof req.model !== "string") {
    throw new ValidationError("Field 'model' must be a string");
  }

  if (!Array.isArray(req.messages)) {
    throw new ValidationError("Field 'messages' must be an array");
  }

  for (let i = 0; i < req.messages.length; i++) {
    const msg = req.messages[i];
    if (!msg || typeof msg !== "object") {
      throw new ValidationError(`messages[${i}] must be an object`);
    }
    const m = msg as Record<string, unknown>;
    const validRoles = ["system", "developer", "user", "assistant", "tool"];
    if (typeof m.role !== "string" || !validRoles.includes(m.role)) {
      throw new ValidationError(`messages[${i}].role must be one of: ${validRoles.join(", ")}`);
    }
    if (m.content === undefined && m.tool_calls === undefined) {
      throw new ValidationError(`messages[${i}] must contain either 'content' or 'tool_calls'`);
    }
    if (m.role === "tool" && typeof m.tool_call_id !== "string") {
      throw new ValidationError(`messages[${i}].tool_call_id must be a string when role is "tool"`);
    }
  }

  // Top-level scalar guards — fail fast with a local 400 instead of proxying
  // a confusing upstream 400.
  if (req.temperature !== undefined && typeof req.temperature !== "number") {
    throw new ValidationError("Field 'temperature' must be a number");
  }
  if (req.temperature !== undefined && (req.temperature < 0 || req.temperature > 2)) {
    throw new ValidationError("Field 'temperature' must be between 0 and 2");
  }
  if (req.top_p !== undefined && typeof req.top_p !== "number") {
    throw new ValidationError("Field 'top_p' must be a number");
  }
  if (req.top_p !== undefined && (req.top_p < 0 || req.top_p > 1)) {
    throw new ValidationError("Field 'top_p' must be between 0 and 1");
  }
  if (req.max_tokens !== undefined && typeof req.max_tokens !== "number") {
    throw new ValidationError("Field 'max_tokens' must be a number");
  }
  if (req.max_tokens !== undefined && (!Number.isFinite(req.max_tokens) || req.max_tokens <= 0)) {
    throw new ValidationError("Field 'max_tokens' must be a positive number");
  }
  if (req.tool_choice !== undefined) {
    const tc = req.tool_choice;
    const validStrings = new Set(["auto", "none", "required"]);
    if (typeof tc === "string" && !validStrings.has(tc)) {
      throw new ValidationError(`Field 'tool_choice' string must be one of: ${[...validStrings].join(", ")}`);
    }
    if (typeof tc === "object" && tc !== null) {
      const t = tc as Record<string, unknown>;
      if (t.type !== "function") {
        throw new ValidationError(`Field 'tool_choice.type' must be "function" when tool_choice is an object`);
      }
    }
  }

  return body as OpenAIChatRequest;
}

// ── Anthropic validation ──

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

/** Validate only fields consumed by the local estimator, not generation fields. */
export function validateCountTokensRequest(body: unknown): Record<string, unknown> {
  if (!isRecord(body)) throw new ValidationError("Request body must be a JSON object");
  if (body.system !== undefined && typeof body.system !== "string") {
    if (
      !Array.isArray(body.system) ||
      !body.system.every((b) => isRecord(b) && (b.text === undefined || typeof b.text === "string"))
    ) {
      throw new ValidationError("Field 'system' must be a string or array of text blocks");
    }
  }
  if (body.messages !== undefined) {
    if (
      !Array.isArray(body.messages) ||
      !body.messages.every(
        (m) =>
          isRecord(m) &&
          (typeof m.content === "string" ||
            (Array.isArray(m.content) && m.content.every(isRecord))),
      )
    ) {
      throw new ValidationError(
        "Field 'messages' must contain objects with string or block-array content",
      );
    }
  }
  if (body.tools !== undefined) {
    if (
      !Array.isArray(body.tools) ||
      !body.tools.every(
        (t) =>
          isRecord(t) &&
          (t.name === undefined || typeof t.name === "string") &&
          (t.description === undefined || typeof t.description === "string") &&
          (t.input_schema === undefined || isRecord(t.input_schema)),
      )
    ) {
      throw new ValidationError("Field 'tools' must be an array of tool objects");
    }
  }
  return body;
}

const UNSUPPORTED_CONTENT_TYPES = new Set([
  "document",
  "search_result",
  "web_search_tool_result",
  "web_fetch_tool_result",
  "code_execution_tool_result",
  "mcp_tool_result",
  "container_upload",
  "server_tool_use",
  "mid_conversation_system",
]);

/** Accepted `thinking.type` values (Messages API: enabled / disabled / adaptive). */
const THINKING_TYPES = new Set(["enabled", "disabled", "adaptive"]);

/** Accepted `output_config.effort` values — the discrete levels CC accepts. */
const EFFORT_LEVELS = new Set(["low", "medium", "high", "xhigh", "max"]);

const BUILT_IN_TOOL_TYPES = new Set([
  "computer_20241022",
  "bash_20241022",
  "text_editor_20241022",
  "web_search_20250305",
]);

export function validateAnthropicRequest(body: unknown): AnthropicRequest {
  if (!body || typeof body !== "object") {
    throw new ValidationError("Request body must be a JSON object");
  }

  const req = body as Record<string, unknown>;

  if (typeof req.model !== "string") {
    throw new ValidationError("Field 'model' must be a string");
  }

  if (typeof req.max_tokens !== "number") {
    throw new ValidationError("Field 'max_tokens' must be a number");
  }

  if (!Array.isArray(req.messages)) {
    throw new ValidationError("Field 'messages' must be an array");
  }

  for (let i = 0; i < req.messages.length; i++) {
    const msg = req.messages[i] as Record<string, unknown>;
    if (msg.role !== "user" && msg.role !== "assistant" && msg.role !== "system") {
      throw new ValidationError(`messages[${i}].role must be "user" or "assistant"`);
    }
    if (msg.content === undefined) {
      throw new ValidationError(`messages[${i}] must contain 'content'`);
    }
    validateContentBlocks(msg.content, i);
  }

  if (Array.isArray(req.tools)) {
    for (let i = 0; i < req.tools.length; i++) {
      const tool = req.tools[i] as Record<string, unknown>;
      const type = tool.type as string | undefined;
      if (type && BUILT_IN_TOOL_TYPES.has(type)) {
        throw new ValidationError(
          `tools[${i}]: built-in tool type "${type}" is not supported. Only custom tools ({name, description, input_schema}) are allowed.`,
        );
      }
    }
  }

  if (req.thinking && typeof req.thinking === "object") {
    const t = req.thinking as Record<string, unknown>;
    // The Messages API defines three values; a client's "thinking off" setting
    // sends "disabled" and its adaptive setting sends "adaptive". Rejecting
    // anything but "enabled" turned a normal setting into a 400 that looked
    // like a connectivity failure downstream.
    if (t.type !== undefined && !THINKING_TYPES.has(t.type as string)) {
      throw new ValidationError(
        `Field 'thinking.type' must be one of: ${[...THINKING_TYPES].join(", ")}`,
      );
    }
    // budget_tokens only exists on the "enabled" form, and is only meaningful
    // there; guard it against max_tokens so the upstream doesn't 400 later.
    if (typeof t.budget_tokens === "number" && t.budget_tokens >= (req.max_tokens as number)) {
      throw new ValidationError("thinking.budget_tokens must be less than max_tokens");
    }
  }

  if (req.output_config !== undefined) {
    const oc = req.output_config as Record<string, unknown>;
    if (oc === null || typeof oc !== "object") {
      throw new ValidationError("Field 'output_config' must be an object");
    }
    if (oc.effort !== undefined && !EFFORT_LEVELS.has(oc.effort as string)) {
      throw new ValidationError(
        `Field 'output_config.effort' must be one of: ${[...EFFORT_LEVELS].join(", ")}`,
      );
    }
  }

  // Top-level scalar guards (Anthropic)
  if (req.temperature !== undefined && typeof req.temperature !== "number") {
    throw new ValidationError("Field 'temperature' must be a number");
  }
  if (req.top_p !== undefined && typeof req.top_p !== "number") {
    throw new ValidationError("Field 'top_p' must be a number");
  }
  if (req.top_k !== undefined && typeof req.top_k !== "number") {
    throw new ValidationError("Field 'top_k' must be a number");
  }

  return body as AnthropicRequest;
}

function validateContentBlocks(content: unknown, msgIdx: number): void {
  if (typeof content === "string") return;
  if (!Array.isArray(content)) {
    throw new ValidationError(`messages[${msgIdx}].content must be a string or array of blocks`);
  }
  for (let i = 0; i < content.length; i++) {
    const block = content[i] as Record<string, unknown>;
    if (typeof block.type !== "string") continue;
    if (UNSUPPORTED_CONTENT_TYPES.has(block.type)) {
      throw new ValidationError(
        `messages[${msgIdx}].content[${i}]: block type "${block.type}" is not supported`,
      );
    }
  }
}
