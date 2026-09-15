//! One generation, end to end: translate the client request into a CC body,
//! send it upstream with model discovery, then either stream the encoded
//! records downstream or collapse them into a non-streaming response.
//!
//! The three attempts a request may make are all visible here and all reuse one
//! `threadId`:
//!
//! * the transport retries inside `upstream::send_to_cc` (429/5xx),
//! * the model-discovery retry below (403 "Model/provider not recognized"),
//! * the streaming-recovery retry in `SseBody` (a failure before any output).
//!
//! CC bills per session, so minting a second `threadId` for any of them would
//! charge the user twice for one intent.

use crate::catalog::CatalogStore;
use crate::log;
use crate::models::Catalog;
use crate::stream_body::Dialect;
use crate::translate::{self, ModelTables};
use crate::upstream::{self, UpstreamError, UpstreamStream};
use serde_json::Value;

/// Where and how to reach CC, resolved from the proxy's config.
pub struct UpstreamOptions<'a> {
    pub api_base: &'a str,
    pub api_key: &'a str,
    pub cc_version: &'a str,
    pub timeout_ms: u64,
    pub idle_timeout_ms: u64,
}

/// One transport-level send. Caller aborts are not modelled: the stop-loss for
/// a disconnected client is dropping the reader, which is what closes the
/// upstream socket. While the proxy is still waiting for the response headers
/// there is nothing to drop, so a disconnect is noticed only at the first
/// write — see `RUST-REWRITE-SPEC.md` on downstream give-up.
fn send_once(opts: &UpstreamOptions, body: Value) -> Result<UpstreamStream, UpstreamError> {
    upstream::send_to_cc(
        opts.api_base,
        opts.api_key,
        opts.cc_version,
        body,
        opts.timeout_ms,
        opts.idle_timeout_ms,
    )
}

/// Whether an upstream failure means "CC did not recognize this model name".
///
/// CC rewrites an unprefixed name to `anthropic:<name>` and answers 403 with
/// `Model/provider not recognized`. That is the one failure a catalog refresh
/// can fix, so it is the only case worth retrying — matching on the message
/// keeps unrelated 403s (auth, quota) from triggering a retry.
fn is_unknown_model_error(err: &UpstreamError) -> bool {
    err.status_code == 403
        && err
            .message
            .to_lowercase()
            .contains("model/provider not recognized")
}

/// Refresh the catalog if that is what is blocking `model`; report whether the
/// name became resolvable.
fn discover_model(
    store: &CatalogStore,
    tables: &ModelTables,
    model: &str,
    api_base: &str,
    api_key: &str,
) -> bool {
    if !translate::needs_catalog_discovery(model, &store.current(), tables) {
        return false;
    }
    // Refresh failures are non-fatal: the name simply stays unresolved.
    let refreshed = store.refresh(api_base, api_key);
    !translate::needs_catalog_discovery(model, &refreshed, tables)
}

/// Send a generation, retrying once if CC rejects the model name as unknown.
///
/// A bare name the catalog has not learned yet is forwarded verbatim, and CC
/// answers 403 on the invented `anthropic:` prefix even when the model does
/// exist upstream. On exactly that failure the catalog is refreshed and the
/// request rebuilt so the name resolves to its full id. The happy path costs
/// nothing extra: no fetch, no rebuild, no second attempt.
pub fn send_with_model_discovery(
    store: &CatalogStore,
    tables: &ModelTables,
    opts: &UpstreamOptions,
    model: &str,
    build_body: &dyn Fn(&Catalog) -> Value,
) -> Result<UpstreamStream, UpstreamError> {
    let first = send_once(opts, build_body(&store.current()));
    let err = match first {
        Ok(stream) => return Ok(stream),
        Err(err) => err,
    };
    if !is_unknown_model_error(&err) {
        return Err(err);
    }
    if !discover_model(store, tables, model, opts.api_base, opts.api_key) {
        // Still unknown → the model genuinely does not exist; report the
        // original failure rather than inventing a second one.
        return Err(err);
    }
    log::info(&format!(
        "Model catalog learned \"{model}\"; retrying with the resolved id"
    ));
    send_once(opts, build_body(&store.current()))
}

/// A re-send of the same generation for the streaming-recovery path.
///
/// Deliberately skips model discovery: the first attempt already resolved the
/// name, and this path exists to recover from a transport failure, not from a
/// rejection. `build_body` pins the threadId, so this stays on one CC session.
pub fn resend(
    store: &CatalogStore,
    opts: &UpstreamOptions,
    build_body: &dyn Fn(&Catalog) -> Value,
) -> Result<UpstreamStream, UpstreamError> {
    send_once(opts, build_body(&store.current()))
}

/// A generation-streaming failure as it reaches the client.
#[derive(Debug, Clone, PartialEq)]
pub enum GenerationError {
    /// CC reported failure in the stream itself (or produced a body that
    /// collapsed into a failure). The client is told
    /// "CC upstream generation failed".
    Failed,
    /// The transport died mid-stream, or the upstream contradicted itself.
    Transport(crate::sse::StreamFailure),
}

impl GenerationError {
    /// The message the downstream client sees. A transport failure keeps its
    /// own wording, so the client can tell a stall from a rejection.
    pub fn message(&self) -> String {
        match self {
            Self::Failed => "CC upstream generation failed".to_string(),
            Self::Transport(f) => f.detail(),
        }
    }
}

/// The dialect-specific model name an encoder reports downstream: the name the
/// client asked for, not the id it resolves to (matching the Node encoders).
pub fn encoder_model(requested: &str, fallback: &str) -> String {
    if requested.is_empty() {
        fallback.to_string()
    } else {
        requested.to_string()
    }
}

/// Which upstream-facing request builder a dialect needs.
pub fn requires_key(dialect: Dialect) -> bool {
    matches!(dialect, Dialect::Openai | Dialect::Anthropic)
}
