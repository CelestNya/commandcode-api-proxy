# Development

> Personal fork of [thaolaptrinh/commandcode-api-proxy](https://github.com/thaolaptrinh/commandcode-api-proxy).
> The auth subsystem (stored key, `auth` subcommand, `--setup-*` generators) has been
> removed in favour of key passthrough; the structure below reflects this fork.
>
> The Rust rewrite is complete and is the only implementation. The Node sources
> and the conformance golden that pinned behaviour against them are gone; the
> behaviour contract lives in `RUST-REWRITE-SPEC.md` + `ADEVIATIONS.md`, and the
> test suite in this workspace is the acceptance gate.

## Prerequisites

- Rust stable (MSVC toolchain on Windows)

## Commands

```bash
cargo build --release --locked   # Both binaries: ccproxy + CCProxyTray
cargo test                       # Unit + fixture + ledger + tray tests (224)
cargo clippy --all-targets --all-features --locked -- -D warnings   # The lint gate
cargo fmt --check
build-rust.cmd                   # Package + publish to the hot-swap folder
```

## Windows tray (Rust, `crates/ccproxy-tray`)

The tray is a second Rust binary in the same workspace, not a separate artifact.
`build-rust.cmd` builds both binaries, runs `cargo test --locked` as the publish
gate (the conformance harness is gone), and publishes to
`%Desktop%\CCProxy-Release\CCProxy-v<version>-rust\`.
Pass `--promote` to switch `CCProxy-current` (a decision, not a side effect of building).

Test a build against an isolated namespace so a running production instance is
untouched:

```cmd
set CC_TRAY_NS=verify
set CC_TRAY_PORT=8897
CCProxyTray.exe --selfcheck
```

`--selfcheck` writes results to `selfcheck.log` (a `winexe` has no console) and
refuses to take over when a tray is already running. `chaos/handover-test.py`
exercises the two-phase handover on an isolated port; it is the M6 acceptance.

## Project structure

```
crates/
├── ccproxy/               # The proxy binary — #![forbid(unsafe_code)]
│   └── src/
│       ├── main.rs          # Entry: config, server startup, signal handling
│       ├── config.rs        # Config loader (env + CLI, validation and clamping)
│       ├── log.rs           # Level-filtered logger
│       ├── server.rs        # HTTP server, routing, CORS, body limits, lifecycle logs
│       ├── stream_body.rs   # Downstream SSE loop, splice retry, terminal records
│       ├── upstream.rs      # CC /alpha/generate client (retries, idle timeout)
│       ├── ndjson.rs        # CC NDJSON line parsing
│       ├── sse.rs           # SSE framing, StreamFailure classification
│       ├── billing.rs       # Per-attempt SQLite ledger (M7) + writer thread
│       ├── cli_version.rs   # Startup CLI-version lookup (with retry)
│       ├── models.rs        # Model resolution, aliasing, reasoning effort
│       ├── catalog.rs       # Dynamic model catalog (provider API + static merge)
│       ├── validation.rs    # Request validation (OpenAI + Anthropic)
│       ├── tool_arguments.rs
│       ├── usage.rs         # /health cache stats
│       ├── models.json      # Vendored model table (aliases, efforts, context)
│       └── translate/       # openai.rs, anthropic_stream.rs, openai_stream.rs,
│                            #   models.rs, catalog.rs, validation.rs, util.rs
└── ccproxy-tray/          # The tray binary — #![deny(unsafe_code)], FFI sites expect()
    └── src/
        ├── main.rs          # Role determination + handover loop
        ├── win.rs           # The only unsafe surface: job objects, mutex/events,
                              #   GetExtendedTcpTable, Shell_NotifyIcon, registry
        ├── owner.rs         # Handover verdict state machine (pure)
        ├── portguard.rs     # Port ownership policy (free/ours/foreign)
        ├── process.rs       # Child supervision, log rotation, egress env
        ├── tray.rs          # Menu, icon, UI state
        └── health.rs        # /health probe for the handover gate
build-rust.cmd            # Packaging + hot-swap publish (root; the release gate)
chaos/                     # Handover and soak scripts (manual, Python)
RUST-REWRITE-SPEC.md       # Behaviour contract (historical spec + deviations)
ADEVIATIONS.md             # Recorded intentional deviations from the Node version
```

## Tech stack

- **Language:** Rust (edition 2021), blocking thread-per-connection — no async runtime (see §4)
- **HTTP:** `tiny_http` (server) + `ureq` (client)
- **Storage:** `rusqlite` (bundled SQLite, WAL)
- **JSON:** `serde_json`
- **Test:** plain `#[test]` — unit, integration and fixture-replay suites

# Rust rewrite — engineering rules

Target: port this proxy to Rust, keeping the behaviour recorded in
`RUST-REWRITE-SPEC.md` + `ADEVIATIONS.md` (the Node version and its golden are
gone; the Rust test suite is the acceptance gate). These rules are the project's
Rust standard; they are normative for every crate in the workspace. Sources are
the Rust API Guidelines, the
Clippy lint reference, and the Tokio documentation, plus the conventions that
the community's better Rust skills converge on — no skill file is vendored.

## 1. Crate layout and unsafe

Two crates, split by where `unsafe` is allowed to live. This is what makes
`forbid(unsafe_code)` possible for everything that is not Win32 FFI.

| Crate | Contents | Unsafe policy |
| ----- | -------- | ------------- |
| `ccproxy` (bin) | Translation, NDJSON/SSE encoding, model catalog, config, HTTP server, billing ledger | `#![forbid(unsafe_code)]` |
| `ccproxy-tray` (bin) | Job Objects, mutex/event handover, `GetExtendedTcpTable`, tray UI, registry | `#![deny(unsafe_code)]` — all `unsafe` confined to `win.rs` |

- `forbid` cannot be lowered by a nested `#[allow]` — attempting it is itself a
  compile error, which is the point.
- Use `deny`, not `forbid`, inside `ccproxy-tray`, so each FFI site can carry
  `#[expect(unsafe_code)]` plus a comment naming the invariant it upholds.
- Do not use `unsafe` to work around the borrow checker. If a data structure
  needs it, find the safe abstraction first.

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
- Route all output through `log.rs` (the level-filtered logger). Never print
  from request-handling code — the two-line request lifecycle log is a contract,
  and stray stdout would corrupt it.

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
  samples in the fixture suite.
- **Never let an error type leak the API key.** CC's error bodies are echoed
  downstream; scrub `Bearer …` and control characters (pinned by tests).
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

Stack, all blocking (the finished proxy binary is 3.2 MB with the whole stack
linked in, against 88 MB for the bundled Node runtime):

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
  `#[tokio::test]` is needed. Snapshot/property tooling (`insta`, `proptest`) and
  extra runners (`nextest`, `cargo-deny`) were considered and deliberately
  dropped: `cargo test` covers everything they would, at none of their setup cost.
- **The fixture-replay suites are the primary acceptance tests.** The recorded
  transcripts live in `crates/ccproxy/tests/fixtures/` (moved out of the retired
  `conformance/` golden): `translate_golden.rs` replays all 52 translation
  samples, `stream_golden.rs` replays every stream transcript against both
  encoders.
- **Stream invariants the fixtures cannot see** are pinned by hand-rolled tests
  (`crates/ccproxy/tests/stream_body.rs`): byte-offset stream splits, block
  lifecycle (no delta before `content_block_start`, no record after
  `message_stop`), and the splice-retry transcripts.
- **The billing ledger is tested at both levels:** writer unit tests
  (numbering, NULL-vs-zero, backpressure drop, flush) and end-to-end fault
  injection driving real turns through the server with the ledger removed or
  broken (`crates/ccproxy/tests/billing_ledger.rs`).
- **The regression gate for every commit:** `cargo fmt --check` +
  `cargo clippy --all-targets -D warnings` + `cargo test`; behaviour commits
  additionally run `record.mjs --check`.

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

- **Decision made: raw `windows-sys`, no typed `windows` crate.** The tray's FFI
  surface is about a dozen well-understood calls, all confined to
  `ccproxy-tray/src/win.rs`; the typed crate would have generated far more
  compile time than it saved. `windows-sys` was already in the lock file via
  `rusqlite`. Pin the version — it moves fast.
- Features are opt-in per API namespace. `CreateJobObjectW` needs
  `Win32_Security` in addition to `Win32_System_JobObjects`; this is discovered
  at compile time and is easy to miss.
- **The tray is raw Shell_NotifyIcon plus a hidden message window** — no
  `tray-icon`/`tao`/`winit`. The menu, icon (GDI-drawn dots) and the handover
  state machine together are smaller than any windowing dependency would have
  been. Win32 mutexes are thread-affine, so the mutex has a dedicated ownership
  thread; releasing it from another thread fails silently.
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
  appears. stdout is then unavailable — `--selfcheck` writes `selfcheck.log`
  next to the exe.
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
  with this profile; the finished binaries are **3.2 MB** (`ccproxy.exe`) and
  **1.4 MB** (`CCProxyTray.exe`) — against 88 MB for the bundled Node runtime
  the rewrite replaces. `panic = "abort"` does not apply to test/bench profiles
  — Cargo forces unwind there. It also removes backtraces, which matters for a
  tray app whose only diagnostic channel is a log file; weigh that if field
  crashes become hard to diagnose.

