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
| `ccproxy` (bin) | Wiring, HTTP server, CLI | `#![forbid(unsafe_code)]` |

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

## 4. Concurrency: threads, not async

**The project is blocking, thread-per-connection, with no tokio anywhere.** This
is a deliberate decision (see `RUST-REWRITE-SPEC.md` §5.5), not an oversight —
do not introduce an async runtime later without revisiting it.

Rationale, measured: the proxy is a pure I/O forwarder with few, long-lived
connections. Production runs about one request per 35 seconds with a peak of 32
concurrent, and a blocked thread costs 24 KB RSS and **zero CPU** (measured over
1000 idle blocked threads). The machine has 16 logical cores; the other target
has 4. Threads are ample, and choosing them removes a whole class of complexity
rather than adding cost:

| Removed by not using async | Why it mattered here |
| -------------------------- | -------------------- |
| `select!` cancellation safety | The subtle hazard for a streaming proxy: a signal branch winning mid-read loses bytes or desynchronises the NDJSON parser. `read_exact` / `read_to_end` / `write_all` are not cancellation-safe. With blocking reads there is no cancellation to reason about |
| `spawn_blocking` wrapper | `rusqlite` is synchronous; under threads it is used directly |
| Runtime shutdown hangs | No runtime to shut down; the writer thread is joined explicitly |
| `Send + 'static` bounds | Shared state needs only `Arc` plus a lock |

Rules that follow:

- **One thread per connection, with a hard cap** (64). Past the cap, refuse the
  connection rather than queueing — the failure mode being defended against is
  a slow or malicious client holding a thread forever, not throughput.
- **Reduce the stack size** (`std::thread::Builder::stack_size`, 512 KB instead
  of the 2 MB default). 64 threads then cost ~1.5 MB rather than ~128 MB.
- **Do not add a total-duration cap on a request.** The only timeouts are the
  header deadline and the byte-interval idle timeout. A legitimate long
  generation must never be cut off mid-answer; that is the worst possible
  failure (content already delivered, retry starts over and is billed again).
  Measured production data confirms long turns are real.
- **Backpressure is implicit.** A slow downstream client blocks the thread in
  `write`, which stops reads from upstream and lets TCP windowing slow the
  source. Do not buffer to "avoid blocking" — that turns backpressure into
  unbounded memory growth.
- **Never hold a lock across a blocking I/O call.** Take the lock, copy what is
  needed, release, then do I/O.
- **Sharing state:** immutable config behind `Arc`; the model catalog behind
  `RwLock` (read-mostly) or `arc-swap`; the usage writer receives records over a
  bounded `std::sync::mpsc` channel from a dedicated writer thread that owns the
  connection. `try_send` only — a full channel drops the record and counts it,
  never blocks the request path.
- **Graceful shutdown:** stop accepting, then let in-flight responses finish
  with a bounded grace period, then exit. Do not put draining in a `Drop` impl —
  destructors must not block (API Guidelines `C-DTOR-BLOCK`).

## 5. HTTP and streaming

Stack, all blocking (measured: 2.2 MB binary with the whole stack linked in,
against 88 MB for the bundled Node runtime):

| Layer | Choice | Why |
| ----- | ------ | --- |
| Server | `tiny_http` | API is "a request comes in, you return a response" — no async runtime, no extractor machinery to learn |
| Client | `ureq` | Blocking, TLS-capable, streams the response body |
| Storage | `rusqlite` (bundled) | Synchronous, fits the thread model directly |
| JSON | `serde_json` | See below |

- **Do not reach for `simd-json` or `sonic-rs`.** The work here is per-line,
  small objects — exactly where SIMD parsers gain least. `simd-json` mutates
  buffers in place and is substantially `unsafe`; `sonic-rs` needs
  `-C target-cpu=native` and is not universally faster on serialise. Revisit
  only with a `criterion` benchmark proving a win.
- **Write each SSE record completely, then flush.** One record is `event:` line
  + `data:` line + **a blank line**; the blank line is what dispatches the event,
  and per the WHATWG spec *"once the end of the file is reached, any pending
  data must be discarded"*. Ending the response right after a `data:` line
  silently drops the final event — plausibly `message_stop`, which looks exactly
  like a turn stopping with no message. This is the single easiest thing to get
  wrong when rewriting the write loop.
- **`[DONE]` must be bare and standalone.** The OpenAI Node SDK compares for
  equality, so a trailing space makes it attempt `JSON.parse("[DONE] ")` and
  throw.
- **Derive the SSE `event:` name and the payload `type` from one constructor.**
  They must always agree, because the Anthropic SDKs dispatch on the event name
  (a whitelist that drops unknown names *silently*) while the Vercel AI SDK
  ignores the event name entirely and validates `type` against a Zod union.
  They are exact inverses; only emitting both consistently satisfies both. The
  Node version achieved this by discipline — the Rust version should make it
  impossible to violate.
- **Consider `KeepAlive` comment pings** if the deployment sits behind an
  intermediary that closes idle connections. The Node version relies on the
  client's own timeout, and no intermediary has caused a problem so far.

## 6. Testing

- **Plain `#[test]` everywhere** — there is no async runtime to bridge, so no
  `#[tokio::test]` is needed. **`insta`** for the translation layer —
  request/response shapes are exactly snapshot-shaped, and `cargo insta review`
  forces an explicit accept.
- **`proptest`** over `quickcheck` for the invariants the Node conformance test
  covers with a seeded PRNG: a stream split at arbitrary byte offsets must parse
  identically, and every emitted Anthropic record must satisfy the block
  lifecycle (no delta before `content_block_start`, no record after
  `message_stop`).
- **The conformance harness is the primary acceptance test**, and it needs no
  port: load `conformance/golden/*.json` and assert against it, or replay the
  scenarios through `conformance/mock-upstream.mjs` (plain Node, no Rust-side
  equivalent required). For an upstream mock inside `cargo test`, `httpmock`
  works with blocking clients; `wiremock` is async-oriented and a poor fit here.
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
  with this profile; with `tiny_http` + `ureq` + `rusqlite` (bundled) linked in,
  **2.2 MB** — against 88 MB for the bundled Node runtime the rewrite replaces.
  `panic = "abort"` does not apply to test/bench profiles — Cargo forces unwind
  there. It also removes backtraces, which matters for a tray app whose only
  diagnostic channel is a log file; weigh that if field crashes become hard to
  diagnose.

