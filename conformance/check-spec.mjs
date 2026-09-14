// Spec-vs-reality check.
//
// RUST-REWRITE-SPEC.md states what the proxy does today. Every such statement
// is a claim that can go stale — and a wrong claim is worse than a missing one,
// because it steers the rewrite into implementing the wrong spec correctly.
// Two claims were already found to be wrong this way (missing `model` is
// forwarded rather than rejected locally; effort clipping is two composed steps
// and collapses four budget bands into one value).
//
// This script re-derives each verifiable claim from the recorded samples and
// fails when they disagree. Run it after editing either the spec or the golden
// files.
//
// Usage: node conformance/check-spec.mjs

import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const golden = JSON.parse(readFileSync(path.join(HERE, "golden", "behaviour.json"), "utf8"));
const translate = JSON.parse(readFileSync(path.join(HERE, "golden", "translate.json"), "utf8"));
const spec = readFileSync(path.join(HERE, "..", "RUST-REWRITE-SPEC.md"), "utf8");

let failures = 0;
let checks = 0;

/** Assert a value observed in the samples matches what the spec claims. */
function expect(label, actual, expected) {
  checks += 1;
  const a = JSON.stringify(actual);
  const e = JSON.stringify(expected);
  if (a !== e) {
    failures += 1;
    console.error(`  FAIL ${label}\n       spec says: ${e}\n       samples:   ${a}`);
  }
}

/** Assert a phrase is present in the spec (guards against silent deletion). */
function expectSpecMentions(label, phrase) {
  checks += 1;
  if (!spec.includes(phrase)) {
    failures += 1;
    console.error(`  FAIL ${label}: spec no longer contains ${JSON.stringify(phrase)}`);
  }
}

const surface = Object.fromEntries(golden.httpSurface.map((s) => [s.name, s]));
const caseOf = (name) => golden.cases.find((c) => c.name === name);
const caseStatus = (name) => caseOf(name)?.downstream.status;
const caseErrType = (name) => caseOf(name)?.downstream.body?.error?.type;
const streamRecords = (name) => caseOf(name)?.downstream.records ?? [];

// ── 2.1 route table ─────────────────────────────────────────────────────────
console.log("2.1 route table");
expect("health -> 200", surface["health"]?.status, 200);
expect("unknown route -> 404", surface["unknown-route"]?.status, 404);
expect("preflight on known path -> 204", surface["preflight-known"]?.status, 204);
expect("preflight on unknown path -> 404", surface["preflight-unknown"]?.status, 404);

// ── 2.2 passthrough auth ────────────────────────────────────────────────────
console.log("2.2 passthrough auth");
expect("no key on openai -> 401", surface["missing-key-openai"]?.status, 401);
expect("no key on anthropic -> 401", surface["missing-key-anthropic"]?.status, 401);

// ── 2.3 error envelopes ─────────────────────────────────────────────────────
console.log("2.3 error envelopes");
// Upstream 4xx passes through; 5xx is collapsed to 502. Both dialects.
for (const [name, openaiStatus, anthropicStatus] of [
  ["unauthorized-401", 401, 401],
  ["forbidden-403-unknown-model", 403, 403],
  ["rate-limited-429", 429, 429],
  ["bad-request-400", 400, 400],
  ["server-error-500", 502, 502],
  ["overloaded-529", 502, 502],
]) {
  expect(`openai ${name}`, caseStatus(`failure/openai/${name}`), openaiStatus);
  expect(`anthropic ${name}`, caseStatus(`failure/anthropic/${name}`), anthropicStatus);
}
expect("openai envelope type", caseErrType("failure/openai/rate-limited-429"), "proxy_error");
expect("anthropic 401 type", caseErrType("failure/anthropic/unauthorized-401"), "authentication_error");
expect("anthropic 403 type", caseErrType("failure/anthropic/forbidden-403-unknown-model"), "permission_error");
expect("anthropic 429 type", caseErrType("failure/anthropic/rate-limited-429"), "rate_limit_error");
expect("anthropic 400 type", caseErrType("failure/anthropic/bad-request-400"), "invalid_request_error");
expect("anthropic 5xx type", caseErrType("failure/anthropic/server-error-500"), "api_error");

// ── 2.4 in-band errors ──────────────────────────────────────────────────────
console.log("2.4 in-band errors");
const anthropicErr = streamRecords("stream/anthropic/error-event").find((r) => r.event === "error");
expect("anthropic in-band type is overloaded_error", anthropicErr?.data?.error?.type, "overloaded_error");
const openaiErr = streamRecords("stream/openai/error-event").find((r) => r.data?.error);
expect("openai in-band code is network_error", openaiErr?.data?.error?.code, "network_error");

// ── 2.4.1 error timing ──────────────────────────────────────────────────────
console.log("2.4.1 error timing");
expectSpecMentions("writeHead is documented as the boundary", "res.writeHead(200)` 是分界线");
// Mid-stream error still terminates the OpenAI stream with a bare [DONE].
const afterContent = streamRecords("stream/openai/error-after-content");
expect("error then [DONE]", afterContent.map((r) => (r.data === "<DONE>" ? "<DONE>" : r.data?.error ? "error" : "chunk")),
  ["chunk", "chunk", "error", "<DONE>"]);

// ── 2.6 streaming hard constraints ──────────────────────────────────────────
console.log("2.6 streaming hard constraints");
// Every Anthropic record carries a type discriminator matching its event name.
const anthropicStreams = golden.cases.filter((c) => c.name.startsWith("stream/anthropic/"));
let mismatches = 0;
let missing = 0;
for (const c of anthropicStreams) {
  for (const r of c.downstream.records ?? []) {
    if (!r.event || typeof r.data !== "object" || r.data === null) continue;
    if (r.data.type === undefined) missing += 1;
    else if (r.data.type !== r.event) mismatches += 1;
  }
}
expect("no Anthropic record missing `type`", missing, 0);
expect("no event/type mismatch", mismatches, 0);
expect("signature_delta is nested, never top-level",
  [...new Set(anthropicStreams.flatMap((c) => (c.downstream.records ?? []).map((r) => r.event)))].includes("signature_delta"),
  false);
expect("[DONE] appears exactly once per openai stream",
  streamRecords("stream/openai/clean-text").filter((r) => r.data === "<DONE>").length, 1);
expect("message_stop carries its discriminator",
  streamRecords("stream/anthropic/clean-text").find((r) => r.event === "message_stop")?.data,
  { type: "message_stop" });

// ── 3.4 no total-duration cap ───────────────────────────────────────────────
console.log("3.4 timeout semantics");
expectSpecMentions("total-duration cap is explicitly forbidden", "禁止引入生成阶段的总时长上限");
expectSpecMentions("idle timeout is byte-interval based", "只认字节间隔");

// ── 2.5 partial output survives; prefill does not ───────────────────────────
// These two claims can't be checked against golden/ — that records what the
// proxy *sends*, not what a client SDK does with it. They are guarded by their
// probe output instead, so the spec can't drift away from the evidence.
console.log("2.5/8.7 client-side findings");
expectSpecMentions("partial text is documented as surviving", "部分输出不会丢失");
expectSpecMentions("the empty-argument tool_use hazard is documented", "空参数工具调用");
expectSpecMentions("prefill is documented as unsupported", "不实现 Anthropic 的");
// The load-bearing correction: in-band errors never retry, whatever their type.
expectSpecMentions("in-band errors are documented as non-retryable", "不能**触发下游重试");
// ...and the consequence: the boundary is message_start, not the 200 itself.
expectSpecMentions("message_start is documented as the real boundary", "`message_start` 是真正不可逆的那一步");
expectSpecMentions("the probe-the-first-chunk recovery path is recorded", "在发 `message_start` 之前先把上游的头探出来");
// Production evidence that the deployed fix did not work.
{
  const prod = JSON.parse(
    readFileSync(path.join(HERE, "client-probes", "observed", "production-inband-error.json"), "utf8"),
  );
  expect("the deployed proxy already sent overloaded_error",
    prod.inBandError?.errorType, "overloaded_error");
  expect("...and the client still refused to retry",
    prod.client?.classification?.retryable, false);
  expect("...with 11 attempts available",
    prod.client?.classification?.maxAttempts, 11);
  expect("...and the client disables SDK-level retry for streams",
    prod.client?.sdkRetryDisabled, true);
}
{
  const probe = JSON.parse(
    readFileSync(path.join(HERE, "client-probes", "observed", "retry-classification.json"), "utf8"),
  );
  const byVariant = Object.fromEntries(probe.results.map((r) => [r.variant, r]));
  // A retry is only real if the upstream was hit more than once.
  expect("http 529 IS retried", byVariant["D-http-529"]?.retried, true);
  expect("http 500 IS retried", byVariant["G-openai-http-500"]?.retried, true);
  expect("http 429 IS retried", byVariant["J-http-429"]?.retried, true);
  expect("http 401 is NOT retried", byVariant["K-http-401"]?.retried, false);
  expect("http 403 is NOT retried", byVariant["L-http-403"]?.retried, false);
  expect("http 400 is NOT retried", byVariant["M-http-400"]?.retried, false);
  expect("error-after-message_start is NOT retried",
    byVariant["A-message_start-then-error"]?.retried, false);
  expect("error-after-content is NOT retried",
    byVariant["C-content-then-error"]?.retried, false);
  expect("error-first without 200 IS retried",
    byVariant["B-error-first"]?.retried, true);
  expect("openai error envelope after content is NOT retried",
    byVariant["E-openai-envelope-after-content"]?.retried, false);
  expect("openai error envelope first is NOT retried",
    byVariant["F-openai-envelope-first"]?.retried, false);
  expect("the partial text still reaches the client",
    byVariant["C-content-then-error"]?.text, "partial text");
  // The error type plays no part in the decision — all three behave identically.
  for (const v of ["A-message_start-then-error", "H-message_start-then-api_error", "I-message_start-then-rate_limit_error"]) {
    expect(`${v} is NOT retried (type is irrelevant)`, byVariant[v]?.retried, false);
  }
  // The worst shape: terminators appended after the error turn it into a success.
  const c2 = byVariant["C2-content-then-error-then-terminators"];
  expect("appending finishRecords still yields finishReason 'stop'", c2?.finish, "stop");
  expect("...and the error part is still delivered", c2?.errorPart, true);
}
{
  const probe = JSON.parse(
    readFileSync(path.join(HERE, "client-probes", "observed", "partial-context.json"), "utf8"),
  );
  const byLabel = Object.fromEntries(probe.results.map((r) => [r.label, r]));
  const keptText = (label) =>
    (byLabel[label]?.kept ?? []).map((b) => b.text ?? "").join("");
  expect("error envelope keeps the partial text (anthropic)",
    keptText("anthropic/error-after-content"), "Here is the beginning of the answer");
  expect("error envelope keeps the partial text (openai)",
    byLabel["openai/error-after-content"]?.kept?.[0]?.content?.includes("Here is the beginning of the answer"), true);
  expect("clean truncation keeps the same text",
    keptText("anthropic/truncated-as-max-tokens"), "Here is the beginning of the answer");
  // The hazard: partial tool JSON is dropped and becomes an empty-argument call.
  const toolBlocks = byLabel["anthropic/tool-use-truncated-clean-stop"]?.kept ?? [];
  expect("truncated tool_use survives as a block", toolBlocks[0]?.type, "tool_use");
  expect("...with its arguments silently emptied", toolBlocks[0]?.input, {});
  expect("empty stream yields no content blocks",
    byLabel["anthropic/empty-no-blocks"]?.kept, []);
}

// ── 4.1 model resolution ────────────────────────────────────────────────────
console.log("4.1 model resolution");
const resolve = (m) => translate.modelResolution[m]?.value;
expect("alias resolves", resolve("deepseek-v4-pro"), "deepseek/deepseek-v4-pro");
expect("alias is case-insensitive", resolve("DEEPSEEK-V4-PRO"), "deepseek/deepseek-v4-pro");
expect("mixed-case alias resolves", resolve("GLM-5.3"), "zai-org/GLM-5.3");
expect("full id passes through unchanged, casing included",
  resolve("DeepSeek/DeepSeek-V4-Pro"), "DeepSeek/DeepSeek-V4-Pro");
expect("bare name matches catalog case-insensitively",
  resolve("Nemotron-3-Ultra-550B-A55B"), "nvidia/nemotron-3-ultra-550b-a55b");
expect("unknown model passes through", resolve("totally-unknown-model"), "totally-unknown-model");
expect("empty string falls back to first catalog entry", resolve("<empty>"), "deepseek/deepseek-v4-pro");
expect("no trim", resolve("deepseek-v4-pro "), "deepseek-v4-pro ");

// ── 4.2 effort: two composed steps ──────────────────────────────────────────
console.log("4.2 reasoning effort (two steps)");
const effort = (n) => translate.anthropicRequests[n]?.value?.params?.reasoning_effort;
const openaiEffort = (n) => translate.openaiRequests[n]?.value?.params?.reasoning_effort;
// deepseek/deepseek-v4-pro accepts only high/max, so every band up to xhigh
// collapses to high — this is the counter-intuitive part the spec calls out.
expect("budget low band -> high", effort("thinking-budget-band-low"), "high");
expect("budget medium band -> high", effort("thinking-budget-band-medium"), "high");
expect("budget high band -> high", effort("thinking-budget-band-high"), "high");
expect("budget xhigh band -> high", effort("thinking-budget-band-xhigh"), "high");
expect("budget max band -> max", effort("thinking-budget-band-max"), "max");
// grok-4.5's set covers the low bands, so it does not collapse.
expect("grok medium band -> medium", effort("thinking-budget-wide-set"), "medium");
expect("grok low band -> low", effort("thinking-clipped-grok"), "low");
expect("effort clips upward", openaiEffort("reasoning-effort-clipped-up"), "high");
expect("effort clips downward", openaiEffort("reasoning-effort-clipped"), "high");
expect("uncatalogued model keeps effort as-is", openaiEffort("reasoning-effort-uncatalogued-model"), "max");

// ── 4.4 tool_choice ─────────────────────────────────────────────────────────
console.log("4.4 tool_choice");
const toolChoice = translate.openaiRequests["tool-choice-object"]?.value?.params?.tool_choice;
expect("tool_choice is an object, not a bare string", typeof toolChoice, "object");
expect("tool_choice uses CC's shape", toolChoice, { type: "tool", name: "get_time" });

// ── 4.5 no-tools safeguard asymmetry ────────────────────────────────────────
console.log("4.5 no-tools safeguard");
const openaiParams = translate.openaiRequests["no-tools-guard"]?.value?.params;
expect("openai injects a system instruction", typeof openaiParams?.system, "string");
const anthropicParams = translate.anthropicRequests["minimal-text"]?.value?.params;
expect("anthropic does not inject it", anthropicParams?.system, undefined);

// ── validation asymmetry ────────────────────────────────────────────────────
console.log("4.6 validation");
const validation = Object.fromEntries(golden.validation.map((v) => [v.name, v]));
expect("missing model is NOT rejected locally (forwarded upstream)", validation["missing-model"]?.status, 400);
expect("...and the message proves it came from upstream",
  /CC API 400/.test(validation["missing-model"]?.body?.error?.message ?? ""), true);
expect("missing messages IS rejected locally",
  validation["missing-messages"]?.body?.error?.message, "Field 'messages' must be an array");
expect("bad temperature rejected", validation["bad-temperature"]?.body?.error?.message,
  "Field 'temperature' must be a number");
expect("out-of-range temperature rejected", validation["temperature-out-of-range"]?.body?.error?.message,
  "Field 'temperature' must be between 0 and 2");
expect("bad tool_choice rejected", validation["bad-tool-choice"]?.body?.error?.message,
  "Field 'tool_choice' string must be one of: auto, none, required");

// ── 5.6 logging contract ────────────────────────────────────────────────────
console.log("5.6 request observability");
expectSpecMentions("arrival/outcome bracketing is documented", "每个请求都由两行");
expectSpecMentions("the finish-not-close pitfall is recorded", "在 `finish` 而非 `close` 上触发");
expectSpecMentions("registering before dispatch is required", "结果行必须在分发前注册");
expectSpecMentions("request id reuse is required", "handler 复用到达时生成的 id");
// A rejection must never be silent again — the whole point of the change.
expectSpecMentions("local rejections must be logged", "4xx 拒绝原因");

// ── upstream request shape ──────────────────────────────────────────────────
console.log("3.2 upstream request");
const upstreamReq = caseOf("stream/openai/clean-text")?.upstreamRequests?.[0];
const headers = upstreamReq?.headers ?? {};
for (const h of ["user-agent", "x-cli-environment", "x-command-code-version", "x-session-id",
                 "x-co-flag", "x-taste-learning", "x-project-slug", "traceparent", "authorization"]) {
  expect(`upstream sends ${h}`, Object.hasOwn(headers, h), true);
}
expect("x-session-id equals body.threadId", headers["x-session-id"], upstreamReq?.body?.threadId);
expect("upstream path", upstreamReq?.path, "/alpha/generate");

// ── report ──────────────────────────────────────────────────────────────────
console.log();
if (failures === 0) {
  console.log(`${checks} spec claims match the recorded samples`);
} else {
  console.error(`${failures} of ${checks} spec claims do NOT match the samples`);
  console.error("Fix the spec (or re-record, if the behaviour intentionally changed).");
  process.exit(1);
}
