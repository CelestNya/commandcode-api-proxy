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
use crate::upstream::{self, AttemptSink, UpstreamError, UpstreamStream};
use serde_json::Value;
use std::sync::Arc;

/// Where and how to reach CC, resolved from the proxy's config.
pub struct UpstreamOptions<'a> {
    pub api_base: &'a str,
    pub api_key: &'a str,
    pub cc_version: &'a str,
    pub timeout_ms: u64,
    pub idle_timeout_ms: u64,
    /// Records every upstream attempt this request makes, so an abandoned
    /// retry leaves a row instead of vanishing. `None` for callers that do not
    /// account usage.
    ///
    /// Held as an `Arc` rather than a borrow because the streaming-recovery
    /// closure outlives this call and must share the same ledger: the
    /// replacement request is another attempt of the same client request.
    pub attempts: Option<Arc<dyn AttemptSink>>,
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
        opts.attempts.as_ref(),
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
    // This dispatch was rejected before generating anything, but it did reach
    // CC, so it gets its own row with unknown usage.
    if let Some(sink) = opts.attempts.as_ref() {
        sink.failed("http-403-model-unknown");
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream_err(status: u16, message: &str) -> UpstreamError {
        UpstreamError {
            message: message.to_string(),
            status_code: status,
            retryable: false,
        }
    }

    #[test]
    fn only_a_403_naming_the_model_is_treated_as_discoverable() {
        assert!(is_unknown_model_error(&upstream_err(
            403,
            "CC API 403: {\"error\":{\"message\":\"Model/provider not recognized\"}}"
        )));
        // Case differences in the upstream's wording must not defeat this.
        assert!(is_unknown_model_error(&upstream_err(
            403,
            "CC API 403: Model/Provider Not Recognized"
        )));
    }

    #[test]
    fn unrelated_403s_do_not_trigger_a_catalog_refresh() {
        // A refresh cannot fix an auth or quota rejection, so retrying one
        // would only spend the user's quota a second time.
        for message in [
            "CC API 403: invalid api key",
            "CC API 403: quota exceeded",
            "CC API 403: {\"error\":{\"message\":\"forbidden\"}}",
        ] {
            assert!(
                !is_unknown_model_error(&upstream_err(403, message)),
                "{message} must not be treated as a discoverable model"
            );
        }
    }

    #[test]
    fn the_message_alone_is_not_enough_without_a_403() {
        // A 500 carrying the same wording is a server fault, not a resolution
        // failure: no catalog refresh can help.
        assert!(!is_unknown_model_error(&upstream_err(
            500,
            "Model/provider not recognized"
        )));
        assert!(!is_unknown_model_error(&upstream_err(
            0,
            "Upstream request failed: Model/provider not recognized"
        )));
    }

    #[test]
    fn discovery_is_skipped_for_a_name_that_already_resolves() {
        // `discover_model` short-circuits before touching the network, so a
        // full id or a known alias never costs a refresh. Passing an
        // unreachable api_base proves no request is attempted.
        let store = CatalogStore::new();
        let tables = ModelTables::load();
        assert!(!discover_model(
            &store,
            &tables,
            "deepseek/deepseek-v4-pro",
            "http://127.0.0.1:1",
            "k"
        ));
        assert!(!discover_model(
            &store,
            &tables,
            "deepseek-v4-pro",
            "http://127.0.0.1:1",
            "k"
        ));
        assert!(!discover_model(
            &store,
            &tables,
            "",
            "http://127.0.0.1:1",
            "k"
        ));
        assert!(!discover_model(
            &store,
            &tables,
            "default",
            "http://127.0.0.1:1",
            "k"
        ));
    }

    #[test]
    fn an_unknown_name_reports_unresolved_when_the_api_is_unreachable() {
        let store = CatalogStore::new();
        let tables = ModelTables::load();
        // The refresh fails, so the name stays unknown and the caller reports
        // the original 403 rather than retrying blindly.
        assert!(!discover_model(
            &store,
            &tables,
            "brand-new-model",
            "http://127.0.0.1:1",
            "k"
        ));
    }

    #[test]
    fn the_encoder_reports_the_requested_name_not_the_resolved_id() {
        // The client sees the name it asked for; the id it resolves to is an
        // upstream detail.
        assert_eq!(encoder_model("deepseek-flash", "default"), "deepseek-flash");
        // An OpenAI request with no model at all reports the literal "default".
        assert_eq!(encoder_model("", "default"), "default");
        // An Anthropic request always carries a model (validation requires it).
        assert_eq!(encoder_model("claude-opus-5", ""), "claude-opus-5");
    }
}
