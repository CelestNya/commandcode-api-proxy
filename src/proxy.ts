#!/usr/bin/env node

// Personal build: the API key is always passed through from the client's own
// Authorization header. There is no stored key, no auth subcommand, and no
// first-run prompt — a request without a key simply gets a 401.

import { loadConfig, fetchLatestCliVersion } from "@/config.js";
import { createServer } from "@/server.js";
import { logger, initLogger } from "@/logger.js";
import { getProxyVersion } from "@/version.js";

const config = loadConfig();
initLogger(config.logLevel);

// Warn if the configured port looks wrong (typo, privileged, etc.) — the
// server itself also guards EADDRINUSE/EACCES, but early feedback is cheaper.
if (config.port < 1024) {
  logger.warn(`Port ${config.port} is privileged (<1024); prefer 8787 or another high port.`);
}

if (!process.env.CC_CLI_VERSION) {
  const latest = await fetchLatestCliVersion();
  if (latest) config.ccVersion = latest;
}

const server = createServer(config);

server.on("error", (err: NodeJS.ErrnoException) => {
  if (err.code === "EADDRINUSE") {
    logger.error(`Port ${config.port} is already in use (EADDRINUSE). Is another proxy running?`);
    process.exit(2);
  } else if (err.code === "EACCES") {
    logger.error(`Permission denied for ${config.host}:${config.port} (EACCES).`);
    process.exit(2);
  } else {
    logger.error(`Failed to listen on ${config.host}:${config.port}: ${err.message}`);
    process.exit(2);
  }
});

server.listen(config.port, config.host, () => {
  console.log(`\n  Command Code API Proxy v${getProxyVersion()} (personal build)`);
  console.log(`  ${"=".repeat(50)}`);
  console.log(`  Listening on  http://${config.host}:${config.port}`);
  console.log(`  Auth: passthrough (client's own key on every request)`);
  console.log("");
  console.log("  Endpoints:");
  console.log("    GET  /health");
  console.log("    GET  /v1/models");
  console.log("    POST /v1/chat/completions  (OpenAI format)");
  console.log("    POST /v1/messages          (Anthropic format)");
  console.log("    POST /v1/messages/count_tokens  (Anthropic format)");
  console.log("");
  console.log("  Press Ctrl+C to stop\n");
});

// Never let an unexpected async error crash the proxy silently — log and keep
// serving. (Route handlers already catch their own errors; this is a backstop.)
process.on("unhandledRejection", (reason) => {
  logger.error("[fatal] Unhandled promise rejection:", reason);
});
process.on("uncaughtException", (err) => {
  logger.error("[fatal] Uncaught exception:", err);
});

const shutdown = (signal: string): void => {
  logger.info(`Received ${signal}, shutting down...`);
  // Don't hang forever waiting on a stuck streaming connection.
  const force = setTimeout(() => {
    logger.warn("Forcing shutdown after 10s timeout");
    process.exit(1);
  }, 10_000);
  force.unref();
  server.close(() => process.exit(0));
};

process.on("SIGINT", () => shutdown("SIGINT"));
process.on("SIGTERM", () => shutdown("SIGTERM"));
