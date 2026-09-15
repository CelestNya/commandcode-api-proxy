import crypto from "node:crypto";
import { toolArgumentSuffix } from "./tool-arguments.js";
import type {
  AnthropicRequest,
  AnthropicContentBlock,
  ImageBlockParam,
  ToolResultBlockParam,
  OutputContentBlock,
  OutputToolUseBlock,
  AnthropicSSERecord,
  AnthropicStopReason,
  AnthropicResponse,
  ContentBlockStartShape,
  DeltaShape,
} from "@/translate/anthropic-types.js";
import type {
  CCMessage,
  CCContentPart,
  CCRequestBody,
  CCToolChoice,
  CCEvent,
  UsageData,
} from "@/translate/types.js";
import { resolveAnthropicModel } from "@/translate/anthropic-models.js";
import { resolveEffortForModel } from "@/translate/models.js";
import { isEffortOff } from "@/translate/validation.js";
import { extractUsage, pruneDanglingTools, buildCCConfig } from "@/translate/util.js";
import { tagStreamError, UpstreamEventError } from "@/stream.js";
import { logger } from "@/logger.js";

// ── Constants ──

const REASONING_THRESHOLDS = { LOW: 2000, MEDIUM: 8000, HIGH: 16000, XHIGH: 32000 } as const;
const ANTHROPIC_STOP_REASON_MAP: Record<string, AnthropicStopReason> = {
  stop: "end_turn",
  length: "max_tokens",
  "tool-call": "tool_use",
  "tool-calls": "tool_use",
  tool_call: "tool_use",
  content_filtered: "stop_sequence",
  pause_turn: "pause_turn",
  refusal: "refusal",
  model_context_window_exceeded: "model_context_window_exceeded",
};
const INITIAL_OUTPUT_TOKENS = 1;
/**
 * Level used when a client asks for thinking to be off — via
 * `thinking.type:"disabled"` or an "off" marker in `output_config.effort`.
 *
 * The upstream accepts no "off" value, so this is the closest expressible
 * intent: the lowest level the model supports (clipping raises it to the
 * model's floor, e.g. "high" for deepseek-v4-pro). Chosen deliberately over
 * dropping the field: measured over interleaved rounds on v4.1-flash, sending
 * "low" produced less reasoning than sending nothing at all (1416 vs 1705
 * chars), because omitting the field hands the choice back to the upstream's
 * own default — exactly what a user turning thinking off is trying to avoid.
 */
const DISABLED_THINKING_EFFORT = "low";
/**
 * CC returns no thinking-block signature, but Anthropic's contract requires one
 * on every thinking block. Clients round-trip it without verifying, so a fixed
 * placeholder is safe.
 */
const THINKING_SIGNATURE = "_cc_proxy_placeholder";

// ── Request translator ──

function toCCMessages(messages: AnthropicRequest["messages"]): {
  ccMessages: CCMessage[];
  systemPrompt: string | undefined;
} {
  const toolUseIdToName = new Map<string, string>();

  for (const msg of messages) {
    if (msg.role !== "assistant" || !Array.isArray(msg.content)) continue;
    for (const block of msg.content as AnthropicContentBlock[]) {
      if (block.type === "tool_use") {
        toolUseIdToName.set(block.id, block.name);
      }
    }
  }

  const ccMessages: CCMessage[] = [];
  const systemParts: string[] = [];

  for (const msg of messages) {
    const content = msg.content;

    if (msg.role === "system") {
      if (typeof content === "string") {
        systemParts.push(content);
      }
      continue;
    }

    if (msg.role === "user") {
      if (typeof content === "string") {
        ccMessages.push({ role: "user", content });
      } else {
        const blocks = content as AnthropicContentBlock[];
        const hasToolResult = blocks.some((b) => b.type === "tool_result");
        if (hasToolResult) {
          const textParts: CCContentPart[] = [];
          for (const block of blocks) {
            if (block.type === "tool_result") {
              const trb = block as ToolResultBlockParam;
              const name = toolUseIdToName.get(trb.tool_use_id) ?? "";
              const resultText =
                typeof trb.content === "string"
                  ? trb.content
                  : (trb.content as Array<{ type: string; text?: string }>)
                      .map((p) => p.text ?? "")
                      .join("");
              ccMessages.push({
                role: "tool",
                content: [
                  {
                    type: "tool-result",
                    toolCallId: trb.tool_use_id,
                    toolName: name,
                    output: { type: "text", value: resultText },
                    isError: trb.is_error,
                  },
                ],
              });
            } else {
              const part = toCCPartByBlock(block);
              if (part) textParts.push(part);
            }
          }
          if (textParts.length > 0) {
            ccMessages.push({ role: "user", content: textParts });
          }
        } else {
          const parts = blocks.map((b) => toCCPartByBlock(b)).filter(Boolean) as CCContentPart[];
          ccMessages.push({ role: "user", content: parts });
        }
      }
      continue;
    }

    // assistant
    if (typeof content === "string") {
      ccMessages.push({ role: "assistant", content });
    } else if (Array.isArray(content)) {
      const parts: CCContentPart[] = [];
      for (const block of content as AnthropicContentBlock[]) {
        if (block.type === "text") {
          parts.push({ type: "text", text: block.text });
        } else if (block.type === "tool_use") {
          parts.push({
            type: "tool-call",
            toolCallId: block.id,
            toolName: block.name,
            input: block.input,
          });
        }
      }
      if (parts.length > 0) {
        ccMessages.push({ role: "assistant", content: parts });
      }
    }
  }

  return {
    ccMessages: pruneDanglingTools(ccMessages),
    systemPrompt: systemParts.length > 0 ? systemParts.join("\n\n") : undefined,
  };
}

function toCCPartByBlock(block: AnthropicContentBlock): CCContentPart | null {
  if (block.type === "text") return { type: "text", text: block.text };
  if (block.type === "image") {
    const src = (block as ImageBlockParam).source;
    if (src.type === "base64") {
      return { type: "image", image: `data:${src.media_type};base64,${src.data}` };
    }
    return { type: "image", image: src.url };
  }
  return null;
}

/**
 * Reasoning strength the upstream should use, from whichever field the client
 * expressed it in.
 *
 * Priority order, highest first:
 *   1. `thinking.type === "disabled"` → the client asked for no extended
 *      thinking. The upstream has no such level (measured: `none`, `disabled`,
 *      `off`, `minimal` all 400), so this degrades to the lowest level the
 *      model supports rather than failing the turn.
 *   2. `output_config.effort` → the explicit level. This is what current
 *      clients send; reading only `budget_tokens` (as this used to) silently
 *      discarded the user's choice and sent the same level every time.
 *      An "off" marker here (see isEffortOff) means the same as case 1.
 *   3. `thinking.budget_tokens` (also accepted as `budgetTokens`) → the older
 *      convention, mapped onto a level.
 *
 * A missing budget yields no level at all. The previous version returned "max"
 * in that case — `undefined <= 2000` is false, so every comparison fell
 * through to the last branch — which is why every request looked like it was
 * pinned to the highest level.
 */
function resolveReasoningEffort(req: AnthropicRequest): string | undefined {
  const thinking = req.thinking;
  const explicit = req.output_config?.effort;
  // An "off" marker on either channel means "no extended thinking". Mapping it
  // to the model's floor beats dropping the field: measured on v4.1-flash, the
  // floor produced less reasoning than sending nothing (1416 vs 1705 chars
  // median-ish over 4 interleaved rounds) — omitting hands the choice back to
  // the upstream's own default, which is what the user was trying to leave.
  if (thinking?.type === "disabled" || isEffortOff(explicit)) return DISABLED_THINKING_EFFORT;
  if (explicit) return explicit;
  if (!thinking || thinking.type !== "enabled") return undefined;

  // Clients differ on the spelling: the Messages API field is snake_case, some
  // send camelCase. Accept both rather than silently ignoring one.
  const t = thinking as { budget_tokens?: unknown; budgetTokens?: unknown };
  const raw = t.budget_tokens ?? t.budgetTokens;
  const b = typeof raw === "number" && Number.isFinite(raw) ? raw : undefined;
  if (b === undefined) return undefined;

  if (b <= REASONING_THRESHOLDS.LOW) return "low";
  if (b <= REASONING_THRESHOLDS.MEDIUM) return "medium";
  if (b <= REASONING_THRESHOLDS.HIGH) return "high";
  if (b <= REASONING_THRESHOLDS.XHIGH) return "xhigh";
  return "max";
}

/**
 * Map an Anthropic `tool_choice` to the object form CC's bridge requires.
 * CC accepts `{type:"auto"|"any"|"tool", name?}` only. Anthropic "any" → "any";
 * Anthropic "none" has no CC equivalent, so we omit it (default auto).
 */
function resolveToolChoice(anthropic: AnthropicRequest): CCToolChoice | undefined {
  const tc = anthropic.tool_choice;
  if (!tc || tc.type === "auto" || tc.type === "none") return undefined;
  if (tc.type === "any") return { type: "any" };
  if (tc.type === "tool") return { type: "tool", name: tc.name };
  return undefined;
}

export function toCCRequest(
  req: AnthropicRequest,
  configOverrides?: Partial<CCRequestBody["config"]>,
): CCRequestBody {
  const { ccMessages, systemPrompt } = toCCMessages(req.messages);

  let systemText: string | undefined;
  if (typeof req.system === "string") {
    systemText = req.system;
  } else if (Array.isArray(req.system)) {
    systemText = req.system
      .filter((b) => b.type === "text")
      .map((b) => (b.type === "text" ? b.text : ""))
      .filter(Boolean)
      .join("\n\n");
  }

  const resolvedModel = resolveAnthropicModel(req.model);

  const ccTools = req.tools?.map((t) => ({
    name: t.name,
    description: t.description,
    input_schema: t.input_schema,
  }));

  const finalSystem = systemText
    ? systemPrompt != null
      ? `${systemPrompt}\n\n${systemText}`
      : systemText
    : systemPrompt;

  const body: CCRequestBody = {
    config: buildCCConfig(configOverrides),
    memory: "",
    taste: "",
    skills: "",
    permissionMode: "standard",
    params: {
      model: resolvedModel,
      messages: ccMessages,
      stream: req.stream ?? false,
      max_tokens: req.max_tokens,
      temperature: req.temperature,
      top_p: req.top_p,
      stop: req.stop_sequences,
      tools: ccTools,
      tool_choice: resolveToolChoice(req),
      reasoning_effort: resolveEffortForModel(resolvedModel, resolveReasoningEffort(req)),
    },
    threadId: crypto.randomUUID(),
  };

  // Add system prompt from top-level field
  if (finalSystem) {
    body.params.system = finalSystem;
  }

  return body;
}

// ── Streaming encoder ──

export class AnthropicStreamEncoder {
  readonly messageId: string;
  private blockIndex = 0;
  private currentBlockIndex = 0;
  private currentBlockType: "text" | "thinking" | "tool_use" | null = null;
  private readonly toolBlocks = new Map<
    string,
    { index: number; arguments: string; closed: boolean }
  >();
  private pendingStart: CCEvent | null = null;
  private started = false;
  private pinged = false;
  private sawFinish = false;
  /** finish 事件里归一后的用量（含缓存命中），供服务层落统计日志 */
  lastUsage?: UsageData;

  constructor(private readonly model: string) {
    this.messageId = `msg_${crypto.randomUUID()}`;
  }

  get finished(): boolean {
    return this.sawFinish;
  }

  /**
   * Whether the client has received anything worth keeping.
   *
   * `started` only flips when a content event is emitted — an upstream `start`
   * is buffered in `pendingStart`, and `message_start` is an empty envelope the
   * replacement stream re-sends. So false means a failed attempt can be
   * re-sent invisibly: the client sees one continuous response.
   */
  get hasEmittedContent(): boolean {
    return this.started;
  }

  emit(event: CCEvent): AnthropicSSERecord[] {
    // 终态守卫：error/finish 已发出 message_stop，message_stop 之后不允许
    // 再有任何记录——行为不端的上游（终态后继续发事件）到此全部吞掉，
    // 否则客户端会看到重复收尾和 stop 后的块记录。
    if (this.sawFinish) return [];
    if (event.type === "start") {
      this.blockIndex = 0;
      this.currentBlockType = null;
      this.pendingStart = event;
      return [];
    }

    if (event.type === "error") {
      const msg =
        (event.data.message as string) ??
        (event.data.error as { message?: string } | undefined)?.message ??
        JSON.stringify(event.data);
      logger.error(`[CC upstream error] ${msg}`);
      // Nothing has been written downstream yet → this is recoverable: throw so
      // the service layer can re-send the request and the client sees a single
      // clean response. Once output exists the failure must be reported in-band
      // (a retry would duplicate it), so fall through to the error records.
      if (!this.started) throw new UpstreamEventError(msg);

      this.sawFinish = true;
      const records: AnthropicSSERecord[] = [];
      this.closeCurrentBlock(records);
      this.closeToolBlocks(records);
      // 统一发 overloaded_error：这是下游分类器唯一写死为可重试的 in-band
      // 错误类型（isRetryable: type==="overloaded_error"），api_error 一律
      // retryable:false 会把重试额度全部作废。此处已投递过内容，重发会让用户
      // 看到重复，只能如实报告。真实错误来源保留在 message 前缀，供人区分。
      records.push({
        event: "error",
        data: {
          type: "error",
          error: { type: "overloaded_error", message: `[upstream-error] ${msg}` },
        },
      });
      records.push({ event: "message_stop", data: { type: "message_stop" } });
      return records;
    }

    if (event.type === "finish") {
      return this.handleFinish(event);
    }

    // Content event
    if (!this.started) {
      const records: AnthropicSSERecord[] = [];
      const startInputTokens = this.pendingStart
        ? (
            this.pendingStart.data.totalUsage as Record<string, unknown> as
              | {
                  inputTokens?: number;
                }
              | undefined
          )?.inputTokens
        : undefined;

      records.push(this.makeMessageStart(startInputTokens ?? 0));
      this.started = true;
      return [...records, ...this.handleContent(event)];
    }

    return this.handleContent(event);
  }

  private handleFinish(event: CCEvent): AnthropicSSERecord[] {
    this.sawFinish = true;
    const records: AnthropicSSERecord[] = [];

    // If we never emitted a content event, `started` is still false and no
    // message_start was sent. The Anthropic SDK requires message_start as
    // the first event — synthesize one before the closing records so we
    // don't deliver a stream that starts with message_delta.
    if (!this.started) {
      records.push(this.makeMessageStart(0));
      this.started = true;
    }

    this.closeCurrentBlock(records);
    this.closeToolBlocks(records);

    const finishReason = (event.data.finishReason as string) ?? "stop";
    const usage = extractUsage(event.data as Record<string, unknown>);
    this.lastUsage = usage;

    // CC's `start` event carries no usage — input/output token counts are only
    // known at `finish`. Anthropic's SDK merges `message_delta.usage` over the
    // `message_start.usage`, so emitting them here corrects the final values
    // (message_start reported input_tokens as 0 because start was empty).
    const cachedTokens = usage?.promptTokensDetails?.cachedTokens;
    records.push({
      event: "message_delta",
      data: {
        type: "message_delta",
        delta: {
          stop_reason: ANTHROPIC_STOP_REASON_MAP[finishReason] ?? "end_turn",
          stop_sequence: null,
        },
        usage: {
          input_tokens: usage?.promptTokens ?? 0,
          output_tokens: usage?.completionTokens ?? 0,
          ...(cachedTokens != null ? { cache_read_input_tokens: cachedTokens } : {}),
        },
      },
    });

    records.push({ event: "message_stop", data: { type: "message_stop" } });
    return records;
  }

  private handleContent(event: CCEvent): AnthropicSSERecord[] {
    const records: AnthropicSSERecord[] = [];

    switch (event.type) {
      case "text-delta":
        this.ensureBlockOpen(records, "text", () => ({ type: "text", text: "" }));
        records.push(
          this.makeDelta({ type: "text_delta", text: (event.data.text as string) ?? "" }),
        );
        break;

      case "reasoning-delta":
        this.ensureBlockOpen(records, "thinking", () => ({
          type: "thinking",
          thinking: "",
        }));
        records.push(
          this.makeDelta({ type: "thinking_delta", thinking: (event.data.text as string) ?? "" }),
        );
        break;

      case "tool-call-delta": {
        const tcId = (event.data.toolCallId as string) ?? "";
        const tcName = (event.data.name as string) ?? "";
        const block = this.ensureToolBlock(records, tcId, tcName);
        if (block.closed) throw new Error("Inconsistent upstream tool arguments");
        const args = (event.data.arguments as string) ?? "";
        block.arguments += args;
        records.push(
          this.makeDelta(
            {
              type: "input_json_delta",
              partial_json: args,
            },
            block.index,
          ),
        );
        break;
      }

      case "tool-call": {
        const tcId = (event.data.toolCallId as string) ?? "";
        const tcName = (event.data.toolName as string) ?? (event.data.name as string) ?? "";
        const input = event.data.input ?? event.data.arguments;
        const argsStr =
          typeof input === "string" ? input : input != null ? JSON.stringify(input) : "";
        const block = this.ensureToolBlock(records, tcId, tcName);
        const suffix = toolArgumentSuffix(block.arguments, argsStr);
        if (suffix) {
          if (block.closed) throw new Error("Inconsistent upstream tool arguments");
          records.push(
            this.makeDelta({ type: "input_json_delta", partial_json: suffix }, block.index),
          );
          block.arguments += suffix;
        }
        this.closeToolBlock(records, block);
        break;
      }
    }

    return records;
  }

  private ensureToolBlock(records: AnthropicSSERecord[], id: string, name: string) {
    const existing = this.toolBlocks.get(id);
    if (existing) return existing;
    this.closeCurrentBlock(records);
    this.ensureBlockOpenWith(records, "tool_use", { type: "tool_use", id, name, input: {} });
    const block = { index: this.currentBlockIndex, arguments: "", closed: false };
    this.toolBlocks.set(id, block);
    // Tool blocks have independent lifetimes: interleaved deltas retain indices.
    this.currentBlockType = null;
    return block;
  }

  private closeToolBlock(
    records: AnthropicSSERecord[],
    block: { index: number; closed: boolean },
  ): void {
    if (block.closed) return;
    records.push({
      event: "content_block_stop",
      data: { type: "content_block_stop", index: block.index },
    });
    block.closed = true;
  }

  private closeToolBlocks(records: AnthropicSSERecord[]): void {
    for (const block of this.toolBlocks.values()) this.closeToolBlock(records, block);
  }

  private ensureBlockOpen(
    records: AnthropicSSERecord[],
    type: "text" | "thinking" | "tool_use",
    blockFactory: () => ContentBlockStartShape,
  ): void {
    if (this.currentBlockType === type) return;
    this.closeCurrentBlock(records);

    this.ensureBlockOpenWith(records, type, blockFactory());
  }

  private ensureBlockOpenWith(
    records: AnthropicSSERecord[],
    type: "text" | "thinking" | "tool_use",
    block: ContentBlockStartShape,
  ): void {
    this.currentBlockType = type;
    this.currentBlockIndex = this.blockIndex++;
    records.push({
      event: "content_block_start",
      data: {
        type: "content_block_start",
        index: this.currentBlockIndex,
        content_block: block,
      },
    });

    if (!this.pinged) {
      this.pinged = true;
      records.push({ event: "ping", data: { type: "ping" } });
    }
  }

  private closeCurrentBlock(records: AnthropicSSERecord[]): void {
    if (this.currentBlockType === null) return;

    if (this.currentBlockType === "thinking") {
      // `signature_delta` is a *delta type*, not a Messages API event name.
      // Emitting it as a top-level event makes strict clients (which parse
      // each record against a union keyed on `type`) abort the whole stream,
      // surfacing to the user as a turn that stops with no message.
      records.push(
        this.makeDelta({ type: "signature_delta", signature: THINKING_SIGNATURE }),
      );
    }

    records.push({
      event: "content_block_stop",
      data: { type: "content_block_stop", index: this.currentBlockIndex },
    });

    this.currentBlockType = null;
  }

  private makeDelta(delta: DeltaShape, index = this.currentBlockIndex): AnthropicSSERecord {
    return {
      event: "content_block_delta",
      data: { type: "content_block_delta", index, delta },
    };
  }

  private makeMessageStart(inputTokens: number): AnthropicSSERecord {
    return {
      event: "message_start",
      data: {
        type: "message_start",
        message: {
          id: this.messageId,
          type: "message",
          role: "assistant",
          model: this.model,
          content: [],
          stop_reason: null,
          stop_sequence: null,
          usage: {
            input_tokens: inputTokens,
            output_tokens: INITIAL_OUTPUT_TOKENS,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            service_tier: "standard",
          },
        },
      },
    };
  }

  /**
   * Build the closing records for a stream that ended without a `finish`
   * event from the upstream (e.g. upstream connection dropped mid-response).
   * Emits synthetic message_delta + message_stop so the Anthropic SDK sees
   * a well-formed end-of-stream instead of a truncated response.
   */
  finishRecords(stopReason: AnthropicStopReason = "end_turn"): AnthropicSSERecord[] {
    const records: AnthropicSSERecord[] = [];
    if (!this.started) {
      records.push(this.makeMessageStart(0));
      this.started = true;
    }
    this.closeCurrentBlock(records);
    this.closeToolBlocks(records);
    records.push({
      event: "message_delta",
      data: {
        type: "message_delta",
        delta: { stop_reason: stopReason, stop_sequence: null },
        usage: { input_tokens: 0, output_tokens: 0 },
      },
    });
    records.push({ event: "message_stop", data: { type: "message_stop" } });
    return records;
  }

  /**
   * 失败收尾：报告错误并终止流，**不伪造成功**。
   *
   * 为什么不复用 `finishRecords`：它会在 error 之前/之后补一条
   * `message_delta(stop_reason="end_turn")`，而客户端把 `message_delta` 当作
   * "模型正常说完"的权威信号 —— 实测（conformance/client-probes）结果是
   * `finishReason` 变成 `"stop"`，于是"中途断流"被洗成一次成功回复。这是本项目
   * 反复强调的最坏失败模式，不能留在自己的代码里。
   *
   * 协议要求 `message_start` 必须是第一条记录，所以未开始时先补一个空信封。
   * error 之后直接 message_stop，让客户端的 finishReason 保持非 stop 的取值。
   */
  errorRecords(err: Error, closeBlocks = true): AnthropicSSERecord[] {
    this.sawFinish = true;
    const records: AnthropicSSERecord[] = [];
    if (!this.started) {
      records.push(this.makeMessageStart(0));
      this.started = true;
    }
    // 已开的内容块必须关闭，否则客户端认为流在半途断掉（块永远不收尾）。
    if (closeBlocks) {
      this.closeCurrentBlock(records);
      this.closeToolBlocks(records);
    }
    records.push({
      event: "error",
      data: {
        type: "error",
        error: { type: "overloaded_error", message: tagStreamError(err) },
      },
    });
    records.push({ event: "message_stop", data: { type: "message_stop" } });
    return records;
  }
}

// ── Non-streaming response builder ──

export function buildAnthropicResponse(
  events: CCEvent[],
  model: string,
  messageId: string,
): AnthropicResponse {
  let textContent = "";
  let thinkingContent = "";
  const toolUseBlocks: OutputToolUseBlock[] = [];

  for (const event of events) {
    switch (event.type) {
      case "error":
        // Do not turn failed generations (or private diagnostics) into content.
        throw new Error("CC upstream generation failed");
      case "text-delta":
        textContent += (event.data.text as string) ?? "";
        break;
      case "reasoning-delta":
        thinkingContent += (event.data.text as string) ?? "";
        break;
      case "tool-call": {
        const input = event.data.input ?? event.data.arguments;
        toolUseBlocks.push({
          type: "tool_use",
          id: (event.data.toolCallId as string) ?? "",
          name: (event.data.toolName as string) ?? (event.data.name as string) ?? "",
          input:
            typeof input === "object" && input != null ? (input as Record<string, unknown>) : {},
        });
        break;
      }
      case "finish":
        break;
    }
  }

  // Block ordering follows Anthropic's extended-thinking contract:
  // thinking blocks must precede the text they reason about, and tool_use
  // blocks come last. Mixing this up confuses strict clients (Claude Code
  // uses thinking-block position to continue reasoning across turns).
  const content: OutputContentBlock[] = [];
  if (thinkingContent) {
    content.push({
      type: "thinking",
      thinking: thinkingContent,
      signature: THINKING_SIGNATURE,
    });
  }
  if (textContent) content.push({ type: "text", text: textContent });
  content.push(...toolUseBlocks);
  // Anthropic requires content to be non-empty — if there was no text, no
  // thinking, and no tool calls (e.g. empty refusal), synthesize an empty
  // text block rather than sending an empty array.
  if (content.length === 0) content.push({ type: "text", text: "" });

  const finishEvent = events.find((e) => e.type === "finish");
  const finishReason = (finishEvent?.data.finishReason as string) ?? "stop";
  const usage = finishEvent ? extractUsage(finishEvent.data as Record<string, unknown>) : undefined;

  return {
    id: messageId,
    type: "message",
    role: "assistant",
    model,
    content,
    stop_reason: ANTHROPIC_STOP_REASON_MAP[finishReason] ?? "end_turn",
    stop_sequence: null,
    usage: {
      input_tokens: usage?.promptTokens ?? 0,
      output_tokens: usage?.completionTokens ?? 0,
      cache_creation_input_tokens: 0,
      cache_read_input_tokens: usage?.promptTokensDetails?.cachedTokens ?? 0,
    },
  };
}
