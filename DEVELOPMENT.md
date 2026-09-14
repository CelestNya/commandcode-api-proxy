# Development

> Personal fork of [thaolaptrinh/commandcode-api-proxy](https://github.com/thaolaptrinh/commandcode-api-proxy).
> The auth subsystem (stored key, `auth` subcommand, `--setup-*` generators) has been
> removed in favour of key passthrough; the structure below reflects this fork.

## Prerequisites

- Node.js >= 24
- pnpm

## Setup

```bash
git clone https://github.com/CelestNya/commandcode-api-proxy.git
cd commandcode-api-proxy
pnpm install
```

## Commands

```bash
pnpm install       # Install dependencies
pnpm dev           # Dev mode with hot reload (tsx)
pnpm build         # Build to dist/
pnpm test          # Run tests
pnpm test:watch    # Run tests in watch mode
pnpm test:coverage # Run tests with coverage
pnpm lint          # Lint (oxlint via vite-plus)
pnpm fmt           # Format (oxfmt via vite-plus)
```

Scripts are defined in `package.json`. The `make` shortcuts are also available if you have `make` installed.

## Windows tray

The tray is a separate C# artifact, built by `tray\build.cmd`:

```cmd
pnpm build           :: dist/ must be fresh first - the packager refuses a stale dist
pnpm release:tray    :: compiles CCProxyTray.exe and publishes to the Desktop
```

`build.cmd` assembles a portable package (`node.exe` + `dist/` + `package.json`),
copies it into `%Desktop%\CCProxy-Release\CCProxy-v<version>\` and mirrors it to
`CCProxy-current\`, keeping older version folders for rollback. The version comes
from `package.json` - the single source of truth, also shown in the tray tooltip.

Test a build against an isolated namespace so a running production instance is
untouched:

```cmd
set CC_TRAY_NS=verify
set CC_TRAY_PORT=8897
CCProxyTray.exe --selfcheck
```

`--selfcheck` writes results to `selfcheck.log` (a `winexe` has no console) and
refuses to take over when a tray is already running.

## Project structure

```
src/
├── proxy.ts              # Entry point: load config, start server, signal handling
├── config.ts             # Config loader (env + CLI, validation and clamping)
├── logger.ts             # Level-filtered console logger
├── models.json           # Model list, aliases, context windows, reasoning efforts
├── version.ts            # Proxy version lookup
├── server.ts             # HTTP server, routing, request/response handling
├── stream.ts             # NDJSON parsing, SSE formatting, error tagging
├── upstream.ts           # CC /alpha/generate client (retries, idle timeout)
├── usage-stats.ts        # Per-request usage accounting + JSONL persistence
└── translate/
    ├── types.ts            # Shared types (OpenAI, CC, UsageData)
    ├── models.ts           # Model resolution, aliasing, reasoning effort
    ├── catalog.ts          # Dynamic model catalog (provider API + static merge)
    ├── util.ts             # CC helpers (usage extraction, tool pruning, safeguard)
    ├── validation.ts       # Request validation (OpenAI + Anthropic)
    ├── tool-arguments.ts   # Tool-argument string handling
    ├── openai.ts           # OpenAI <-> CC translation
    ├── anthropic-types.ts  # Anthropic API types
    ├── anthropic-models.ts # claude-* -> CC model mapping
    └── anthropic.ts        # Anthropic <-> CC translation
tests/                      # Vitest suites (unit + e2e + reliability)
tray/                       # C# tray manager (Tray.cs) and its build.cmd
chaos/                      # Handover and soak scripts (manual, Python/Node)
```

## Tech stack

- **Runtime:** Node.js (zero runtime dependencies — built-ins only)
- **Build:** TypeScript + tsc-alias
- **Test:** Vitest
- **Lint:** Oxlint (via vite-plus)
- **Format:** Oxfmt (via vite-plus)
