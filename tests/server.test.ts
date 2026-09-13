import { describe, it, expect, beforeAll, afterAll } from "vitest";
import http from "node:http";
import { readFileSync } from "node:fs";
import { loadConfig } from "@/config.js";
import { createServer } from "@/server.js";

const pkg = JSON.parse(readFileSync(new URL("../package.json", import.meta.url), "utf8")) as {
  version: string;
};

describe("Server", () => {
  let server: http.Server;
  const port = 18987;
  const baseUrl = `http://127.0.0.1:${port}`;

  beforeAll(async () => {
    // No server-held key: forces clients to send their own (keyless-passthrough
    // mode). This also lets the 401 test run without hitting the real CC API.
    const config = { ...loadConfig(), port, apiKey: null as string | null, host: "127.0.0.1" };
    server = createServer(config);

    return new Promise<void>((resolve) => {
      server.listen(port, "127.0.0.1", () => resolve());
    });
  });

  afterAll(() => {
    return new Promise<void>((resolve) => {
      server.close(() => resolve());
    });
  });

  it("returns 404 for unknown routes", async () => {
    const res = await fetch(`${baseUrl}/unknown`);
    expect(res.status).toBe(404);
    const body = await res.json();
    expect(body.error).toBe("Not found");
  });

  it("returns health status with the real package version", async () => {
    const res = await fetch(`${baseUrl}/health`);
    expect(res.status).toBe(200);
    const body = (await res.json()) as any;
    expect(body.status).toBe("ok");
    expect(body.version).toBe(pkg.version);
  });

  it("returns model list", async () => {
    const res = await fetch(`${baseUrl}/v1/models`);
    expect(res.status).toBe(200);
    const body = (await res.json()) as any;
    expect(body.object).toBe("list");
    expect(body.data.length).toBeGreaterThan(0);
    expect(body.data[0].id).toBeDefined();
    expect(body.data[0].object).toBe("model");
  });

  it("returns 401 for chat completions without API key", async () => {
    const res = await fetch(`${baseUrl}/v1/chat/completions`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ model: "default", messages: [{ role: "user", content: "hi" }] }),
    });
    expect(res.status).toBe(401);
  });

  it("supports CORS preflight", async () => {
    const res = await fetch(`${baseUrl}/v1/models`, {
      method: "OPTIONS",
    });
    expect(res.status).toBe(204);
    expect(res.headers.get("access-control-allow-origin")).toBe("*");
  });

  it("emits Vary: Origin so caches key on the origin", async () => {
    const res = await fetch(`${baseUrl}/health`);
    expect(res.headers.get("vary")).toBe("Origin");
  });

  it("returns 404 for OPTIONS on an unknown path (no blanket preflight)", async () => {
    const res = await fetch(`${baseUrl}/nope`, { method: "OPTIONS" });
    expect(res.status).toBe(404);
  });

  it("tolerates an X-Request-Id header without breaking routing", async () => {
    // No Authorization header → the 401 path runs first and no upstream call is
    // made, keeping this test offline. The point is the custom header is
    // accepted rather than causing a 500.
    const res = await fetch(`${baseUrl}/v1/chat/completions`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-Request-Id": "correlation-abc-123",
      },
      body: JSON.stringify({ model: "default", messages: [{ role: "user", content: "hi" }] }),
    });
    expect(res.status).toBe(401);
  });
});

describe("Server body-size guard", () => {
  let server: http.Server;
  const port = 18988;
  const baseUrl = `http://127.0.0.1:${port}`;
  const LIMIT = 1024;

  beforeAll(async () => {
    // A deliberately tiny limit makes the oversized path deterministic and
    // offline: the Content-Length pre-check rejects before any upstream call.
    const config = { ...loadConfig(), port, host: "127.0.0.1", maxBodyBytes: LIMIT };
    server = createServer(config);
    return new Promise<void>((resolve) => {
      server.listen(port, "127.0.0.1", () => resolve());
    });
  });

  afterAll(() => new Promise<void>((resolve) => server.close(() => resolve())));

  it("returns a JSON 413 (not a socket reset) for an oversized declared body", async () => {
    const res = await fetch(`${baseUrl}/v1/chat/completions`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Authorization: "Bearer test-key",
      },
      body: JSON.stringify({ model: "m", messages: [{ role: "user", content: "x".repeat(LIMIT * 2) }] }),
    });
    expect(res.status).toBe(413);
    const body = (await res.json()) as any;
    expect(body.error.message).toMatch(/too large/i);
  });

  it("keeps the connection usable after a 413 (keep-alive, no destroy)", async () => {
    // Same socket agent reused across calls: if the server destroyed the
    // socket, the follow-up request would fail with a reset.
    const agent = new http.Agent({ keepAlive: true, maxSockets: 1 });
    const big = JSON.stringify({ model: "m", messages: [{ role: "user", content: "y".repeat(LIMIT * 2) }] });
    const post = (body: string): Promise<number> =>
      new Promise((resolve, reject) => {
        const req = http.request(
          {
            host: "127.0.0.1",
            port,
            path: "/v1/chat/completions",
            method: "POST",
            agent,
            headers: {
              "Content-Type": "application/json",
              Authorization: "Bearer test-key",
              "Content-Length": String(Buffer.byteLength(body)),
            },
          },
          (res) => {
            res.resume();
            res.on("end", () => resolve(res.statusCode ?? 0));
          },
        );
        req.on("error", reject);
        req.write(body);
        req.end();
      });
    try {
      expect(await post(big)).toBe(413);
      expect(await post(big)).toBe(413);
    } finally {
      agent.destroy();
    }
  });
});
