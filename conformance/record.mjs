// Behaviour recorder: drives a running proxy through every scenario and writes
// a normalised transcript to conformance/golden/.
//
// The transcript is deliberately language-neutral — no Node types, no internal
// field names — so the Rust rewrite can replay the same scenarios and diff its
// own transcript against this one. Anything that differs is a behaviour change,
// whether intentional or not.
//
// Usage:
//   node conformance/record.mjs [--proxy http://127.0.0.1:8787] [--out golden]
//   node conformance/record.mjs --check          (compare instead of write)

import { spawn } from "node:child_process";
import { mkdirSync, readFileSync, writeFileSync, existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.dirname(HERE);
const SCENARIOS = JSON.parse(
  readFileSync(path.join(HERE, "scenarios", "upstream-scenarios.json"), "utf8"),
);

const args = process.argv.slice(2);
const flag = (name, fallback) => {
  const i = args.indexOf(`--${name}`);
  return i >= 0 ? args[i + 1] : fallback;
};
const CHECK_MODE = args.includes("--check");
const PROXY_PORT = Number(flag("port", 18899));
const MOCK_PORT = Number(flag("mock-port", 19888));
const OUT_DIR = path.join(HERE, flag("out", "golden"));

// A fixed key keeps the recorded transcript free of real credentials.
const FIXTURE_KEY = "conformance-fixture-key";

// Pinned so the recorded `x-command-code-version` header is reproducible.
// It is an arbitrary valid-looking version; the contract is that the header is
// present and carries the configured value, not which value that is.
const PINNED_CC_VERSION = "0.0.0-conformance";

// ── harness plumbing ────────────────────────────────────────────────────────

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function waitFor(url, timeoutMs = 15000) {
  const end = Date.now() + timeoutMs;
  while (Date.now() < end) {
    try {
      const res = await fetch(url);
      if (res.ok) return true;
    } catch {
      /* not up yet */
    }
    await sleep(150);
  }
  throw new Error(`timed out waiting for ${url}`);
}

async function control(pathname, body) {
  const res = await fetch(`http://127.0.0.1:${MOCK_PORT}${pathname}`, {
    method: body ? "POST" : "GET",
    headers: body ? { "Content-Type": "application/json" } : {},
    body: body ? JSON.stringify(body) : undefined,
  });
  return res.json();
}

/** Start the proxy under test with the mock as its upstream. */
function startProxy() {
  const env = {
    ...process.env,
    PORT: String(PROXY_PORT),
    HOST: "127.0.0.1",
    CC_API_BASE: `http://127.0.0.1:${MOCK_PORT}`,
    // Pin the advertised CLI version. At startup the proxy may refresh this
    // from the npm registry, and may serve a cached value from an earlier run —
    // so without this the recorded `x-command-code-version` depends on whether
    // the machine happened to be online, making the golden non-reproducible.
    CC_CLI_VERSION: PINNED_CC_VERSION,
    LOG_LEVEL: "error",
    CC_IDLE_TIMEOUT_MS: "800",
    CC_UPSTREAM_TIMEOUT_MS: "1200",
    CORS_ORIGIN: "*",
  };
  // --exe <path> spawns that binary directly (e.g. the Rust build) instead of
  // node + dist/proxy.js. The scenarios, mock and golden stay shared.
  const exe = flag("exe", null);
  const proc = exe
    ? spawn(exe, { env, stdio: ["ignore", "pipe", "pipe"] })
    : spawn(process.execPath, [path.join(ROOT, "dist", "proxy.js")], {
        env,
        stdio: ["ignore", "pipe", "pipe"],
      });
  proc.stdout.on("data", () => {});
  proc.stderr.on("data", (d) => process.stderr.write(`[proxy] ${d}`));
  return proc;
}

// ── normalisation ───────────────────────────────────────────────────────────

/**
 * Normalise a header map: lowercase keys, drop values that legitimately vary
 * per run (trace ids, dates, versions) so diffs stay meaningful.
 */
function normaliseHeaders(h) {
  const out = {};
  for (const [k, v] of Object.entries(h)) {
    const key = k.toLowerCase();
    if (key === "date" || key === "connection" || key === "keep-alive") continue;
    // The advertised CLI version is configuration, not behaviour: it is
    // refreshed from npm at startup, so its value depends on the machine and
    // the day. The contract is that the header is present and version-shaped.
    if (key === "x-command-code-version" && /^\S+$/.test(String(v))) {
      out[key] = "<version>";
      continue;
    }
    // content-length is derived from the body, and the body embeds the working
    // directory — so the length leaks how long that path is (moving the repo
    // changes every case by the difference in path length). The body is
    // recorded in full and compared separately, so the length adds nothing.
    if (key === "content-length") {
      out[key] = "<len>";
      continue;
    }
    // The project slug is derived from the working-directory basename, so it
    // changes whenever the checkout is renamed and would fail every case for a
    // reason unrelated to behaviour. The contract is the header's presence and
    // slug shape, not which slug.
    if (key === "x-project-slug" && /^[a-z0-9-]+$/.test(String(v))) {
      out[key] = "<slug>";
      continue;
    }
    out[key] = redact(String(v));
  }
  return out;
}

/**
 * Replace anything run-specific with a stable placeholder.
 *
 * Order matters: the UUID rule must run before the narrower hex rules, and it
 * must not rely on `\b` — generated ids look like `msg_<uuid>` and
 * `chatcmpl-<uuid>`, where the separator is a word character, so a word
 * boundary never appears before the first hex digit.
 */
function redact(s) {
  return s
    .replace(
      /[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/gi,
      "<uuid>",
    )
    .replace(/(?<![0-9a-f])[0-9a-f]{32}(?![0-9a-f])/gi, "<trace-id>")
    .replace(/(?<![0-9a-f])[0-9a-f]{16}(?![0-9a-f])/gi, "<span-id>")
    // The User-Agent carries the CLI version, which is configured per run (and
    // refreshed from npm at startup). The contract is the header's shape, not
    // which version this machine happened to advertise — so normalise both the
    // version and the node version, including pre-release/build suffixes.
    .replace(
      /commandcode-cli\/[^\s]+ Node\.js\/v[^\s]+/g,
      "commandcode-cli/<ver> Node.js/<ver>",
    )
    .replace(/\d{4}-\d{2}-\d{2}T[\d:.]+Z/g, "<timestamp>");
}

/** Parse an SSE byte stream into ordered records, preserving event names. */
function parseSSE(text) {
  const records = [];
  for (const block of text.split("\n\n")) {
    if (!block.trim()) continue;
    let event = null;
    const dataLines = [];
    for (const line of block.split("\n")) {
      if (line.startsWith("event:")) event = line.slice(6).trim();
      else if (line.startsWith("data:")) dataLines.push(line.slice(5).trim());
    }
    const data = dataLines.join("\n");
    if (data === "") {
      if (event) records.push({ event, data: null });
      continue;
    }
    if (data === "[DONE]") {
      records.push({ event: event ?? "data", data: "<DONE>" });
      continue;
    }
    let parsed;
    try {
      parsed = JSON.parse(data);
    } catch {
      records.push({ event: event ?? "data", data: `<unparseable>${redact(data)}` });
      continue;
    }
    records.push({ event: event ?? "data", data: normaliseJSON(parsed) });
  }
  return records;
}

/**
 * Recursively normalise a JSON value: sort object keys so serialisation order
 * is not part of the contract, and redact run-varying scalars.
 */
function normaliseJSON(value, key = "") {
  if (typeof value === "string") {
    // The upstream request carries today's date in `config.date`; it changes
    // daily and is not part of the contract, so pin it — otherwise every case
    // fails the morning after the golden was recorded.
    if (key === "date" && /^\d{4}-\d{2}-\d{2}$/.test(value)) return "<date>";
    // `/health` reports the proxy's own version. A release bump is not a
    // behaviour change, so pin it too — otherwise every version bump fails the
    // whole surface group for a reason that has nothing to do with the contract.
    if (key === "version" && /^\d+\.\d+\.\d+(?:[-+][\w.-]+)?$/.test(value)) return "<version>";
    // `config.workingDir` is the directory the proxy was started in. It comes
    // from process.cwd(), so it changes when the repo moves and would make
    // every case fail against a golden recorded elsewhere. The contract is that
    // the field is filled from the environment, not what the path is.
    if (key === "workingDir" && /^([A-Za-z]:[\\/]|\/)/.test(value)) return "<cwd>";
    return redact(value);
  }
  // Unix timestamps and other wall-clock scalars vary per run by design; the
  // contract is their presence and type, not their value.
  if (typeof value === "number") {
    if (key === "created" || key === "created_at" || key === "createdAt") return "<epoch>";
    return value;
  }
  if (Array.isArray(value)) return value.map((v) => normaliseJSON(v, key));
  if (value && typeof value === "object") {
    const out = {};
    for (const k of Object.keys(value).sort()) out[k] = normaliseJSON(value[k], k);
    return out;
  }
  return value;
}

/** Read a response body, normalised. Detects SSE vs plain JSON automatically. */
async function readResponse(res) {
  const text = await res.text();
  const ctype = res.headers.get("content-type") ?? "";
  const base = {
    status: res.status,
    headers: normaliseHeaders(Object.fromEntries(res.headers)),
  };
  if (ctype.includes("text/event-stream")) {
    return { ...base, bodyKind: "sse", records: parseSSE(text) };
  }
  if (text.trim() === "") return { ...base, bodyKind: "empty" };
  try {
    return { ...base, bodyKind: "json", body: normaliseJSON(JSON.parse(text)) };
  } catch {
    return { ...base, bodyKind: "text", body: redact(text) };
  }
}

// ── scenario execution ──────────────────────────────────────────────────────

function openAIRequest(model = "deepseek-v4-flash", extra = {}) {
  return {
    model,
    messages: [{ role: "user", content: "hello" }],
    max_tokens: 64,
    stream: true,
    ...extra,
  };
}

function anthropicRequest(model = "deepseek-v4-flash", extra = {}) {
  return {
    model,
    messages: [{ role: "user", content: "hello" }],
    max_tokens: 64,
    stream: true,
    ...extra,
  };
}

async function post(pathname, body, headers = {}) {
  const res = await fetch(`http://127.0.0.1:${PROXY_PORT}${pathname}`, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Authorization: `Bearer ${FIXTURE_KEY}`,
      ...headers,
    },
    body: JSON.stringify(body),
  });
  return readResponse(res);
}

/** One end-to-end case: upstream script in, downstream transcript out. */
async function runCase(name, { pathname, request, headers, scenario, models }) {
  await control("/__scenario", { scenario, models });
  let downstream;
  try {
    downstream = await post(pathname, request, headers);
  } catch (err) {
    downstream = { status: null, error: redact(String(err)), bodyKind: "transport-error" };
  }
  const upstream = await control("/__log");
  return {
    name,
    pathname,
    // The request the proxy sent upstream: headers matter as much as the body,
    // because CC rejects requests that don't look like the official CLI.
    upstreamRequests: upstream.requests.map((r) => ({
      method: r.method,
      path: r.path,
      headers: normaliseHeaders(r.headers),
      body: r.body ? normaliseJSON(r.body) : null,
    })),
    downstream,
  };
}

async function main() {
  // 1. mock upstream
  const mock = spawn(process.execPath, [path.join(HERE, "mock-upstream.mjs"), String(MOCK_PORT)], {
    stdio: ["ignore", "pipe", "pipe"],
  });
  mock.stdout.on("data", (d) => process.stdout.write(`[mock] ${d}`));
  mock.stderr.on("data", (d) => process.stderr.write(`[mock] ${d}`));

  // 2. proxy under test
  const proxy = startProxy();

  const cleanup = () => {
    try { proxy.kill(); } catch {}
    try { mock.kill(); } catch {}
  };
  process.on("exit", cleanup);

  try {
    await waitFor(`http://127.0.0.1:${MOCK_PORT}/__ready`);
    await waitFor(`http://127.0.0.1:${PROXY_PORT}/health`);

    const transcript = {
      $comment:
        "Behaviour contract for the cc-proxy rewrite. Generated by conformance/record.mjs; " +
        "do not edit by hand. The Rust implementation must reproduce every case verbatim.",
      fixtureKeyLength: FIXTURE_KEY.length,
      cases: [],
    };

    // ── streaming translation: both downstream dialects × every scenario ──
    for (const [key, scenario] of Object.entries(SCENARIOS.scenarios)) {
      transcript.cases.push(
        await runCase(`stream/openai/${key}`, {
          pathname: "/v1/chat/completions",
          request: openAIRequest(),
          scenario,
        }),
      );
      transcript.cases.push(
        await runCase(`stream/anthropic/${key}`, {
          pathname: "/v1/messages",
          request: anthropicRequest(),
          headers: { "anthropic-version": "2023-06-01" },
          scenario,
        }),
      );
    }

    // ── non-streaming: the same upstream, collapsed into one response ──
    for (const key of ["clean-text", "reasoning-then-text", "tool-call-final-only", "error-event"]) {
      transcript.cases.push(
        await runCase(`nonstream/openai/${key}`, {
          pathname: "/v1/chat/completions",
          request: openAIRequest("deepseek-v4-flash", { stream: false }),
          scenario: SCENARIOS.scenarios[key],
        }),
      );
      transcript.cases.push(
        await runCase(`nonstream/anthropic/${key}`, {
          pathname: "/v1/messages",
          request: anthropicRequest("deepseek-v4-flash", { stream: false }),
          headers: { "anthropic-version": "2023-06-01" },
          scenario: SCENARIOS.scenarios[key],
        }),
      );
    }

    // ── upstream HTTP failures: status mapping and envelopes ──
    for (const [key, fail] of Object.entries(SCENARIOS.httpFailures)) {
      if (key.startsWith("$")) continue;
      transcript.cases.push(
        await runCase(`failure/openai/${key}`, {
          pathname: "/v1/chat/completions",
          request: openAIRequest(),
          scenario: { name: key, status: fail.status, errorBody: fail.errorBody },
        }),
      );
      transcript.cases.push(
        await runCase(`failure/anthropic/${key}`, {
          pathname: "/v1/messages",
          request: anthropicRequest(),
          headers: { "anthropic-version": "2023-06-01" },
          scenario: { name: key, status: fail.status, errorBody: fail.errorBody },
        }),
      );
    }

    // ── pure HTTP surface: no upstream involved ──
    const surface = [];
    const call = async (name, pathname, init) => {
      const res = await fetch(`http://127.0.0.1:${PROXY_PORT}${pathname}`, init);
      surface.push({ name, pathname, ...(await readResponse(res)) });
    };

    await call("health", "/health", { method: "GET" });
    await call("models-openai", "/v1/models", { method: "GET" });
    await call("models-anthropic", "/v1/models", {
      method: "GET",
      headers: { "anthropic-version": "2023-06-01" },
    });
    await call("unknown-route", "/nope", { method: "GET" });
    await call("preflight-known", "/v1/chat/completions", {
      method: "OPTIONS",
      headers: { Origin: "http://example.test", "Access-Control-Request-Method": "POST" },
    });
    await call("preflight-unknown", "/nope", {
      method: "OPTIONS",
      headers: { Origin: "http://example.test", "Access-Control-Request-Method": "POST" },
    });
    await call("missing-key-openai", "/v1/chat/completions", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(openAIRequest()),
    });
    await call("missing-key-anthropic", "/v1/messages", {
      method: "POST",
      headers: { "Content-Type": "application/json", "anthropic-version": "2023-06-01" },
      body: JSON.stringify(anthropicRequest()),
    });
    transcript.httpSurface = surface;

    // ── request validation: locally rejected, upstream never called ──
    const invalid = [
      ["not-json", "{not json"],
      ["missing-model", JSON.stringify({ messages: [{ role: "user", content: "x" }] })],
      ["missing-messages", JSON.stringify({ model: "m", max_tokens: 8 })],
      ["bad-role", JSON.stringify({ model: "m", max_tokens: 8, messages: [{ role: "wizard", content: "x" }] })],
      ["bad-temperature", JSON.stringify({ ...openAIRequest(), temperature: "hot" })],
      ["temperature-out-of-range", JSON.stringify({ ...openAIRequest(), temperature: 9 })],
      ["bad-tool-choice", JSON.stringify({ ...openAIRequest(), tool_choice: "whatever" })],
    ];
    transcript.validation = [];
    for (const [name, raw] of invalid) {
      const res = await fetch(`http://127.0.0.1:${PROXY_PORT}/v1/chat/completions`, {
        method: "POST",
        headers: { "Content-Type": "application/json", Authorization: `Bearer ${FIXTURE_KEY}` },
        body: raw,
      });
      transcript.validation.push({ name, ...(await readResponse(res)) });
    }

    // ── model resolution: alias → upstream model id ──
    transcript.modelResolution = [];
    for (const requested of [
      "deepseek-v4-pro",
      "deepseek-v4",
      "deepseek-flash",
      "glm-5.3",
      "unknown-model-xyz",
      "deepseek/deepseek-v4-pro",
    ]) {
      await control("/__scenario", { scenario: SCENARIOS.scenarios["clean-text"] });
      await post("/v1/chat/completions", openAIRequest(requested));
      const log = await control("/__log");
      const body = log.requests[0]?.body;
      transcript.modelResolution.push({
        requested,
        sentUpstream: body?.model ?? null,
        threadIdStable: typeof body?.threadId === "string",
      });
    }

    // ── write or compare ──
    if (CHECK_MODE) {
      const file = path.join(OUT_DIR, "behaviour.json");
      if (!existsSync(file)) {
        console.error(`no golden file at ${file}; run without --check first`);
        process.exitCode = 1;
      } else {
        const golden = JSON.parse(readFileSync(file, "utf8"));
        const diffs = diffTranscripts(golden, transcript);
        if (diffs.length === 0) {
          console.log(`OK — ${transcript.cases.length} cases match the golden transcript`);
        } else {
          console.error(`${diffs.length} behaviour difference(s):`);
          for (const d of diffs) console.error(`  - ${d}`);
          process.exitCode = 1;
        }
      }
    } else {
      mkdirSync(OUT_DIR, { recursive: true });
      const file = path.join(OUT_DIR, "behaviour.json");
      writeFileSync(file, JSON.stringify(transcript, null, 2) + "\n");
      const cases = transcript.cases.length + transcript.httpSurface.length + transcript.validation.length;
      console.log(`recorded ${cases} behaviour samples -> ${path.relative(ROOT, file)}`);
    }
  } finally {
    cleanup();
  }
}

/** Deep-compare two transcripts, reporting the first path that differs per case. */
function diffTranscripts(a, b) {
  const diffs = [];
  const casesA = new Map(a.cases.map((c) => [c.name, c]));
  const casesB = new Map(b.cases.map((c) => [c.name, c]));
  for (const name of casesA.keys()) {
    if (!casesB.has(name)) {
      diffs.push(`case missing in current run: ${name}`);
      continue;
    }
    const da = JSON.stringify(casesA.get(name));
    const db = JSON.stringify(casesB.get(name));
    if (da !== db) diffs.push(`case differs: ${name}`);
  }
  for (const name of casesB.keys()) {
    if (!casesA.has(name)) diffs.push(`new case not in golden: ${name}`);
  }
  return diffs;
}

await main();
