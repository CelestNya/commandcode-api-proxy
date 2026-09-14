# Conformance harness

The behaviour contract for the Rust rewrite. It captures what the Node
implementation **does**, not what its source says, and freezes it in a form the
Rust implementation can be diffed against.

## Why this exists

The rewrite must preserve a lot of behaviour that is easy to lose and hard to
notice: the exact SSE event ordering, which error types are retryable, how the
upstream request is shaped, what happens when upstream dies mid-stream. Unit
tests cover those in TypeScript, but they express intent in TypeScript types and
call internal functions — none of that survives a port.

These samples are recorded from the **outside**: HTTP requests in, HTTP
responses out, plus a log of what the proxy sent upstream. Any language can
replay them.

## Layout

```
conformance/
├── mock-upstream.mjs        scriptable CC upstream (NDJSON scripts, fault injection)
├── record.mjs               drives the proxy over HTTP, writes golden/behaviour.json
├── record-translate.mjs     pure-function samples, writes golden/translate.json
├── scenarios/
│   └── upstream-scenarios.json   the scripted upstream event sequences
├── client-probes/           what the *client* does with what we send (not diffed)
│   ├── partial-context.mjs      does a broken stream lose the text already sent?
│   ├── upstream-continuation.mjs can a truncated turn be resumed?
│   └── observed/                probe output, checked in as evidence
└── golden/
    ├── behaviour.json       67 samples: streaming, failures, surface, validation, aliases
    └── translate.json       39 samples: request translation + model resolution
```

## What is frozen

| Group | Count | Covers |
| ----- | ----- | ------ |
| `stream/*` | 32 | Every upstream scenario × both downstream dialects: event order, block lifecycle, terminal records, synthesised finishes |
| `nonstream/*` | 8 | The same upstream collapsed into a single response, both dialects |
| `failure/*` | 12 | Upstream status mapping (401/403/429/500/529/400) into each envelope shape |
| `httpSurface` | 8 | `/health`, `/v1/models` in both shapes, 404, CORS preflight (known + unknown), 401 |
| `validation` | 7 | Requests rejected locally, never reaching upstream |
| `modelResolution` | 6+ | Alias → upstream model id, including unknowns and case handling |
| `translate.json` | 39 | `toCCRequest` for both dialects, plus the full alias table |

Each `stream`/`failure` case also records **the upstream request** the proxy
made — method, path, headers and body. CC rejects requests that don't look like
the official CLI, so the header set (including the CLI-identifying ones) is part
of the contract, not an implementation detail.

## `client-probes/` — the other half of the contract

`golden/` records what the proxy **sends**. These probes record what a real
client SDK **does** with it, which is where several non-obvious behaviours live
and where two claims in the spec turned out to be wrong.

They need the real client SDKs (`@anthropic-ai/sdk`, `ai` +
`@ai-sdk/openai-compatible`) and are **not** part of `conformance:check` —
`upstream-continuation.mjs` additionally needs a live upstream and a key. Run
them by hand when changing anything about how a stream ends:

```bash
node conformance/client-probes/partial-context.mjs        # offline, fake upstream
node conformance/client-probes/upstream-continuation.mjs  # needs the real proxy up
```

Findings worth keeping (full detail in `RUST-REWRITE-SPEC.md` §2.5 and §8.7):

- **A broken stream does not lose the text already sent.** Both SDKs keep the
  partial text in the message they hand back, so a mid-stream failure is a
  *truncation*, not a *discard*.
- **A truncated tool call becomes an empty-argument tool call.** The partial
  JSON is dropped and `input` ends up `{}` — the one case where "just truncate
  it" is dangerous.
- **CC does not implement Anthropic's prefill.** Ending a request with an
  `assistant` message makes the model rewrite from the start, not continue.
  Telling it "continue from where you stopped" in a user message does work.

## Running it

```bash
pnpm build                  # record against dist/, so build first
pnpm conformance:record     # capture → golden/
pnpm conformance:check      # replay and diff against golden/
```

The harness starts its own mock upstream and proxy on ports 19888/18899 and
tears them down on exit. It never touches a running instance — production is on
8787 and is not involved.

## What is deliberately NOT frozen

Run-varying values are normalised to placeholders so the diff stays meaningful:

| Value | Becomes | Why |
| ----- | ------- | --- |
| Message ids (`msg_<uuid>`, `chatcmpl-<uuid>`) | `<uuid>` | Random per response |
| `traceparent` / `x-session-id` | `<trace-id>` / `<uuid>` | Random per request |
| `created` / `created_at` | `<epoch>` | Wall clock |
| `config.environment`, `workingDir`, `date` | `<platform>`, `<cwd>`, `<date>` | Host-specific |
| Timestamps in logs | `<timestamp>` | Wall clock |

The harness keeps the **shape** and discards the **value**. So a case still
fails if a field disappears or changes type, but passes if only the random id
differs.

Verified deterministic: three consecutive `--check` runs produce zero
differences. If you see a spurious diff, that is a harness bug — fix it rather
than regenerating the golden file.

## Regenerating after an intentional change

The golden files are the contract, so changing them is a decision, not a
chore:

```bash
pnpm conformance:check      # see exactly which cases differ, by name
pnpm conformance:record     # accept the new behaviour
git diff conformance/golden/   # review every change before committing
```

Review the diff. A regenerated sample that nobody read is worse than no sample
at all — it silently redefines correct.

## For the Rust implementation

Two ways to use this, in increasing strictness:

1. **Replay the same scenarios and diff transcripts.** Point a Rust-side
   recorder at the Rust binary using the same `mock-upstream.mjs`, and compare
   its output against `golden/behaviour.json`. The mock and scenario files are
   plain Node/JSON and need no port. This is the acceptance test for the rewrite
   as a whole.

2. **Drive the samples directly in `cargo test`.** Each case in
   `golden/behaviour.json` has `upstreamRequests` (what to assert the client
   sends) and `downstream` (what to assert the server returns). Load the JSON
   with `serde_json` and assert against it — there is no need to reimplement the
   harness to get regression coverage.

For `golden/translate.json`, the mapping is a straight unit test: feed the
`request` in, compare the returned structure field by field.

The `failure/*` group is the highest-value part to port first: which error type
goes in the envelope decides whether the downstream client retries, and that
logic was reverse-engineered from the client's classifier rather than from any
spec.
