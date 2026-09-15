//! HTTP surface: routing, CORS preflight, the two-shape 404, error envelopes,
//! body limits and the request lifecycle log. Ported from src/server.ts.
//!
//! M1 covers the surface that does not need the upstream: /health, /v1/models,
//! the 404/401 paths, CORS and local validation rejections.

use crate::catalog::CatalogStore;
use crate::config::Config;
use crate::generate;
use crate::json_error;
use crate::log;
use crate::models::Catalog;
use crate::stream_body::{self, Dialect, SseBody};
use crate::translate;
use crate::upstream::UpstreamError;
use crate::usage::UsageTotals;
use crate::validation;
use serde_json::{json, Map, Value};
use std::io::Read;
use std::sync::{Arc, Mutex};
use tiny_http::{Header, Method, Request, Response, StatusCode};

pub const PROXY_VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct AppState {
    pub config: Config,
    /// Re-read per request: a model-discovery refresh replaces the catalog, and
    /// subsequent requests must see the new one (the Node version reads a
    /// module-level `current` the same way).
    pub catalog: CatalogStore,
    pub tables: crate::translate::ModelTables,
    /// `ANTHROPIC_DEFAULT_MODEL` / `CC_NO_TOOLS_GUARD`, read once at startup.
    pub translate_env: crate::translate::Env,
    pub usage: Mutex<UsageTotals>,
}

pub type SharedState = Arc<AppState>;

pub fn new_state(config: Config) -> SharedState {
    Arc::new(AppState {
        config,
        catalog: CatalogStore::new(),
        tables: crate::translate::ModelTables::load(),
        translate_env: crate::translate::Env::from_process(),
        usage: Mutex::new(UsageTotals::default()),
    })
}

/// CORS headers, unconditional `Vary: Origin` included — the golden pins the
/// exact set, and an empty CORS_ORIGIN only drops the allow-origin header.
fn cors_headers(state: &AppState, out: &mut Vec<Header>) {
    push_header(out, "Access-Control-Allow-Methods", "GET, POST, OPTIONS");
    push_header(
        out,
        "Access-Control-Allow-Headers",
        "Content-Type, Authorization, x-api-key",
    );
    push_header(out, "Vary", "Origin");
    if !state.config.cors_origin.is_empty() {
        push_header(
            out,
            "Access-Control-Allow-Origin",
            &state.config.cors_origin,
        );
    }
}

/// tiny_http has `with_header` (one at a time); fold a Vec into it.
trait HeadersExt {
    fn with_headers(self, headers: Vec<Header>) -> Self;
}

impl HeadersExt for Response<std::io::Cursor<Vec<u8>>> {
    fn with_headers(self, headers: Vec<Header>) -> Self {
        headers.into_iter().fold(self, |r, h| r.with_header(h))
    }
}

impl HeadersExt for Response<std::io::Empty> {
    fn with_headers(self, headers: Vec<Header>) -> Self {
        headers.into_iter().fold(self, |r, h| r.with_header(h))
    }
}

fn push_header(out: &mut Vec<Header>, name: &str, value: &str) {
    if let Ok(h) = Header::from_bytes(name.as_bytes(), value.as_bytes()) {
        out.push(h);
    }
}

fn header_value<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    req.headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}

fn is_anthropic(req: &Request) -> bool {
    header_value(req, "anthropic-version").is_some()
}

/// `Authorization: Bearer <key>` or `x-api-key: <key>`; passthrough only.
pub fn extract_api_key(req: &Request) -> Option<String> {
    if let Some(auth) = header_value(req, "authorization") {
        // Case-insensitive scheme match that keeps the original key bytes.
        let (scheme, rest) = auth.split_at(auth.len().min(7));
        if scheme.eq_ignore_ascii_case("bearer ") && !rest.trim().is_empty() {
            return Some(rest.to_string());
        }
    }
    header_value(req, "x-api-key")
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

pub fn get_or_create_request_id(req: &Request) -> String {
    if let Some(incoming) = header_value(req, "x-request-id") {
        let trimmed = incoming.trim();
        if !trimmed.is_empty() {
            return trimmed.chars().take(128).collect();
        }
    }
    uuid::Uuid::new_v4().to_string()
}

// ── response helpers ──────────────────────────────────────

fn respond_json(res: Request, status: u16, body: Value, state: &AppState, ctx: &RequestId) {
    if status >= 400 {
        let message = body
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        log_local_failure(ctx, status, &message, "json");
    }
    let payload = serde_json::to_string(&body).unwrap_or_else(|_| "{}".into());
    let mut headers = Vec::new();
    push_header(&mut headers, "Content-Type", "application/json");
    cors_headers(state, &mut headers);
    let _ = res.respond(
        Response::from_string(payload)
            .with_status_code(status)
            .with_headers(headers),
    );
}

fn respond_openai_error(
    res: Request,
    status: u16,
    message: &str,
    state: &AppState,
    ctx: &RequestId,
) {
    respond_json(res, status, json_error(message), state, ctx);
}

/// Anthropic error `type` for a status. `overloaded_error` is used for 529;
/// anything unmapped falls back to `api_error` (upstream 5xx never reaches
/// here — the upstream layer maps them to 502 first).
#[allow(dead_code)] // used by the upstream error mapping in M4
pub fn anthropic_error_type(status: u16) -> &'static str {
    match status {
        400 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        500 => "api_error",
        529 => "overloaded_error",
        _ => "api_error",
    }
}

fn respond_anthropic_error(
    res: Request,
    status: u16,
    error_type: &str,
    message: &str,
    state: &AppState,
    ctx: &RequestId,
) {
    if status >= 400 {
        log_local_failure(ctx, status, message, error_type);
    }
    let body = json!({"type": "error", "error": {"type": error_type, "message": message}});
    let payload = serde_json::to_string(&body).unwrap_or_else(|_| "{}".into());
    let mut headers = Vec::new();
    push_header(&mut headers, "Content-Type", "application/json");
    cors_headers(state, &mut headers);
    let _ = res.respond(
        Response::from_string(payload)
            .with_status_code(status)
            .with_headers(headers),
    );
}

/// 4xx is the client's problem, 5xx is ours — the distinction matters when
/// scanning the log for "did the proxy do something wrong".
fn log_local_failure(ctx: &RequestId, status: u16, message: &str, kind: &str) {
    let line = format!(
        "[reject] [{}] {} {} -> {} {}: {}",
        ctx.id, ctx.method, ctx.path, status, kind, message
    );
    if status >= 500 {
        log::error(&line);
    } else {
        log::warn(&line);
    }
}

/// Body-size and JSON-parse failures, mirroring BodyParseError.
pub struct BodyError {
    pub status: u16,
    pub message: String,
}

/// Read the body with the two-stage guard: a Content-Length pre-check that
/// rejects without reading, then a streaming cap. Never resets the connection —
/// the golden's 413 cases keep using the same keep-alive socket afterwards.
pub fn parse_body(req: &mut Request, max_bytes: u64) -> Result<Value, BodyError> {
    if let Some(len) = header_value(req, "content-length") {
        if let Ok(declared) = len.trim().parse::<f64>() {
            if declared.is_finite() && declared > max_bytes as f64 {
                drain(req);
                return Err(BodyError {
                    status: 413,
                    message: "Request body too large".into(),
                });
            }
        }
    }

    let mut raw = Vec::new();
    let mut limited = req.as_reader().take(max_bytes.saturating_add(1));
    let over = limited
        .read_to_end(&mut raw)
        .map(|_| raw.len() as u64 > max_bytes)
        .unwrap_or(false);
    if over {
        drain(req);
        return Err(BodyError {
            status: 413,
            message: "Request body too large".into(),
        });
    }
    if raw.is_empty() {
        return Ok(Value::Null);
    }
    let text = String::from_utf8_lossy(&raw);
    serde_json::from_str(&text).map_err(|_| BodyError {
        status: 400,
        message: "Invalid JSON body".into(),
    })
}

/// Consume whatever is left of the body so the connection stays usable.
fn drain(req: &mut Request) {
    let mut sink = Vec::new();
    let _ = req.as_reader().read_to_end(&mut sink);
}

// ── routing ───────────────────────────────────────────────

pub struct RequestId {
    pub id: String,
    pub method: String,
    pub path: String,
}

pub fn handle(state: &SharedState, req: Request) {
    let path = req.url().split('?').next().unwrap_or("/").to_string();
    let method = req.method().as_str().to_string();

    // OPTIONS is handled before the arrival line, matching the Node order
    // (preflight requests never produce request-log lines).
    if req.method() == &Method::Options {
        return handle_preflight(state, req, &path);
    }

    let ctx = RequestId {
        id: get_or_create_request_id(&req),
        method: method.clone(),
        path: path.clone(),
    };
    log::debug(&format!("[{}] <- {} {}", ctx.id, ctx.method, ctx.path));

    match (method.as_str(), path.as_str()) {
        ("GET", "/health") => handle_health(state, req, &ctx),
        ("GET", "/v1/models") => handle_models(state, req, &ctx),
        ("POST", "/v1/chat/completions") => handle_chat(state, req, &ctx),
        ("POST", "/v1/messages") => handle_messages(state, req, &ctx),
        ("POST", "/v1/messages/count_tokens") => handle_count_tokens(state, req, &ctx),
        _ => {
            if is_anthropic(&req) {
                respond_anthropic_error(req, 404, "not_found_error", "Not found", state, &ctx);
            } else {
                respond_json(req, 404, json!({"error": "Not found"}), state, &ctx);
            }
        }
    }
}

fn handle_preflight(state: &SharedState, req: Request, path: &str) {
    let known = matches!(
        path,
        "/health"
            | "/v1/models"
            | "/v1/chat/completions"
            | "/v1/messages"
            | "/v1/messages/count_tokens"
    );
    if !known {
        let ctx = RequestId {
            id: get_or_create_request_id(&req),
            method: "OPTIONS".into(),
            path: path.into(),
        };
        if is_anthropic(&req) {
            return respond_anthropic_error(req, 404, "not_found_error", "Not found", state, &ctx);
        }
        return respond_json(req, 404, json!({"error": "Not found"}), state, &ctx);
    }
    let mut headers = Vec::new();
    cors_headers(state, &mut headers);
    let _ = req.respond(Response::empty(StatusCode(204)).with_headers(headers));
}

fn handle_health(state: &SharedState, req: Request, ctx: &RequestId) {
    let cache = state
        .usage
        .lock()
        .map(|t| t.snapshot())
        .unwrap_or_else(|_| crate::usage::UsageTotals::default().snapshot());
    let body = json!({
        "status": "ok",
        "version": PROXY_VERSION,
        "cache": cache.to_json(),
    });
    respond_json(req, 200, body, state, ctx);
}

fn render_models(models: &Catalog, req: &Request) -> Value {
    let anthropic = is_anthropic(req);
    let body = if anthropic {
        let items: Vec<Value> = models
            .models
            .iter()
            .map(|m| {
                json!({
                    "id": m.id,
                    "type": "model",
                    "display_name": m.display_name,
                    "created_at": crate::now_iso8601(),
                    "max_input_tokens": null,
                    "max_tokens": null,
                    "capabilities": null,
                })
            })
            .collect();
        let first = models.models.first().map(|m| m.id.clone());
        let last = models.models.last().map(|m| m.id.clone());
        json!({
            "data": items,
            "has_more": false,
            "first_id": first,
            "last_id": last,
        })
    } else {
        let created = crate::now_epoch_secs();
        let items: Vec<Value> = models
            .ids
            .iter()
            .map(|id| {
                json!({
                    "id": id,
                    "object": "model",
                    "created": created,
                    "owned_by": "commandcode",
                })
            })
            .collect();
        json!({"object": "list", "data": items})
    };
    body
}

fn handle_models(state: &SharedState, req: Request, ctx: &RequestId) {
    let catalog = state.catalog.current();
    let body = render_models(&catalog, &req);
    respond_json(req, 200, body, state, ctx);
}

/// OpenAI `/v1/chat/completions`: validate, translate, generate.
fn handle_chat(state: &SharedState, mut req: Request, ctx: &RequestId) {
    let raw = match parse_body(&mut req, state.config.max_body_bytes) {
        Ok(v) => v,
        Err(e) => return respond_openai_error(req, e.status, &e.message, state, ctx),
    };
    if let Err(validation::ValidationError(msg)) = validation::validate_openai_chat_request(&raw) {
        return respond_openai_error(req, 400, &msg, state, ctx);
    }
    let Some(api_key) = extract_api_key(&req) else {
        return respond_openai_error(req, 401, "Unauthorized", state, ctx);
    };

    let requested = raw
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("default");
    let is_stream = raw.get("stream").and_then(Value::as_bool) == Some(true);
    // The encoder reports the name the client asked for, not the resolved id.
    let encoder_model = generate::encoder_model(requested, "default");

    log_incoming(ctx, &raw, requested);

    // One threadId per client request, shared by every upstream attempt so CC
    // bills one session per intent.
    let thread_id = uuid::Uuid::new_v4().to_string();
    let build_body = |catalog: &Catalog| {
        let mut body = translate::openai_to_cc(
            &raw,
            catalog,
            &state.tables,
            state.translate_env.no_tools_guard_off,
        );
        if let Some(obj) = body.as_object_mut() {
            obj.insert("threadId".into(), Value::String(thread_id.clone()));
        }
        body
    };

    let opts = upstream_options(state, &api_key);
    let stream = match generate::send_with_model_discovery(
        &state.catalog,
        &state.tables,
        &opts,
        requested,
        &build_body,
    ) {
        Ok(s) => s,
        Err(err) => return respond_upstream_error(req, &err, Dialect::Openai, state, ctx),
    };

    if is_stream {
        serve_sse(
            state,
            req,
            ctx,
            stream,
            Dialect::Openai,
            &encoder_model,
            &api_key,
            &build_body,
        );
    } else {
        let id = uuid::Uuid::new_v4().to_string();
        let mut stream = stream;
        match stream_body::collect_non_streaming(&mut stream, Dialect::Openai, &encoder_model, &id)
        {
            Ok(collected) => {
                record_usage(state, &collected.usage);
                respond_json(req, 200, collected.body, state, ctx);
            }
            Err(f) => {
                let err = UpstreamError {
                    message: f.detail(),
                    status_code: 0,
                    retryable: false,
                };
                respond_upstream_error(req, &err, Dialect::Openai, state, ctx);
            }
        }
    }
}

/// Anthropic `/v1/messages`: validate, translate, generate.
fn handle_messages(state: &SharedState, mut req: Request, ctx: &RequestId) {
    let raw = match parse_body(&mut req, state.config.max_body_bytes) {
        Ok(v) => v,
        Err(e) => {
            let kind = if e.status == 413 {
                "api_error"
            } else {
                "invalid_request_error"
            };
            return respond_anthropic_error(req, e.status, kind, &e.message, state, ctx);
        }
    };
    if let Err(validation::ValidationError(msg)) = validation::validate_anthropic_request(&raw) {
        return respond_anthropic_error(req, 400, "invalid_request_error", &msg, state, ctx);
    }
    let Some(api_key) = extract_api_key(&req) else {
        return respond_anthropic_error(
            req,
            401,
            "authentication_error",
            "Missing API key",
            state,
            ctx,
        );
    };

    let requested = raw.get("model").and_then(Value::as_str).unwrap_or("");
    let is_stream = raw.get("stream").and_then(Value::as_bool) == Some(true);
    let encoder_model = generate::encoder_model(requested, "");

    log::info(&format!("[{}] [Anthropic] Model: {requested}", ctx.id));

    let thread_id = uuid::Uuid::new_v4().to_string();
    let build_body = |catalog: &Catalog| {
        let mut body =
            translate::anthropic_to_cc_with_env(&raw, catalog, &state.tables, &state.translate_env);
        if let Some(obj) = body.as_object_mut() {
            obj.insert("threadId".into(), Value::String(thread_id.clone()));
        }
        body
    };

    let opts = upstream_options(state, &api_key);
    let stream = match generate::send_with_model_discovery(
        &state.catalog,
        &state.tables,
        &opts,
        requested,
        &build_body,
    ) {
        Ok(s) => s,
        Err(err) => return respond_upstream_error(req, &err, Dialect::Anthropic, state, ctx),
    };

    if is_stream {
        serve_sse(
            state,
            req,
            ctx,
            stream,
            Dialect::Anthropic,
            &encoder_model,
            &api_key,
            &build_body,
        );
    } else {
        // The message id is minted before the body is built: it is part of the
        // response the client reads and must be stable across the collapse.
        let id = format!("msg_{}", uuid::Uuid::new_v4());
        let mut stream = stream;
        match stream_body::collect_non_streaming(
            &mut stream,
            Dialect::Anthropic,
            &encoder_model,
            &id,
        ) {
            Ok(collected) => {
                record_usage(state, &collected.usage);
                respond_json(req, 200, collected.body, state, ctx);
            }
            Err(f) => {
                let err = UpstreamError {
                    message: f.detail(),
                    status_code: 0,
                    retryable: false,
                };
                respond_upstream_error(req, &err, Dialect::Anthropic, state, ctx);
            }
        }
    }
}

fn handle_count_tokens(state: &SharedState, mut req: Request, ctx: &RequestId) {
    let raw = match parse_body(&mut req, state.config.max_body_bytes) {
        Ok(v) => v,
        Err(e) => {
            let kind = if e.status == 413 {
                "api_error"
            } else {
                "invalid_request_error"
            };
            return respond_anthropic_error(req, e.status, kind, &e.message, state, ctx);
        }
    };
    if let Err(validation::ValidationError(msg)) = validation::validate_count_tokens_request(&raw) {
        return respond_anthropic_error(req, 400, "invalid_request_error", &msg, state, ctx);
    }
    // Local estimate: no key required, no upstream call (matches Node).
    let estimate = crate::usage::estimate_tokens(&raw);
    respond_json(req, 200, json!({"input_tokens": estimate}), state, ctx);
}

/// Parse a JSON request body into a map for the translation layer (M2+).
pub fn body_object(raw: &Value) -> Map<String, Value> {
    raw.as_object().cloned().unwrap_or_default()
}

// ── generation plumbing ───────────────────────────────────

fn upstream_options<'a>(state: &'a AppState, api_key: &'a str) -> generate::UpstreamOptions<'a> {
    generate::UpstreamOptions {
        api_base: &state.config.cc_api_base,
        api_key,
        cc_version: &state.config.cc_version,
        timeout_ms: state.config.upstream_timeout_ms,
        idle_timeout_ms: state.config.idle_timeout_ms,
    }
}

/// The per-request tool summary the Node handler logs. Two facts only — the
/// model and whether tools were offered — because that is what makes a
/// "the model ignored my tool" report diagnosable from the log alone.
fn log_incoming(ctx: &RequestId, raw: &Value, requested: &str) {
    let tools = raw.get("tools").and_then(Value::as_array);
    log::info(&format!(
        "[{}] [Incoming Request] Model: {requested}",
        ctx.id
    ));
    let count = tools.map_or(0, Vec::len);
    log::info(&format!(
        "[{}] [Incoming Request] Tools count: {count}",
        ctx.id
    ));
    if let Some(names) = tools.filter(|t| !t.is_empty()) {
        let list: Vec<&str> = names
            .iter()
            .filter_map(|t| t.pointer("/function/name").and_then(Value::as_str))
            .collect();
        log::info(&format!(
            "[{}] [Incoming Request] Tools list: {}",
            ctx.id,
            list.join(", ")
        ));
    } else {
        log::info(&format!(
            "[{}] [Incoming Request] No tools were sent by the client!",
            ctx.id
        ));
    }
}

fn record_usage(state: &AppState, usage: &Option<crate::usage::UsageData>) {
    let Some(usage) = usage else { return };
    // `recordUsage` counts only turns that actually reported a prompt token
    // count, so a stream with no usage does not inflate the request counter.
    let Some(prompt) = usage.prompt_tokens else {
        return;
    };
    let cached = usage.cached_tokens.unwrap_or(0);
    let completion = usage.completion_tokens.unwrap_or(0);
    let snapshot = match state.usage.lock() {
        Ok(mut totals) => {
            totals.requests = totals.requests.saturating_add(1);
            totals.prompt_tokens = totals.prompt_tokens.saturating_add(prompt);
            totals.cached_tokens = totals.cached_tokens.saturating_add(cached);
            totals.completion_tokens = totals.completion_tokens.saturating_add(completion);
            totals.snapshot()
        }
        Err(_) => return,
    };
    log::info(&format!(
        "[usage] {} 缓存 {}/{} tokens（{}%） 输出 {} | 累计 {}/{} tokens（{}%，{} 次请求）",
        "", // model is not part of the snapshot; the line stays count-only
        cached,
        prompt,
        pct(cached, prompt),
        completion,
        snapshot.cached_tokens,
        snapshot.prompt_tokens,
        snapshot.cache_rate,
        snapshot.requests,
    ));
}

fn pct(cached: u64, prompt: u64) -> f64 {
    if prompt == 0 {
        return 0.0;
    }
    ((cached as f64 / prompt as f64) * 1000.0).round() / 10.0
}

/// Stream the encoded records downstream as SSE.
///
/// The response is handed to tiny_http as a `Read`, so the body is pulled as
/// the socket accepts bytes and a slow client throttles the upstream instead of
/// being buffered. Nothing here can change the 200 once it is written, which is
/// why failures after this point are reported in-band by `SseBody`.
#[allow(clippy::too_many_arguments)]
fn serve_sse(
    state: &SharedState,
    req: Request,
    ctx: &RequestId,
    stream: crate::upstream::UpstreamStream,
    dialect: Dialect,
    model: &str,
    api_key: &str,
    build_body: &dyn Fn(&Catalog) -> Value,
) {
    // The recovery re-send repeats the request that just failed. It rebuilds
    // from the current catalog (which model discovery may have refreshed) but
    // reuses the pinned threadId, so a recovery stays on one CC session.
    let retry_state = Arc::clone(state);
    let retry_key = api_key.to_string();
    let retry_body = build_body(&retry_state.catalog.current());
    let reconnect = Box::new(move || {
        let opts = generate::UpstreamOptions {
            api_base: &retry_state.config.cc_api_base,
            api_key: &retry_key,
            cc_version: &retry_state.config.cc_version,
            timeout_ms: retry_state.config.upstream_timeout_ms,
            idle_timeout_ms: retry_state.config.idle_timeout_ms,
        };
        generate::resend(&retry_state.catalog, &opts, &|_catalog| retry_body.clone())
            .map_err(|e| crate::sse::StreamFailure::Other(e.message))
    });

    let usage_slot = SseBody::new_slot();
    let body = SseBody::new(
        stream,
        dialect,
        model,
        Some(reconnect),
        Arc::clone(&usage_slot),
    );

    let mut headers = Vec::new();
    push_header(&mut headers, "Content-Type", "text/event-stream");
    push_header(&mut headers, "Cache-Control", "no-cache");
    push_header(&mut headers, "Connection", "keep-alive");
    cors_headers(state, &mut headers);

    // tiny_http already picks chunked for a body with no known length; the
    // explicit threshold keeps that choice from depending on how many bytes
    // happen to be buffered when the response is written.
    let response =
        Response::new(StatusCode(200), headers, body, None, None).with_chunked_threshold(0);
    log::debug(&format!("[{}] -> 200 text/event-stream", ctx.id));
    // `respond` drives the body to EOF, at which point the usage the encoder
    // observed (if any) has been published. Recording after this point rather
    // than inside the body keeps the accounting off the write path.
    let _ = req.respond(response);
    let observed = usage_slot.lock().ok().and_then(|g| g.clone());
    record_usage(state, &observed);
}

/// Map an upstream failure to the downstream envelope for `dialect`.
fn respond_upstream_error(
    req: Request,
    err: &UpstreamError,
    dialect: Dialect,
    state: &AppState,
    ctx: &RequestId,
) {
    let status = err.downstream_status();
    match dialect {
        Dialect::Openai => respond_openai_error(req, status, &err.message, state, ctx),
        Dialect::Anthropic => {
            let kind = anthropic_error_type(status);
            respond_anthropic_error(req, status, kind, &err.message, state, ctx);
        }
    }
}

// ── accept loop ───────────────────────────────────────────

/// Max concurrent connection threads. Past the cap a connection is refused
/// rather than queued: the failure being defended against is a slow or
/// malicious client holding a thread forever (spec 5.5.1), not throughput.
const MAX_CONCURRENCY: usize = 64;
/// Blocked threads cost ~24 KB RSS measured; the 2 MB default is waste.
const THREAD_STACK_BYTES: usize = 512 * 1024;

pub fn serve(server: tiny_http::Server, state: SharedState) {
    let in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for request in server.incoming_requests() {
        let state = Arc::clone(&state);
        let in_flight = Arc::clone(&in_flight);
        let current = in_flight.load(std::sync::atomic::Ordering::Relaxed);
        if current >= MAX_CONCURRENCY {
            // Refuse rather than queue; the client sees a fast failure.
            let _ = request.respond(
                Response::from_string(
                    "{\"error\":{\"message\":\"Server busy\",\"type\":\"proxy_error\"}}",
                )
                .with_status_code(503),
            );
            continue;
        }
        in_flight.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let counter = Arc::clone(&in_flight);
        let spawned = std::thread::Builder::new()
            .stack_size(THREAD_STACK_BYTES)
            .spawn(move || {
                handle(&state, request);
                counter.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            });
        if spawned.is_err() {
            in_flight.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}
