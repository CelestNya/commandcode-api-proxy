interface CliArgs {
  host?: string;
  port?: string;
}

export interface Config {
  host: string;
  port: number;
  ccApiBase: string;
  ccVersion: string;
  logLevel: string;
  corsOrigin: string;
  /** Per-attempt deadline for upstream headers and any non-2xx error body. */
  upstreamTimeoutMs: number;
  /** Max ms between consecutive chunks during streaming. 0 = disabled. */
  idleTimeoutMs: number;
  /** Maximum request body size in bytes (Content-Length pre-check + streaming guard). */
  maxBodyBytes: number;
}

/**
 * Hardcoded CLI version fallback. The real CLI ships frequent releases, so
 * `fetchLatestCliVersion()` should be used to refresh this at startup. CC's
 * server actively blocks requests whose version looks stale or absent.
 */
export const DEFAULT_CC_VERSION = "0.40.3";
export const DEFAULT_CC_API_BASE = "https://api.commandcode.ai";
const CC_VERSION_REFRESH_MS = 24 * 60 * 60 * 1000;

let cachedVersion: string | null = null;
let lastFetchAt = 0;

/**
 * Fetch the latest published `command-code` CLI version from the npm registry.
 * Returns `null` on any failure (caller falls back to DEFAULT_CC_VERSION).
 * Cached for CC_VERSION_REFRESH_MS.
 */
export async function fetchLatestCliVersion(): Promise<string | null> {
  if (cachedVersion && Date.now() - lastFetchAt < CC_VERSION_REFRESH_MS) {
    return cachedVersion;
  }
  try {
    const res = await fetch("https://registry.npmjs.org/command-code/latest", {
      signal: AbortSignal.timeout(10_000),
    });
    if (!res.ok) return null;
    const pkg = (await res.json()) as { version?: string };
    if (pkg.version && typeof pkg.version === "string") {
      cachedVersion = pkg.version;
      lastFetchAt = Date.now();
      return cachedVersion;
    }
    return null;
  } catch {
    return null;
  }
}

function parseCliArgs(): CliArgs {
  const args = process.argv.slice(2);
  const map: CliArgs = {};
  for (let i = 0; i < args.length; i++) {
    const arg = args[i];
    if (!arg.startsWith("--")) continue;
    const key = arg.slice(2);
    if (!key) continue;
    const next = args[i + 1];
    if (next != null && !next.startsWith("--")) {
      (map as Record<string, string | undefined>)[key] = next;
      i++;
    } else {
      // Flag without value (e.g. --help). Store as boolean true but keep
      // host/port handling safe: loadConfig treats boolean as missing.
      (map as Record<string, unknown>)[key] = true as unknown as string;
    }
  }
  return map;
}

// ── Validation helpers ─────────────────────────────────────

function parsePort(raw: string | undefined, fallback: number): number {
  if (raw == null || raw === "" || raw === "true") return fallback;
  const n = Number(raw);
  if (!Number.isInteger(n) || n < 1 || n > 65535) return fallback;
  return n;
}

function parseHost(raw: string | undefined): string {
  const fallback = "127.0.0.1";
  if (raw == null || raw === "" || raw === "true") return fallback;
  // Reject whitespace and control characters rather than letting a garbage
  // value reach listen() and fail with a less actionable error.
  for (let i = 0; i < raw.length; i++) {
    const code = raw.charCodeAt(i);
    if (code <= 0x20 || code === 0x7f) return fallback;
  }
  return raw;
}

const MAX_BODY_BYTES_DEFAULT = 10 * 1024 * 1024;
const MAX_BODY_BYTES_MAX = 50 * 1024 * 1024;

function parseBodyLimit(raw: string | undefined): number {
  if (raw == null || raw === "") return MAX_BODY_BYTES_DEFAULT;
  const n = Number(raw);
  if (!Number.isFinite(n) || n <= 0) return MAX_BODY_BYTES_DEFAULT;
  return Math.min(Math.floor(n), MAX_BODY_BYTES_MAX);
}

// Clamp upstream timeouts to a sane ceiling so a typo like 999999999
// doesn't create a half-hour zombie request. 0 is allowed for idle (disabled).
const UPSTREAM_TIMEOUT_MAX_MS = 30 * 60 * 1000;
const IDLE_TIMEOUT_MAX_MS = 30 * 60 * 1000;

function clampTimeout(value: number, max: number, fallback: number): number {
  if (!Number.isFinite(value) || value < 0) return fallback;
  if (value === 0) return 0;
  return Math.min(Math.floor(value), max);
}

export function loadConfig(): Config {
  const cli = parseCliArgs();

  const host = parseHost(cli.host ?? process.env.HOST);
  const port = parsePort(cli.port ?? process.env.PORT, 8787);
  const ccApiBase = process.env.CC_API_BASE || DEFAULT_CC_API_BASE;
  const ccVersion = process.env.CC_CLI_VERSION || cachedVersion || DEFAULT_CC_VERSION;
  const logLevel = process.env.LOG_LEVEL || "info";
  // `*` is fine for a localhost proxy; restrict (e.g. to an origin or leave
  // empty to disable) before exposing the proxy on a network.
  const corsOrigin = process.env.CORS_ORIGIN ?? "*";
  const maxBodyBytes = parseBodyLimit(process.env.CC_MAX_BODY_BYTES);

  // Upstream timeouts. The connection timeout covers the wall-clock time
  // until the upstream returns headers (and consumes any error body) — bump it for
  // slow reasoning models. The idle timeout catches stalled streams where
  // the upstream opened the connection but stopped sending chunks
  // mid-response (e.g. tool call hung on the upstream side). Set
  // CC_IDLE_TIMEOUT_MS=0 to disable idle detection entirely.
  const rawUpstream = parsePositiveInt(process.env.CC_UPSTREAM_TIMEOUT_MS, 600_000);
  const rawIdle = parsePositiveInt(process.env.CC_IDLE_TIMEOUT_MS, 120_000);
  const upstreamTimeoutMs = clampTimeout(rawUpstream, UPSTREAM_TIMEOUT_MAX_MS, 600_000);
  const idleTimeoutMs = clampTimeout(rawIdle, IDLE_TIMEOUT_MAX_MS, 120_000);

  return {
    host,
    port,
    ccApiBase,
    ccVersion,
    logLevel,
    corsOrigin,
    upstreamTimeoutMs,
    idleTimeoutMs,
    maxBodyBytes,
  };
}

function parsePositiveInt(raw: string | undefined, fallback: number): number {
  if (raw == null || raw === "") return fallback;
  const n = Number(raw);
  if (!Number.isFinite(n) || n < 0) return fallback;
  return Math.floor(n);
}
