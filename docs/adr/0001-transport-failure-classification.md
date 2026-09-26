# ADR 0001 — Classify upstream transport failures by structure, not by message

- **Date:** 2026-09-23 (amended 2026-09-26 after review: `Route` per-URL instead
  of a global flag, the pure-core table tests, and the client-envelope decision
  recorded under Consequences)
- **Status:** Accepted
- **Supersedes:** the `timeout_tag()` heuristic in `crates/ccproxy/src/upstream.rs`

## Context

The proxy's only path to the upstream (`/alpha/generate`) is over `ureq`, and
when it fails the failure has to be named. That name appears twice: in the
`[reject]` log line an operator greps, and in the `errorTag` column of the
billing ledger the WebUI reads. Until now both came from one function:

```rust
fn timeout_tag(err: &UpstreamError) -> &'static str {
    let lowered = err.message.to_lowercase();
    if lowered.contains("timed out") || lowered.contains("timeout") {
        "http-timeout"
    } else {
        "http-network"
    }
}
```

It reads the error's **message text**. `ureq` builds that text by appending the
underlying OS error verbatim, and on a non-English Windows that text is
localised — a socket timeout reads:

```
Network Error: Network Error: Error encountered in the status line:
由于连接方在一段时间后没有正确答复或连接的主机没有反应，连接尝试失败。 (os error 10060)
```

There is no English "timeout" in it. The consequence was measured against the
production ledger (`%LOCALAPPDATA%\cc-proxy\billing.db`):

| `errorTag`     | rows     |
| -------------- | -------- |
| `http-network` | **2611** |
| `http-timeout` | **0**    |

Every transport failure in the ledger's history had been filed as a network
error. `http-timeout` had never been written once. The classification existed
and never once worked — and it could not have been detected by reading the
code, only by comparing the tag distribution against the wording of a live
error.

A second problem sat underneath it: "upstream unreachable" was **one** bucket
for four different failures — a connect timeout, a wait for response headers
that never came, a refused socket, and a DNS failure. The 0.6.3 outage was the
header-timeout case, and the log line for it was indistinguishable from a
connection that was never established, which is precisely what made it slow to
diagnose.

## Decision

Classify from the error's **structure**, and split the classes that have
different causes.

1. **Read the type, not the text.** `ureq::Error::Transport` exposes
   `kind()`, and the real socket error sits down the `source()` chain. The
   classifier walks that chain for an `std::io::Error` and uses its
   `ErrorKind`. Both are locale-independent.
2. **Introduce `TransportFault`** in `upstream.rs`, carried on `UpstreamError`
   as `fault: Option<TransportFault>`, replacing the `status_code == 0` +
   message-parsing pairing. Variants: `ConnectTimeout`, `HeaderTimeout`,
   `ConnectRefused`, `Dns`, `Reset`, `ProxyFailed`, `Other`.
3. **One vocabulary for both surfaces.** `TransportFault::tag()` returns the
   ledger's `errorTag` (`transport-header-timeout`), and the log line leads
   with the same string in brackets (`[transport-header-timeout]`). A log line
   and a ledger row name the same class identically.
4. **Classify while the typed error still exists** — in `attempt_send`, at the
   `ureq::Error::Transport` arm — so no later layer has to reconstruct the
   class from a string. `classify_for_url` is exported so the model-catalog
   fetch uses the same vocabulary.
5. **Refused vs. timed-out connect is decided by whether a proxy is in use.**
   With an HTTP proxy the client resolves and connects only to the proxy, so a
   resolution failure or a refused socket is the proxy's fault. The route is
   the type `proxy::Route` (`Direct` / `ViaProxy`), resolved **per-URL** via
   `proxy::route_for(url)` — per-URL because the egress plan exempts loopback
   and `noProxy` hosts, so a global "are we proxied" flag would blame the proxy
   for faults it never carried. (A review pass also flagged the original
   `proxied: bool` as violating the repo's rule that arguments carry meaning
   through types; the enum is that correction.)

## What the probe established

The classes were chosen from a probe that drove `ureq` against real sockets and
printed each error's `kind()`, `Display` chain and `io::ErrorKind`, not from
reading the crate's source. Three findings shaped the outcome:

| Scenario                       | `ureq` `kind()`     | `io::ErrorKind` in chain | OS code |
| ------------------------------ | ------------------- | ------------------------ | ------- |
| Response headers never arrive  | `Io`                | `TimedOut`               | 10060   |
| Connect never accepted         | `ConnectionFailed`  | `TimedOut`               | —       |
| Refused, **no** connect timeout| `ConnectionFailed`  | `ConnectionRefused`      | 10061   |
| Refused, **with** timeout      | `ConnectionFailed`  | `TimedOut`               | 10060   |
| DNS failure                    | `Dns`               | `Uncategorized`          | 11001   |
| Body stalls after headers      | (reader-side)       | `TimedOut`               | 10060   |

Two consequences are recorded honestly rather than papered over:

- **A refused connect and a timed-out connect are not always separable.** With
  a connect timeout configured, `ureq` routes the dial through
  `connect_timeout` and the refusal surfaces as `connection timed out`. So
  `ConnectRefused` is only reachable when no connect timeout is set; otherwise
  the class is `ConnectTimeout`. `TransportFault::ConnectRefused` documents
  this. A sharper split would mean hand-rolling the connect (no ureq connect
  timeout, a deadline, and manual timing) — a larger change for a distinction a
  reader rarely needs.
- **A timed-out request write is not separable from a timed-out header read.**
  Both are `ErrorKind::Io` over `TimedOut` with no message. There is therefore
  **no `WriteTimeout` variant**: a second label would be a distinction the code
  cannot actually make. The rarer write case is folded into `HeaderTimeout`,
  and the enum says so.

## Consequences

- A reader can now tell the four failures apart by grepping one token. Verified
  end to end against a mock upstream: a 6 s header delay logs
  `CC upstream [transport-header-timeout], retrying 1/2...` and the reject line
  `[transport-header-timeout] upstream header timeout: …`; a dead port logs
  `[transport-refused]` — the same two failures that used to be one
  `http-network` row.
- **The ledger's tag vocabulary changes**, which is a breaking change for
  anything reading `errorTag` by value: `http-network` and the never-written
  `http-timeout` are replaced by the `transport-*` set. Historical rows keep
  their old tags; the WebUI displays the column verbatim and needs no mapping,
  so old and new rows coexist without a migration.
- **The client-visible error envelope also carries the tag.** This is a decided
  part of the change, not incidental scope: a transport failure reaches the
  client as `[transport-header-timeout] upstream header timeout: …`, and the
  non-streaming collapse path now sends `StreamFailure::tagged()` where it
  previously sent the untagged `detail()` — matching the streaming path, whose
  bracketed form (`[idle-timeout] …`) the behaviour fixtures have pinned all
  along. One wording now serves the log line, the ledger row and the client;
  the untagged form was an inconsistency, and no fixture pinned it.
- The classifier's **decision is a pure core, `fault_from(ureq_kind, io_kind,
  route, unclassified)`**, pinned by a table test that covers every branch —
  including the shapes a real socket cannot be made to produce reliably (a NAT
  gateway answering a connect with RST). `classify_transport` is a thin
  extractor, and the real-socket tests that remain are integration smoke pins
  for the classes whose ureq-side shape matters (`HeaderTimeout`,
  `ConnectRefused` without a connect timeout, `Dns`, `ProxyFailed`); the
  network-dependent `10.255.255.1` connect test was removed in favour of the
  table.
- `proxy::Route` + `route_for(url)` are the one addition to the egress module,
  so the classification can ask, per request, what route it took.
- The mid-body classifier (`StreamFailure`, tags `[idle-timeout]`,
  `[connection-reset]`, …) is **unchanged** in behaviour. It is *mostly*
  classified by `io::ErrorKind`, with three residual text matches for the
  wordings transport layers use for a dropped connection (`"terminated"`,
  `"socket hang up"`, `"Error while decoding chunks"`); that path is out of
  this ADR's scope and its tags are pinned by fixtures, so the residue is
  recorded here rather than "fixed" in the same change. The two vocabularies
  are deliberately distinct: one names a stall after output began, the other
  names a failure before any output existed.

## Alternatives considered

- **Match more English words** (add "connection timed out", "did not properly
  respond"). Rejected: it optimises the same broken approach, breaks again in
  the next locale, and still cannot split the four classes.
- **Read `raw_os_error` and map the numbers** (10060/10061/11001). Rejected as
  the primary signal because the codes are platform-specific; the structure
  already carries the answer. The codes remain useful in the raw text the
  message keeps, for a human confirming a diagnosis.
- **Hand-roll the connect to separate refused from timed-out.** Rejected: the
  split is real but the cost is a bespoke connection path, and no operator
  action differs between the two beyond "check the socket".
- **A `WriteTimeout` variant for completeness.** Rejected: unreachable by
  construction — see above.
