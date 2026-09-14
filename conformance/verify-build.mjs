// Pre-hot-swap verification: prove a candidate build actually emits the
// retryable error markers before it replaces a running instance.
//
// Motivation: a released-but-undeployed fix (v0.4.2) changes the in-band error
// envelope so the downstream client classifies it as retryable. That claim was
// only ever checked by reading the diff. This script checks the shipped
// artifact by making it fail for real and inspecting the bytes it emits.
//
// It starts its own mock upstream and its own proxy instance on ephemeral
// ports. It never contacts the production port or an existing instance.
//
// Usage:
//   node conformance/verify-build.mjs                 (verify dist/ on random ports)
//   node conformance/verify-build.mjs --exe <path>    (verify a packaged folder's dist)
//
// Exit code 0 = all checks passed, 1 = at least one failed.

import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync } from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.dirname(HERE);

const args = process.argv.slice(2);
const flag = (name, fallback) => {
  const i = args.indexOf(`--${name}`);
  return i >= 0 ? args[i + 1] : fallback;
};

/** Ask the OS for a free port, so parallel runs never collide. */
function freePort() {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.once("error", reject);
    srv.listen(0, "127.0.0.1", () => {
      const { port } = srv.address();
      srv.close(() => resolve(port));
    });
  });
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function waitFor(url, timeoutMs = 20000) {
  const end = Date.now() + timeoutMs;
  let lastErr = "";
  while (Date.now() < end) {
    try {
      const res = await fetch(url);
      if (res.ok) return true;
    } catch (err) {
      lastErr = String(err);
    }
    await sleep(150);
  }
  throw new Error(`timed out waiting for ${url} (${lastErr})`);
}

/**
 * Resolve which dist/ to verify. A packaged build ships its own dist next to
 * the tray exe, so `--exe` points at that folder.
 */
function resolveDist() {
  const exe = flag("exe", null);
  if (exe) {
    const dir = path.dirname(path.resolve(exe));
    const dist = path.join(dir, "dist");
    if (!existsSync(path.join(dist, "proxy.js"))) {
      throw new Error(`no dist/proxy.js next to ${exe} (looked in ${dist})`);
    }
    return { dist, label: dir };
  }
  const dist = path.join(ROOT, "dist");
  if (!existsSync(path.join(dist, "proxy.js"))) {
    throw new Error(`no dist/proxy.js — run \`pnpm build\` first`);
  }
  return { dist, label: "dist/ (working tree)" };
}

/** Read the version string the build reports, for the record. */
function versionOf(dist) {
  try {
    return JSON.parse(readFileSync(path.join(dist, "..", "package.json"), "utf8")).version;
  } catch {
    return "unknown";
  }
}

const results = [];
function check(name, pass, detail) {
  results.push({ name, pass, detail });
  const mark = pass ? "ok  " : "FAIL";
  console.log(`  ${mark} ${name}${detail ? ` — ${detail}` : ""}`);
}

// ── the checks ──────────────────────────────────────────────────────────────

/**
 * Drive one streaming request whose upstream dies mid-stream, and return the
 * downstream records. `terminated` is the failure seen in production: the
 * upstream socket drops after the response has started.
 */
async function captureMidStreamFailure(proxyPort, mockPort, pathname) {
  const scenario = {
    name: "verify-reset",
    ndjson: [
      { type: "start", data: {} },
      { type: "text-delta", data: { text: "partial" } },
    ],
    resetAfterMs: 80,
  };
  await fetch(`http://127.0.0.1:${mockPort}/__scenario`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ scenario }),
  });

  const res = await fetch(`http://127.0.0.1:${proxyPort}${pathname}`, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Authorization: "Bearer verify-build-fixture",
      ...(pathname === "/v1/messages" ? { "anthropic-version": "2023-06-01" } : {}),
    },
    body: JSON.stringify({
      model: "deepseek-v4-flash",
      max_tokens: 32,
      stream: true,
      messages: [{ role: "user", content: "hi" }],
    }),
  });
  const text = await res.text();
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
    if (data === "[DONE]") { records.push({ event, done: true }); continue; }
    if (!data) continue;
    try { records.push({ event, data: JSON.parse(data) }); } catch { /* skip */ }
  }
  return records;
}

async function main() {
  const { dist, label } = resolveDist();
  const mockPort = await freePort();
  const proxyPort = await freePort();
  const tmp = mkdtempSync(path.join(os.tmpdir(), "ccverify-"));

  console.log(`verifying: ${label}`);
  console.log(`  proxy.js version : ${versionOf(dist)}`);
  console.log(`  proxy port       : ${proxyPort} (ephemeral)`);
  console.log(`  mock port        : ${mockPort} (ephemeral)`);
  console.log(`  production 8787  : NOT TOUCHED`);
  console.log();

  const mock = spawn(process.execPath, [path.join(HERE, "mock-upstream.mjs"), String(mockPort)], {
    stdio: ["ignore", "ignore", "pipe"],
  });
  mock.stderr.on("data", (d) => process.stderr.write(`[mock] ${d}`));

  // A private log dir keeps this run out of any real instance's logs.
  const proxy = spawn(process.execPath, [path.join(dist, "proxy.js")], {
    env: {
      ...process.env,
      HOST: "127.0.0.1",
      PORT: String(proxyPort),
      CC_API_BASE: `http://127.0.0.1:${mockPort}`,
      LOG_LEVEL: "error",
      CC_IDLE_TIMEOUT_MS: "3000",
      CC_UPSTREAM_TIMEOUT_MS: "3000",
      CC_TRAY_NS: "verify-build",
      TEMP: tmp,
    },
    stdio: ["ignore", "ignore", "pipe"],
  });
  proxy.stderr.on("data", (d) => process.stderr.write(`[proxy] ${d}`));

  const cleanup = () => { try { proxy.kill(); } catch {} try { mock.kill(); } catch {} };
  process.on("exit", cleanup);

  try {
    await waitFor(`http://127.0.0.1:${mockPort}/__ready`);
    await waitFor(`http://127.0.0.1:${proxyPort}/health`);

    // ── 1. service is actually up and serving ──
    console.log("1. service reachable");
    const health = await (await fetch(`http://127.0.0.1:${proxyPort}/health`)).json();
    check("GET /health returns ok", health.status === "ok", `status=${health.status}`);
    check("version is reported", typeof health.version === "string", `version=${health.version}`);

    // ── 2. clean turn still works (the fix must not break the happy path) ──
    console.log();
    console.log("2. clean turn (regression guard)");
    await fetch(`http://127.0.0.1:${mockPort}/__scenario`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        scenario: {
          name: "verify-ok",
          ndjson: [
            { type: "start", data: {} },
            { type: "text-delta", data: { text: "hello" } },
            { type: "finish", data: { finishReason: "stop" } },
          ],
        },
      }),
    });
    const okRes = await fetch(`http://127.0.0.1:${proxyPort}/v1/chat/completions`, {
      method: "POST",
      headers: { "Content-Type": "application/json", Authorization: "Bearer verify-build-fixture" },
      body: JSON.stringify({
        model: "deepseek-v4-flash", max_tokens: 32, stream: true,
        messages: [{ role: "user", content: "hi" }],
      }),
    });
    const okText = await okRes.text();
    check("stream returns 200", okRes.status === 200, `status=${okRes.status}`);
    check("stream carries the content", okText.includes("hello"));
    check("stream ends with [DONE]", okText.includes("[DONE]"));

    // ── 3. the actual reason for this script: retryable error markers ──
    console.log();
    console.log("3. mid-stream failure is marked retryable");

    const openaiRecords = await captureMidStreamFailure(proxyPort, mockPort, "/v1/chat/completions");
    const openaiErr = openaiRecords.find((r) => r.data?.error);
    const code = openaiErr?.data?.error?.code;
    check("openai path emits an error envelope", Boolean(openaiErr), JSON.stringify(openaiErr?.data?.error?.message ?? null));
    check("openai error code is network_error (retryable)", code === "network_error",
      `code=${JSON.stringify(code)}`);

    const anthropicRecords = await captureMidStreamFailure(proxyPort, mockPort, "/v1/messages");
    const anthropicErr = anthropicRecords.find((r) => r.event === "error");
    const errType = anthropicErr?.data?.error?.type;
    check("anthropic path emits event: error", Boolean(anthropicErr));
    check("anthropic error type is overloaded_error (retryable)", errType === "overloaded_error",
      `type=${JSON.stringify(errType)}`);
    check("anthropic error keeps the origin tag",
      /\[(connection-reset|stream-error|upstream-error|idle-timeout)\]/.test(anthropicErr?.data?.error?.message ?? ""),
      JSON.stringify(anthropicErr?.data?.error?.message ?? null));

    // ── report ──
    console.log();
    const failed = results.filter((r) => !r.pass);
    if (failed.length === 0) {
      console.log(`${results.length} checks passed — this build is safe to hot-swap.`);
    } else {
      console.error(`${failed.length} of ${results.length} checks FAILED:`);
      for (const f of failed) console.error(`  - ${f.name}${f.detail ? ` (${f.detail})` : ""}`);
      console.error("\nDo NOT hot-swap this build.");
      process.exitCode = 1;
    }
  } finally {
    cleanup();
  }
}

await main();
