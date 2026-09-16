// Probe: does the response-header wait obey the upstream timeout or the idle timeout?
//
// This is the evidence for deviation #3 in `ADEVIATIONS.md`. Node arms
// `setTimeout(abort, CC_UPSTREAM_TIMEOUT_MS)` before the fetch and clears it
// once the headers arrive, so the header wait is bounded by that (larger)
// value. ureq applies a single socket read timeout for the whole connection
// life, so in the Rust build the header wait is bounded by CC_IDLE_TIMEOUT_MS
// instead. This probe runs either build against a mock that stalls before
// sending headers, with idle < stall < upstream, and reports which survives.
//
// Expected: Node SURVIVED, Rust FAILED. That difference is known and accepted
// (see ADEVIATIONS.md for why ureq cannot express the two-phase deadline).
//
// Usage:
//   node conformance/probe-slow-headers.mjs                      # the Node build
//   node conformance/probe-slow-headers.mjs --exe target/release/ccproxy.exe

import { createServer } from "node:http";
import { spawn } from "node:child_process";
import { setTimeout as sleep } from "node:timers/promises";

const EXE = process.argv.includes("--exe")
  ? process.argv[process.argv.indexOf("--exe") + 1]
  : null;
// The Node build is driven from its compiled entry point, one directory over.
const NODE_ENTRY = process.env.NODE_ENTRY ?? "../../oldproxy/dist/proxy.js";
const MOCK_PORT = Number(process.env.PROBE_MOCK_PORT ?? 19895);
const PROXY_PORT = Number(process.env.PROBE_PROXY_PORT ?? 18897);
const STALL_MS = 2000;
const IDLE_MS = 500;
const UPSTREAM_MS = 10000;

/** Holds the response headers back for STALL_MS, then answers normally. */
function startMock() {
  const server = createServer(async (req, res) => {
    const chunks = [];
    for await (const c of req) chunks.push(c);
    await sleep(STALL_MS);
    res.writeHead(200, { "Content-Type": "text/event-stream" });
    res.write('data: {"type":"text","text":"hi"}\n\n');
    res.write("data: [DONE]\n\n");
    res.end();
  });
  return new Promise((resolve) => {
    server.listen(MOCK_PORT, "127.0.0.1", () => resolve(server));
  });
}

function startProxy() {
  const env = {
    ...process.env,
    PORT: String(PROXY_PORT),
    CC_API_BASE: `http://127.0.0.1:${MOCK_PORT}`,
    CC_UPSTREAM_TIMEOUT_MS: String(UPSTREAM_MS),
    CC_IDLE_TIMEOUT_MS: String(IDLE_MS),
  };
  const child = EXE
    ? spawn(EXE, [], { env, stdio: ["ignore", "pipe", "pipe"] })
    : spawn(process.execPath, [NODE_ENTRY], { env, stdio: ["ignore", "pipe", "pipe"] });
  child.stdout.on("data", () => {});
  child.stderr.on("data", (d) => process.stderr.write(`[proxy] ${d}`));
  return child;
}

async function waitFor(url, timeoutMs = 15000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      await fetch(url);
      return true;
    } catch {
      await sleep(100);
    }
  }
  throw new Error(`timeout waiting for ${url}`);
}

async function main() {
  const mock = await startMock();
  const proxy = startProxy();
  try {
    await waitFor(`http://127.0.0.1:${PROXY_PORT}/health`);
    const started = Date.now();
    const res = await fetch(`http://127.0.0.1:${PROXY_PORT}/v1/chat/completions`, {
      method: "POST",
      headers: { "Content-Type": "application/json", Authorization: "Bearer probe-key-000000" },
      body: JSON.stringify({
        model: "deepseek-v4-flash",
        messages: [{ role: "user", content: "hi" }],
      }),
    });
    const text = await res.text();
    const elapsed = Date.now() - started;
    const kind = res.headers.get("content-type")?.includes("event-stream") ? "sse" : "json";
    console.log(
      `RESULT kind=${kind} status=${res.status} elapsed=${elapsed}ms ` +
        `verdict=${res.ok ? "SURVIVED" : "FAILED"}`,
    );
    if (!res.ok) console.log(`  body: ${text.slice(0, 300)}`);
  } finally {
    proxy.kill();
    mock.close();
  }
}

process.on("exit", () => process.exit(0));
main().catch((err) => {
  console.error("probe failed:", err);
  process.exit(1);
});
