// Translation-layer golden samples: request → CC body, and the model/alias
// resolution rules. These are pure functions, so they can be captured exactly
// and replayed by the Rust implementation without any network involved.
//
// Usage:
//   node conformance/record-translate.mjs          (write)
//   node conformance/record-translate.mjs --check  (compare)

import { mkdirSync, readFileSync, writeFileSync, existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.dirname(HERE);
const OUT = path.join(HERE, "golden", "translate.json");
const CHECK = process.argv.includes("--check");

/** Dynamic import needs a file:// URL — a bare Windows path is not a valid specifier. */
const load = (rel) => import(pathToFileURL(path.join(ROOT, "dist", ...rel)).href);

// dist/ is the shipped artifact — record against it so the sample describes
// what actually runs, not what the sources say.
const { toCCRequest: openAIToCC } = await load(["translate", "openai.js"]);
const { toCCRequest: anthropicToCC } = await load(["translate", "anthropic.js"]);
const { resolveModel } = await load(["translate", "models.js"]);

const OPENAI_CASES = {
  "minimal-text": {
    model: "deepseek-v4-flash",
    messages: [{ role: "user", content: "hello" }],
    max_tokens: 64,
  },
  "system-string": {
    model: "deepseek-v4-flash",
    messages: [
      { role: "system", content: "be brief" },
      { role: "user", content: "hi" },
    ],
    max_tokens: 64,
  },
  "multi-turn": {
    model: "deepseek-v4-flash",
    messages: [
      { role: "user", content: "first" },
      { role: "assistant", content: "reply" },
      { role: "user", content: "second" },
    ],
    max_tokens: 64,
  },
  "tool-roundtrip": {
    model: "deepseek-v4-flash",
    max_tokens: 128,
    messages: [
      { role: "user", content: "what time is it" },
      {
        role: "assistant",
        content: null,
        tool_calls: [
          {
            id: "call_1",
            type: "function",
            function: { name: "get_time", arguments: '{"tz":"UTC"}' },
          },
        ],
      },
      { role: "tool", tool_call_id: "call_1", content: "12:00" },
    ],
    tools: [
      {
        type: "function",
        function: {
          name: "get_time",
          description: "Get the current time",
          parameters: {
            type: "object",
            properties: { tz: { type: "string" } },
            required: ["tz"],
          },
        },
      },
    ],
  },
  "parallel-tools": {
    model: "deepseek-v4-flash",
    max_tokens: 128,
    messages: [
      { role: "user", content: "two calls" },
      {
        role: "assistant",
        content: null,
        tool_calls: [
          { id: "a", type: "function", function: { name: "first", arguments: "{}" } },
          { id: "b", type: "function", function: { name: "second", arguments: '{"n":2}' } },
        ],
      },
      { role: "tool", tool_call_id: "a", content: "ra" },
      { role: "tool", tool_call_id: "b", content: "rb" },
    ],
    tools: [
      { type: "function", function: { name: "first", parameters: { type: "object" } } },
      { type: "function", function: { name: "second", parameters: { type: "object" } } },
    ],
  },
  "image-base64": {
    model: "deepseek-v4-flash",
    max_tokens: 64,
    messages: [
      {
        role: "user",
        content: [
          { type: "text", text: "what is this" },
          { type: "image_url", image_url: { url: "data:image/png;base64,iVBORw0KGgo=" } },
        ],
      },
    ],
  },
  "image-url-passthrough": {
    model: "deepseek-v4-flash",
    max_tokens: 64,
    messages: [
      {
        role: "user",
        content: [{ type: "image_url", image_url: { url: "https://example.test/a.png" } }],
      },
    ],
  },
  "reasoning-effort-max": {
    model: "deepseek/deepseek-v4-pro",
    max_tokens: 64,
    reasoning_effort: "max",
    messages: [{ role: "user", content: "think hard" }],
  },
  "reasoning-effort-clipped": {
    model: "xai/grok-4.5",
    max_tokens: 64,
    reasoning_effort: "max",
    messages: [{ role: "user", content: "think" }],
  },
  // Effort clipping is two steps: a rank ordering picks the nearest legal
  // level, and the model's own effort set bounds it. Pin both directions —
  // "low" must rise to the model's minimum, "max" must fall to its maximum.
  "reasoning-effort-clipped-up": {
    model: "deepseek/deepseek-v4-pro",
    max_tokens: 64,
    reasoning_effort: "low",
    messages: [{ role: "user", content: "think a little" }],
  },
  "reasoning-effort-uncatalogued-model": {
    model: "totally-unknown-model",
    max_tokens: 64,
    reasoning_effort: "max",
    messages: [{ role: "user", content: "think" }],
  },
  "tool-choice-object": {
    model: "deepseek-v4-flash",
    max_tokens: 64,
    tool_choice: { type: "function", function: { name: "get_time" } },
    messages: [{ role: "user", content: "time" }],
    tools: [
      { type: "function", function: { name: "get_time", parameters: { type: "object" } } },
    ],
  },
  "dangling-tool-call-pruned": {
    model: "deepseek-v4-flash",
    max_tokens: 64,
    messages: [
      { role: "user", content: "go" },
      {
        role: "assistant",
        content: null,
        tool_calls: [
          { id: "orphan", type: "function", function: { name: "never_ran", arguments: "{}" } },
        ],
      },
    ],
  },
  "no-tools-guard": {
    model: "deepseek-v4-flash",
    max_tokens: 64,
    messages: [{ role: "user", content: "plain chat" }],
  },
};

const ANTHROPIC_CASES = {
  "minimal-text": {
    model: "deepseek-v4-flash",
    max_tokens: 64,
    messages: [{ role: "user", content: "hello" }],
  },
  "system-string": {
    model: "deepseek-v4-flash",
    max_tokens: 64,
    system: "be brief",
    messages: [{ role: "user", content: "hi" }],
  },
  "system-blocks": {
    model: "deepseek-v4-flash",
    max_tokens: 64,
    system: [{ type: "text", text: "part one. " }, { type: "text", text: "part two." }],
    messages: [{ role: "user", content: "hi" }],
  },
  "tool-use-and-result": {
    model: "deepseek-v4-flash",
    max_tokens: 128,
    messages: [
      { role: "user", content: "what time is it" },
      {
        role: "assistant",
        content: [
          { type: "tool_use", id: "tu_1", name: "get_time", input: { tz: "UTC" } },
        ],
      },
      {
        role: "user",
        content: [{ type: "tool_result", tool_use_id: "tu_1", content: "12:00" }],
      },
    ],
    tools: [
      {
        name: "get_time",
        description: "Get the time",
        input_schema: { type: "object", properties: { tz: { type: "string" } } },
      },
    ],
  },
  "thinking-budget": {
    model: "deepseek/deepseek-v4-pro",
    max_tokens: 4000,
    thinking: { type: "enabled", budget_tokens: 2000 },
    messages: [{ role: "user", content: "think" }],
  },
  // One case per threshold band: the raw budget maps to an effort level first
  // (b<=2000 low, <=8000 medium, <=16000 high, <=32000 xhigh, else max), then
  // that level is clipped to the model's own set. Recording each band pins the
  // composition of the two steps, which is easy to get wrong by looking at
  // either step alone.
  "thinking-budget-band-low": {
    model: "deepseek/deepseek-v4-pro",
    max_tokens: 20000,
    thinking: { type: "enabled", budget_tokens: 2000 },
    messages: [{ role: "user", content: "think" }],
  },
  "thinking-budget-band-medium": {
    model: "deepseek/deepseek-v4-pro",
    max_tokens: 20000,
    thinking: { type: "enabled", budget_tokens: 8000 },
    messages: [{ role: "user", content: "think" }],
  },
  "thinking-budget-band-high": {
    model: "deepseek/deepseek-v4-pro",
    max_tokens: 40000,
    thinking: { type: "enabled", budget_tokens: 16000 },
    messages: [{ role: "user", content: "think" }],
  },
  "thinking-budget-band-xhigh": {
    model: "deepseek/deepseek-v4-pro",
    max_tokens: 60000,
    thinking: { type: "enabled", budget_tokens: 32000 },
    messages: [{ role: "user", content: "think" }],
  },
  "thinking-budget-band-max": {
    model: "deepseek/deepseek-v4-pro",
    max_tokens: 90000,
    thinking: { type: "enabled", budget_tokens: 60000 },
    messages: [{ role: "user", content: "think" }],
  },
  // A model whose set is wider than the band mapping, so clipping is a no-op.
  "thinking-budget-wide-set": {
    model: "xai/grok-4.5",
    max_tokens: 20000,
    thinking: { type: "enabled", budget_tokens: 8000 },
    messages: [{ role: "user", content: "think" }],
  },
  "thinking-clipped-grok": {
    model: "xai/grok-4.5",
    max_tokens: 4000,
    thinking: { type: "enabled", budget_tokens: 2000 },
    messages: [{ role: "user", content: "think" }],
  },
  "thinking-dropped-from-history": {
    model: "deepseek-v4-flash",
    max_tokens: 64,
    messages: [
      { role: "user", content: "q" },
      {
        role: "assistant",
        content: [
          { type: "thinking", thinking: "internal", signature: "sig" },
          { type: "text", text: "visible" },
        ],
      },
      { role: "user", content: "again" },
    ],
  },
  "image-base64": {
    model: "deepseek-v4-flash",
    max_tokens: 64,
    messages: [
      {
        role: "user",
        content: [
          { type: "text", text: "what is this" },
          {
            type: "image",
            source: { type: "base64", media_type: "image/png", data: "iVBORw0KGgo=" },
          },
        ],
      },
    ],
  },
  "claude-model-mapped": {
    model: "claude-sonnet-4-5",
    max_tokens: 64,
    messages: [{ role: "user", content: "hi" }],
  },
};

const ALIAS_CASES = [
  "deepseek-v4-pro",
  "deepseek-v4",
  "deepseek-pro",
  "deepseek-v4-flash",
  "deepseek-flash",
  "glm-5.3",
  "glm5.3",
  "kimi-k3",
  "qwen3.8-max",
  "grok-4.6",
  "hy4",
  "muse-spark",
  "laguna",
  "deepseek/deepseek-v4-pro",
  // Casing is documented as insensitive for aliases; pin it, including the
  // mixed-case form of a multi-word alias.
  "DEEPSEEK-V4-PRO",
  "GLM-5.3",
  "GLM5.3",
  "Kimi-K3",
  // A full id keeps its casing (passthrough), unlike a bare alias.
  "DeepSeek/DeepSeek-V4-Pro",
  // Bare-last-segment matching against the catalog is also case-insensitive.
  "Nemotron-3-Ultra-550B-A55B",
  "deepseek-v4-pro ",
  "totally-unknown-model",
  "",
];

// ── capture ─────────────────────────────────────────────────────────────────

function attempt(fn) {
  try {
    return { ok: true, value: fn() };
  } catch (err) {
    return { ok: false, error: String(err?.message ?? err) };
  }
}

const samples = {
  $comment:
    "Translation-layer contract for the cc-proxy rewrite. Generated by " +
    "conformance/record-translate.mjs against dist/; do not edit by hand. " +
    "Volatile fields (threadId, traceparent) are normalised to placeholders.",
  openaiRequests: {},
  anthropicRequests: {},
  modelResolution: {},
};

for (const [name, req] of Object.entries(OPENAI_CASES)) {
  samples.openaiRequests[name] = attempt(() =>
    normalisePhone(openAIToCC(req), req.model),
  );
}
for (const [name, req] of Object.entries(ANTHROPIC_CASES)) {
  samples.anthropicRequests[name] = attempt(() =>
    normalisePhone(anthropicToCC(req), req.model),
  );
}
for (const requested of ALIAS_CASES) {
  samples.modelResolution[requested || "<empty>"] = attempt(() => resolveModel(requested));
}

/**
 * Strip values that legitimately change per call, keeping their shape.
 *
 * `config` carries host-specific context (cwd, platform, node build) that the
 * rewrite will naturally differ on — the contract is that the field exists and
 * is filled from the environment, not its literal contents.
 */
function normalisePhone(cc) {
  const out = structuredClone(cc);
  if (out.threadId) out.threadId = "<uuid>";
  if (out.config) {
    out.config.workingDir = "<cwd>";
    out.config.date = "<date>";
    out.config.environment = "<platform>";
    out.config.isGitRepo = "<bool>";
    if (Array.isArray(out.config.structure)) out.config.structure = "<tree>";
  }
  return out;
}

if (CHECK) {
  if (!existsSync(OUT)) {
    console.error(`no golden file at ${OUT}; run without --check first`);
    process.exit(1);
  }
  const golden = JSON.parse(readFileSync(OUT, "utf8"));
  const diffs = [];
  for (const section of ["openaiRequests", "anthropicRequests", "modelResolution"]) {
    const a = Object.keys(golden[section] ?? {});
    const b = Object.keys(samples[section] ?? {});
    for (const k of a) {
      if (!b.includes(k)) diffs.push(`${section}.${k}: missing`);
      else if (JSON.stringify(golden[section][k]) !== JSON.stringify(samples[section][k]))
        diffs.push(`${section}.${k}: differs`);
    }
    for (const k of b) if (!a.includes(k)) diffs.push(`${section}.${k}: new sample`);
  }
  if (diffs.length === 0) {
    const n =
      Object.keys(samples.openaiRequests).length +
      Object.keys(samples.anthropicRequests).length +
      Object.keys(samples.modelResolution).length;
    console.log(`OK — ${n} translation samples match`);
  } else {
    console.error(`${diffs.length} translation difference(s):`);
    for (const d of diffs) console.error(`  - ${d}`);
    process.exitCode = 1;
  }
} else {
  mkdirSync(path.dirname(OUT), { recursive: true });
  writeFileSync(OUT, JSON.stringify(samples, null, 2) + "\n");
  const n =
    Object.keys(samples.openaiRequests).length +
    Object.keys(samples.anthropicRequests).length +
    Object.keys(samples.modelResolution).length;
  console.log(`recorded ${n} translation samples -> ${path.relative(ROOT, OUT)}`);
}
