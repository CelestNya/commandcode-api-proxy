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

# Rust rewrite — engineering rules

Target: port this proxy to Rust, keeping the behaviour in `conformance/golden/`
byte-identical. These rules are the project's Rust standard; they are normative
for every crate in the workspace. Sources are the Rust API Guidelines, the
Clippy lint reference, and the Tokio documentation, plus the conventions that
the community's better Rust skills converge on — no skill file is vendored.

## 1. Crate layout and unsafe

Three crates, not one. This is what makes `forbid(unsafe_code)` possible.

| Crate | Contents | Unsafe policy |
| ----- | -------- | ------------- |
| `ccproxy-core` | Translation, NDJSON/SSE encoding, model catalog, config | `#![forbid(unsafe_code)]` |
| `ccproxy-win32` | Job Objects, mutex/event handover, `GetExtendedTcpTable`, tray | `#![deny(unsafe_code)]` |
| `ccproxy` (bin) | Wiring, axum server, CLI | `#![forbid(unsafe_code)]` |

- Put `#![forbid(unsafe_code)]` at the crate root of `-core` and the binary.
  `forbid` cannot be lowered by a nested `#[allow]` — attempting it is itself a
  compile error, which is the point.
- Use `deny`, not `forbid`, inside `ccproxy-win32`, so each FFI site can carry
  `#[expect(unsafe_code)]` plus a comment naming the invariant it upholds.
- Do not use `unsafe` to work around the borrow checker. If a data structure
  needs it, use the right crate (`parking_lot`, `arc-swap`, `crossbeam`).

## 2. Lints — the gate

```bash
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo fmt --check
```

Default-on groups (`correctness`, `suspicious`, `style`, `complexity`, `perf`)
are already a hard error under `-D warnings`. Additionally deny these, which are
off by default:

```rust
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::todo,
    clippy::unimplemented,
    clippy::dbg_macro,
)]
```

- Cherry-pick from `pedantic`; never enable the group wholesale. Useful picks:
  `cast_possible_truncation`, `must_use_candidate`, `missing_errors_doc`.
- Never enable `restriction` as a group — Clippy warns if it finds
  `#![warn(clippy::restriction)]`.
- `indexing_slicing` will fire constantly in the NDJSON parser. Keep the crate
  deny and use `#[expect(clippy::indexing_slicing)]` on the few hot functions
  where the bound is locally provable, with a comment saying why.
- Prefer `#[expect(lint)]` over `#[allow(lint)]`: `expect` warns when the lint
  stops firing, so stale suppressions cannot rot silently.
- Route all output through `tracing`. Do not add `print_stdout` to the deny list
  and then fight it — just never print.

## 3. Errors

A proxy is both a library and a binary, so both idioms apply to different parts.

- **`ccproxy-core` exposes typed errors** with `thiserror`. One enum per domain
  (`TranslateError`, `UpstreamError`), each variant carrying `#[source]` so the
  chain survives. Include the data needed to build a response — a bare
  `UpstreamStatus(StatusCode)` loses the body that CC sends with the reason.
- **The binary composes with `anyhow`** and `.context()`. `anyhow` must not
  appear in a module that returns a typed error.
- **One `IntoResponse` impl decides every status code.** This is the single
  place HTTP semantics live, and the place to diff against the `failure/*`
  samples in `golden/behaviour.json`.
- **Never let an error type leak the API key.** CC's error bodies are echoed
  downstream; scrub `Bearer …` and control characters, as the Node version does
  in `reliability-upstream.test.ts`.
- Log at the boundary only — one `error!` where the request fails, not in the
  translation layer.

## 4. Async rules (tokio)

The streaming path is where the subtle bugs live.

- **Never block in an async context.** `std::fs`, `std::net`, `std::thread::sleep`
  and CPU-heavy work go in `spawn_blocking` or a dedicated thread.
- **`spawn_blocking` tasks cannot be cancelled.** Tokio's runtime shutdown waits
  for started ones indefinitely unless `shutdown_timeout` is set. For a tray app
  that must exit promptly, use a real thread for long-lived blocking work, and
  set an explicit shutdown timeout.
- **Cancellation safety is the core hazard here.** `select!` that reads the
  upstream body in one branch and a shutdown signal in another will lose bytes
  or desynchronise the NDJSON parser whenever the signal branch wins mid-read.
  `read_exact`, `read_to_end`, `read_to_string` and `write_all` are **not**
  cancellation-safe. Read into a buffer the loop owns across iterations, and
  treat cancellation as a hard stop — discard the buffer rather than resuming a
  partial line.
- `select!` panics if every branch is disabled and there is no `else`.
- Hold `std::sync::Mutex` for data protection; use `tokio::sync::Mutex` **only**
  when the guard must live across an `.await`. Never hold a std guard across
  await.
- Graceful shutdown: `axum::serve(...).with_graceful_shutdown(signal)` stops
  accepting, then `CancellationToken` (cloned per task) signals, then
  `TaskTracker::wait()` drains in-flight streams. Do not put connection draining
  in a `Drop` impl — destructors must not block (API Guidelines `C-DTOR-BLOCK`).
- Per the idempotency rule: if a function misbehaves when restarted while
  waiting at an `.await`, it is not cancellation-safe and must not sit in a
  `select!` branch.

## 5. HTTP and streaming

- **Server: axum** (thin over hyper, reuses tower/tower-http for timeouts,
  tracing, body limits). Raw hyper means reimplementing routing and extractors;
  actix-web has a different runtime model and ~10× less adoption.
- **Client: reqwest** with `default-features = false` and an explicit TLS
  backend, to keep the dependency tree (and binary) small.
- **Streaming:** prefer axum's `Sse` for the response side — it enforces
  `text/event-stream` framing and supports `KeepAlive` (off by default, which is
  a real trap behind idle-closing intermediaries). Use `async-stream`'s
  `stream!` for the NDJSON→SSE transform to keep the state machine readable.
- **JSON: `serde_json`. Do not reach for `simd-json` or `sonic-rs`.** The work
  here is per-line, small objects — exactly where SIMD parsers gain least.
  `simd-json` mutates buffers in place and is substantially `unsafe`;
  `sonic-rs` needs `-C target-cpu=native` and is not universally faster on
  serialise. Revisit only with a `criterion` benchmark proving a win.
- **Add `KeepAlive` on the SSE response.** The Node version relies on the
  client's own idle timeout; a Rust server behind the same intermediaries should
  send comment pings.

## 6. Testing

- **`#[tokio::test]`** for async units; **`insta`** for the translation layer —
  request/response shapes are exactly snapshot-shaped, and `cargo insta review`
  forces an explicit accept.
- **`proptest`** over `quickcheck` for the invariants the Node conformance test
  covers with a seeded PRNG: a stream split at arbitrary byte offsets must parse
  identically, and every emitted Anthropic record must satisfy the block
  lifecycle (no delta before `content_block_start`, no record after
  `message_stop`).
- **`wiremock`** for the upstream mock — it intercepts at the reqwest layer, so
  real serialisation is exercised.
- **`cargo-nextest`** for the run (`doctests` are not supported — run
  `cargo test --doc` separately). `--no-tests` exits non-zero by default.
- **`cargo-deny`** for advisories, licences, bans and sources; it is a superset
  of `cargo-audit` on advisories. Run it in CI.
- **The conformance harness is the primary acceptance test.** Load
  `conformance/golden/behaviour.json` and `translate.json` and assert against
  them; see `conformance/README.md`.

## 7. Naming and API shape

Per the Rust API Guidelines, applied to an application:

- `as_` for a cheap borrow, `to_` for an expensive conversion, `into_` when
  ownership is consumed. No `get_` prefix on accessors.
- Arguments convey meaning through types, not `bool`/`Option` (`C-CUSTOM-TYPE`).
  A `stream: bool` parameter becomes an enum.
- Validate config once at startup, not per request. The Node version parses and
  clamps all bounds in `loadConfig()`; do the same.
- Do not add `new_`-prefixed free functions; constructors are inherent methods.
- Borrow (`&str`, `&[T]`) unless ownership transfer is needed. Reach for
  `Cow<'_, T>` when ownership is genuinely ambiguous.
- Comments explain *why*; doc comments explain *what*. Every `TODO` links an
  issue.

## 8. Windows, tray, and binary size

- `windows` (typed) for the tray crate; `windows-sys` (raw, unsafe) when the
  typed surface is missing. Pin the version exactly — it moves fast.
- Features are opt-in per API namespace. `CreateJobObjectW` needs
  `Win32_Security` in addition to `Win32_System_JobObjects`; this is discovered
  at compile time and is easy to miss.
- **Job Object for child lifetime:** `CreateJobObjectW` →
  `SetInformationJobObject(JobObjectExtendedLimitInformation)` with
  `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` → `AssignProcessToJobObject` → hold the
  handle for the tray's lifetime. Closing the last handle kills the tree, so a
  hard-killed tray never orphans the proxy. Association cannot be broken once
  set.
- **Named mutex alone is not enough for handover.** The documented caveat is
  that a malicious process can create the mutex first and block startup. Pair it
  with a named event the incumbent waits on, and namespace per user
  (`Local\`, not `Global\`, or fast user switching collides).
- **`GetExtendedTcpTable`** needs the two-call pattern: call with a too-small
  `pdwSize` to get `ERROR_INSUFFICIENT_BUFFER` and the real size, then call
  again. AF_INET6 requires a `_OWNER_PID_*` table class — the `BASIC` classes
  return `ERROR_NOT_SUPPORTED`.
- **`#![windows_subsystem = "windows"]`** at the crate root only, so no console
  appears. stdout is then unavailable — keep the Node version's `--selfcheck`
  behaviour of writing `selfcheck.log` next to the exe.
- **Tray:** `tray-icon` needs an event loop running on its thread. `tao` is the
  Tauri-aligned choice; `winit` has wider adoption. Taking a full windowing
  dependency for an icon is heavy — calling the shell APIs directly through
  `windows-sys` is viable if the tray stays minimal.
- **Size profile** — the whole reason for the rewrite is the 88 MB Node runtime,
  so lock this in:

  ```toml
  [profile.release]
  opt-level = "s"    # "s" often beats "z"; "z" also disables loop vectorisation
  lto = true
  codegen-units = 1
  panic = "abort"    # removes backtraces — weigh against log-only diagnostics
  strip = true
  ```

  Measured on this machine: a hello-world binary is 1.17 MB default and 245 KB
  with this profile; with axum + reqwest + tokio + windows-rs linked in, 1.8 MB.
  `panic = "abort"` does not apply to test/bench profiles — Cargo forces unwind
  there.

