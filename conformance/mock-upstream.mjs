// Scriptable mock of the Command Code upstream (`/alpha/generate` + `/provider/v1/models`).
//
// The conformance harness drives the proxy against this server so that every
// upstream behaviour — clean turns, malformed NDJSON, mid-stream resets, slow
// headers — is reproducible byte for byte. The scenario document is supplied by
// the harness over the control channel; nothing is global mutable state.
//
// Control channel (same port, distinct paths, never proxied):
//   POST /__scenario  { scenario }  load a scenario, reset the request log
//   GET  /__log                     every upstream request the proxy made
//   GET  /__ready                   readiness probe
//
// Usage: node conformance/mock-upstream.mjs [port]     (default 19888)

import http from "node:http";
import { readFileSync } from "node:fs";

const PORT = Number(process.argv[2] ?? 19888);

/** @type {{name: string, status?: number, headers?: Record<string,string>, ndjson?: unknown[], raw?: string, delayMs?: number, resetAfterBytes?: number, resetAfterMs?: number, hangAfterEvents?: number}} */
let scenario = { name: "unset", ndjson: [] };

/** Every request the proxy sent upstream, in order. */
let requestLog = [];

/** Models payload for the catalog endpoint; scenarios may override it. */
let modelsPayload = {
  object: "list",
  data: [{ id: "deepseek/deepseek-v4-flash", object: "model" }],
};

function readBody(req) {
  return new Promise((resolve) => {
    let raw = "";
    req.on("data", (c) => (raw += c));
    req.on("end", () => resolve(raw));
  });
}

function sendJson(res, status, obj) {
  const body = JSON.stringify(obj);
  res.writeHead(status, {
    "Content-Type": "application/json",
    "Content-Length": Buffer.byteLength(body),
  });
  res.end(body);
}

/** CC streams newline-delimited JSON over a 200 that never closes early. */
function writeNdjson(res, event) {
  res.write("data: " + JSON.stringify(event) + "\n");
}

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url ?? "/", `http://127.0.0.1:${PORT}`);

  // ── control channel ──
  if (url.pathname === "/__ready") return sendJson(res, 200, { ready: true });
  if (url.pathname === "/__log") return sendJson(res, 200, { requests: requestLog });
  if (url.pathname === "/__scenario") {
    const body = await readBody(req);
    const parsed = JSON.parse(body);
    scenario = parsed.scenario ?? scenario;
    if (parsed.models) modelsPayload = parsed.models;
    requestLog = [];
    return sendJson(res, 200, { loaded: scenario.name });
  }

  // ── recorded upstream surface ──
  requestLog.push({
    method: req.method,
    path: url.pathname,
    headers: { ...req.headers },
    body: url.pathname.includes("/alpha/generate") ? JSON.parse((await readBody(req)) || "null") : null,
  });

  if (url.pathname.startsWith("/provider/v1/models")) {
    return sendJson(res, 200, modelsPayload);
  }
  if (!url.pathname.startsWith("/alpha/generate")) {
    return sendJson(res, 404, { error: { message: "no such upstream route" } });
  }

  // ── scripted responses ──
  const s = scenario;

  if (s.delayMs) {
    await new Promise((r) => setTimeout(r, s.delayMs));
  }

  if (s.status && s.status !== 200) {
    return sendJson(res, s.status, s.errorBody ?? { error: { message: `scripted ${s.status}` } });
  }

  res.writeHead(200, {
    "Content-Type": "application/json",
    "Cache-Control": "no-cache",
    ...(s.headers ?? {}),
  });

  if (s.raw !== undefined) {
    res.write(s.raw);
    return res.end();
  }

  const events = s.ndjson ?? [];
  let written = 0;
  for (const event of events) {
    // Hang *after* writing this many events: stop sending, but keep the
    // response open so the proxy's idle timeout (not a clean EOF) is what
    // observes the stall. `return` alone would end the response, which the
    // proxy sees as a normal end-of-stream — that is how this scenario
    // silently stopped exercising the idle path before.
    if (s.hangAfterEvents !== undefined && written >= s.hangAfterEvents) {
      return; // leave the socket open and silent — exercises the idle timeout
    }
    writeNdjson(res, event);
    written += 1;
    if (s.resetAfterBytes !== undefined && written >= s.resetAfterBytes) {
      return res.socket?.destroy();
    }
    if (s.eventDelayMs) await new Promise((r) => setTimeout(r, s.eventDelayMs));
  }
  // Script exhausted. If the scenario asked to hang, do it now — otherwise a
  // scenario whose `hangAfterEvents` equals its event count would end cleanly.
  if (s.hangAfterEvents !== undefined) {
    return; // socket stays open and silent
  }
  if (s.resetAfterMs !== undefined) {
    setTimeout(() => res.socket?.destroy(), s.resetAfterMs);
    return;
  }
  res.end();
});

server.listen(PORT, "127.0.0.1", () => {
  // The harness waits for this line before sending traffic.
  console.log(`mock upstream ready on ${PORT}`);
});
