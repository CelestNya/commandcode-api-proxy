// M5 acceptance: drive the proxy against the *real* CC upstream and check that a
// whole session works — a conversation turn, and a tool call round-trip.
//
// This is the one check the golden transcript cannot make. Every other test
// runs against `mock-upstream.mjs`, which by construction agrees with whatever
// the proxy believes; only the real upstream can reject a request for looking
// unlike the official CLI.
//
// The proxy under test is started on an isolated port, so a production instance
// on 8787 is never touched. The key is read from ZCode's provider config and is
// never written to disk or to the log output.
//
// Usage:
//   node conformance/acceptance.mjs --exe target/release/ccproxy.exe
//   node conformance/acceptance.mjs --exe target/release/ccproxy.exe --key <key>
//
// Exit code 0 means every check passed.

import { readFileSync } from "node:fs";
import { spawn } from "node:child_process";
import { setTimeout as sleep } from "node:timers/promises";

const args = process.argv.slice(2);
const flag = (name, fallback = null) => {
  const i = args.indexOf(`--${name}`);
  return i >= 0 ? args[i + 1] : fallback;
};

const EXE = flag("exe");
const PORT = Number(flag("port", "18920"));
const MODEL = flag("model", "deepseek/deepseek-v4-flash");
const ZCODE_CONFIG =
  process.env.ZCODE_CONFIG ?? "C:/Users/CelestNya/.zcode/v2/config.json";
const TIMEOUT_MS = Number(flag("timeout", "120000"));

if (!EXE) {
  console.error("--exe <path to ccproxy binary> is required");
  process.exit(2);
}

/** The real CC key. Read from ZCode's config so no credential lands in the repo. */
function loadKey() {
  const explicit = flag("key");
  if (explicit) return explicit;
  let cfg;
  try {
    cfg = JSON.parse(readFileSync(ZCODE_CONFIG, "utf8"));
  } catch (err) {
    throw new Error(`cannot read ${ZCODE_CONFIG} (${err.message}); pass --key`);
  }
  // The provider pointing at the local proxy is the one carrying a CC key.
  for (const provider of Object.values(cfg.provider ?? {})) {
    const base = provider?.options?.baseURL ?? "";
    const key = provider?.options?.apiKey;
    if (base.includes("127.0.0.1:8787") && key) return key;
  }
  throw new Error(`no CC key found in ${ZCODE_CONFIG}; pass --key`);
}

const results = [];
function check(name, ok, detail = "") {
  results.push({ name, ok });
  console.log(`  ${ok ? "PASS" : "FAIL"}  ${name}${detail ? ` — ${detail}` : ""}`);
}

async function waitFor(url, timeoutMs = 20000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const res = await fetch(url);
      if (res.ok) return await res.json();
    } catch {
      /* not up yet */
    }
    await sleep(150);
  }
  throw new Error(`timeout waiting for ${url}`);
}

/** Collect an SSE response into its parsed records. */
async function readSse(res) {
  const text = await res.text();
  const records = [];
  for (const block of text.split("\n\n")) {
    if (!block.trim()) continue;
    let event = "data";
    const dataLines = [];
    for (const line of block.split("\n")) {
      if (line.startsWith("event: ")) event = line.slice(7);
      else if (line.startsWith("data: ")) dataLines.push(line.slice(6));
    }
    const data = dataLines.join("\n");
    let parsed = data;
    try {
      parsed = JSON.parse(data);
    } catch {
      /* [DONE] and friends stay strings */
    }
    records.push({ event, data: parsed });
  }
  return records;
}

/** A unique marker so a reply can only have come from this prompt. */
const nonce = () => `ZQ${Math.random().toString(36).slice(2, 10).toUpperCase()}`;

async function main() {
  const key = loadKey();
  console.log(`proxy under test: ${EXE}`);
  console.log(`isolated port:    ${PORT}`);
  console.log(`model:            ${MODEL}`);

  const proxy = spawn(EXE, [], {
    env: {
      ...process.env,
      HOST: "127.0.0.1",
      PORT: String(PORT),
      // Real upstream, real headers. CC_CLI_VERSION is left unset so the
      // startup refresh runs against npm, which is the path production uses.
      LOG_LEVEL: "debug",
      CC_IDLE_TIMEOUT_MS: String(TIMEOUT_MS),
      CC_UPSTREAM_TIMEOUT_MS: String(TIMEOUT_MS),
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  let log = "";
  proxy.stdout.on("data", (d) => {
    log += d.toString();
  });
  proxy.stderr.on("data", (d) => {
    log += d.toString();
  });

  const cleanup = () => {
    try {
      proxy.kill();
    } catch {
      /* already gone */
    }
  };
  process.on("exit", cleanup);

  const base = `http://127.0.0.1:${PORT}`;
  const health = await waitFor(`${base}/health`);

  // ── 0. startup ──
  check("proxy is up on the isolated port", health.status === "ok", `version ${health.version}`);
  const refreshed = /refreshed from npm: (\S+)/.exec(log);
  check(
    "CLI version refreshed from npm (not the stale fallback)",
    refreshed !== null && refreshed[1] !== "0.40.3",
    refreshed ? refreshed[1] : "no refresh logged — using fallback",
  );

  // ── 1. a plain conversation turn (Anthropic dialect) ──
  const marker1 = nonce();
  const t1 = Date.now();
  const res1 = await fetch(`${base}/v1/messages`, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      "x-api-key": key,
      "anthropic-version": "2023-06-01",
    },
    body: JSON.stringify({
      model: MODEL,
      max_tokens: 64,
      stream: true,
      messages: [{ role: "user", content: `Reply with exactly this token: ${marker1}` }],
    }),
  });
  check("conversation request accepted upstream", res1.status === 200, `HTTP ${res1.status}`);
  if (res1.status === 200) {
    const records = await readSse(res1);
    const types = records.map((r) => r.data?.type ?? r.event);
    const text = records
      .map((r) => r.data?.delta?.text ?? "")
      .join("");
    check("stream produced text", text.length > 0, `${text.length} chars in ${Date.now() - t1}ms`);
    check(
      "stream opened with message_start",
      types[0] === "message_start",
      types[0] ?? "(no records)",
    );
    check("stream terminated with message_stop", types.at(-1) === "message_stop", types.at(-1));
    check(
      "no error record was laundered into a success",
      !types.includes("error"),
      types.includes("error") ? "an error record appeared" : "",
    );
  } else {
    const body = await res1.text();
    check("upstream rejection is explained", false, body.slice(0, 300));
  }

  // ── 2. a tool call round-trip (OpenAI dialect) ──
  const marker2 = nonce();
  const res2 = await fetch(`${base}/v1/chat/completions`, {
    method: "POST",
    headers: { "Content-Type": "application/json", Authorization: `Bearer ${key}` },
    body: JSON.stringify({
      model: MODEL,
      stream: true,
      messages: [{ role: "user", content: `Call the tool with value ${marker2}.` }],
      tools: [
        {
          type: "function",
          function: {
            name: "record_value",
            description: "Record a value the user supplied.",
            parameters: {
              type: "object",
              properties: { value: { type: "string", description: "the value" } },
              required: ["value"],
            },
          },
        },
      ],
    }),
  });
  check("tool request accepted upstream", res2.status === 200, `HTTP ${res2.status}`);
  if (res2.status === 200) {
    const records = await readSse(res2);
    const toolCalls = records
      .map((r) => r.data?.choices?.[0]?.delta?.tool_calls)
      .filter(Boolean)
      .flat();
    check(
      "model produced a tool call",
      toolCalls.length > 0,
      toolCalls.length ? `${toolCalls[0]?.function?.name ?? "?"}` : "no tool_calls delta",
    );
    const finish = records
      .map((r) => r.data?.choices?.[0]?.finish_reason)
      .filter(Boolean)
      .at(-1);
    check("finish_reason is tool_calls", finish === "tool_calls", finish ?? "(none)");
  } else {
    const body = await res2.text();
    check("upstream rejection is explained", false, body.slice(0, 300));
  }

  // ── 3. usage was recorded for both turns ──
  const after = await (await fetch(`${base}/health`)).json();
  check(
    "usage recorded both turns",
    (after.cache?.requests ?? 0) >= 2,
    `requests=${after.cache?.requests ?? 0}`,
  );

  // ── 4. the key never reached the log ──
  check(
    "API key absent from proxy log",
    !log.includes(key),
    log.includes(key) ? "key found in log output" : "",
  );
  const leaked = /user_[A-Za-z0-9]{4}/.exec(log);
  check("no key-shaped token in log", leaked === null, leaked ? leaked[0] : "");

  cleanup();

  const failed = results.filter((r) => !r.ok);
  console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
  if (failed.length) {
    console.log("failed:");
    for (const f of failed) console.log(`  - ${f.name}`);
    console.log("\n── proxy log tail ──");
    console.log(log.split("\n").slice(-25).join("\n"));
    process.exit(1);
  }
  console.log("M5 session-level acceptance: PASS");
  process.exit(0);
}

main().catch((err) => {
  console.error(`acceptance failed: ${err.message}`);
  process.exit(1);
});
