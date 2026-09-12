import { describe, it, expect, beforeEach, vi } from "vitest";

describe("loadConfig", () => {
  beforeEach(() => {
    vi.resetModules();
    vi.unstubAllEnvs();
  });

  it("uses default values when no env is set", async () => {
    vi.stubEnv("CC_API_KEY", "");
    vi.stubEnv("HOST", "");
    vi.stubEnv("PORT", "");

    const { loadConfig } = await import("@/config.js");
    const config = loadConfig();

    expect(config.host).toBe("127.0.0.1");
    expect(config.port).toBe(8787);
    expect(config.ccApiBase).toBe("https://api.commandcode.ai");
    expect(config.ccVersion).toBe("0.40.3");
    // Defaults: 10-minute connect, 2-minute idle.
    expect(config.upstreamTimeoutMs).toBe(600_000);
    expect(config.idleTimeoutMs).toBe(120_000);
  });

  it("reads upstream + idle timeouts from env vars", async () => {
    vi.stubEnv("CC_API_KEY", "");
    vi.stubEnv("CC_UPSTREAM_TIMEOUT_MS", "1800000"); // 30 min
    vi.stubEnv("CC_IDLE_TIMEOUT_MS", "300000"); // 5 min

    const { loadConfig } = await import("@/config.js");
    const config = loadConfig();

    expect(config.upstreamTimeoutMs).toBe(1_800_000);
    expect(config.idleTimeoutMs).toBe(300_000);
  });

  it("allows disabling the idle timeout by setting it to 0", async () => {
    vi.stubEnv("CC_API_KEY", "");
    vi.stubEnv("CC_IDLE_TIMEOUT_MS", "0");

    const { loadConfig } = await import("@/config.js");
    const config = loadConfig();

    expect(config.idleTimeoutMs).toBe(0);
  });

  it("falls back to defaults on invalid timeout values", async () => {
    vi.stubEnv("CC_API_KEY", "");
    vi.stubEnv("CC_UPSTREAM_TIMEOUT_MS", "not-a-number");
    vi.stubEnv("CC_IDLE_TIMEOUT_MS", "-5");

    const { loadConfig } = await import("@/config.js");
    const config = loadConfig();

    expect(config.upstreamTimeoutMs).toBe(600_000);
    expect(config.idleTimeoutMs).toBe(120_000);
  });

});

describe("fetchLatestCliVersion", () => {
  beforeEach(() => {
    vi.resetModules();
    vi.unstubAllEnvs();
  });

  it("returns the latest version from the npm registry", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValueOnce(
      new Response(JSON.stringify({ version: "0.41.0" }), { status: 200 }),
    );

    const { fetchLatestCliVersion } = await import("@/config.js");
    const version = await fetchLatestCliVersion();

    expect(version).toBe("0.41.0");
    expect(globalThis.fetch).toHaveBeenCalledWith(
      "https://registry.npmjs.org/command-code/latest",
      expect.objectContaining({ signal: expect.any(AbortSignal) }),
    );
  });

  it("returns null when the registry responds with an error", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValueOnce(new Response("not found", { status: 500 }));

    const { fetchLatestCliVersion } = await import("@/config.js");
    const version = await fetchLatestCliVersion();

    expect(version).toBeNull();
  });

  it("returns null when the request throws", async () => {
    vi.spyOn(globalThis, "fetch").mockRejectedValueOnce(new Error("network down"));

    const { fetchLatestCliVersion } = await import("@/config.js");
    const version = await fetchLatestCliVersion();

    expect(version).toBeNull();
  });

  it("caches the result so subsequent calls do not refetch", async () => {
    const fetchSpy = vi
      .spyOn(globalThis, "fetch")
      .mockResolvedValueOnce(new Response(JSON.stringify({ version: "0.41.0" }), { status: 200 }));

    const { fetchLatestCliVersion } = await import("@/config.js");
    expect(await fetchLatestCliVersion()).toBe("0.41.0");
    expect(await fetchLatestCliVersion()).toBe("0.41.0");
    expect(fetchSpy).toHaveBeenCalledTimes(1);
  });
});
