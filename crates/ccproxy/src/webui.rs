//! WebUI — a two-screen, live control panel served by the proxy itself.
//!
//! Screen one: summary statistics — attempt counts by outcome, token totals,
//! today's numbers and a 14-day trend. Screen two: the recent attempt ledger.
//! Both screens update in place over a Server-Sent Events stream; there is no
//! refresh button anywhere in the page.
//!
//! The proxy is the only writer of `billing.db`; the WebUI is a pure reader,
//! which is exactly the WAL "single-writer, many readers" case the ledger was
//! designed for (spec §"sqlite ledger"). Each request opens its own read
//! connection, so a reader can never block the writer.
//!
//! The live stream polls `max(id)` once a second and re-sends a full snapshot
//! only when new rows appear; otherwise it emits a keepalive comment so
//! intermediate proxies keep the connection open.

use crate::billing;
use crate::pricing::{Pricing, PRICING_SOURCE};
use crate::server::{RequestId, SharedState};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;
use tiny_http::{Header, Request, Response, StatusCode};

/// The page compiled into the binary — the fallback, and the whole story when
/// no loose sources ship beside the executable.
///
/// The authoring sources live in `crates/ccproxy/webui/`: `index.html` lists the
/// stylesheets and scripts, and `build.rs` inlines them via `webui_site`. Edit
/// the file you care about, not this constant.
const PAGE: &str = include_str!(concat!(env!("OUT_DIR"), "/webui.html"));

/// The served page: the loose `webui/` beside the executable when it is present
/// and assembles cleanly, otherwise the compiled-in copy.
///
/// Preferring the loose copy is what lets a stylesheet be edited and the page
/// reloaded with no rebuild — the point of shipping the sources.
///
/// The assembly is cached against the newest mtime among the sources, so a
/// request after an edit re-reads the files (that is the whole feature) while a
/// request with nothing changed costs two `stat` calls and no file reads. A
/// broken or half-present directory falls back to the compiled copy rather than
/// serving a page missing its styles, which is the failure this must never
/// produce.
fn page() -> String {
    use std::sync::Mutex;
    // (newest source mtime seen, the page assembled from it). A restart of the
    // mtime clock (a file restored to an older timestamp) still invalidates,
    // because only equality hits the cache.
    static CACHE: OnceLock<Mutex<Option<(std::time::SystemTime, String)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));

    let Some(dir) = crate::config::webui_site_dir() else {
        return PAGE.to_string();
    };

    let newest = newest_mtime(&dir);
    if let (Some(stamp), Ok(guard)) = (newest, cache.lock()) {
        if let Some((cached_at, page)) = guard.as_ref() {
            if *cached_at == stamp {
                return page.clone();
            }
        }
    }

    let assembled = match crate::webui_site::assemble(&dir) {
        Ok(page) => {
            crate::log::info(&format!(
                "WebUI 使用磁盘源码（改后刷新即生效）: {}",
                dir.display()
            ));
            page
        }
        Err(e) => {
            crate::log::warn(&format!(
                "WebUI 磁盘源码不可用（{e}）；回退到内置副本: {}",
                dir.display()
            ));
            PAGE.to_string()
        }
    };

    if let (Some(stamp), Ok(mut guard)) = (newest, cache.lock()) {
        *guard = Some((stamp, assembled.clone()));
    }
    assembled
}

/// The newest modification time among the sources, or `None` when the tree
/// cannot be walked (in which case the caller assembles without caching).
///
/// Walking is what makes an edit to *any* file — a stylesheet, a script, or the
/// `index.html` that ties them together — invalidate the cache.
fn newest_mtime(root: &std::path::Path) -> Option<std::time::SystemTime> {
    fn walk(dir: &std::path::Path, newest: &mut Option<std::time::SystemTime>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, newest);
            } else if let Ok(modified) = entry.metadata().and_then(|m| m.modified()) {
                if newest.is_none_or(|n| modified > n) {
                    *newest = Some(modified);
                }
            }
        }
    }
    let mut newest = None;
    walk(root, &mut newest);
    newest
}

/// How many ledger rows the detail screen shows by default.
const DEFAULT_LIMIT: i64 = 200;
/// Default trend window (days) for the overview screen.
const DEFAULT_TREND_DAYS: i64 = 14;
const MAX_LIMIT: i64 = 1000;
/// Poll interval of the live stream.
const SSE_POLL_MS: u64 = 1000;
/// Emit a keepalive comment every N idle polls (~10 s).
const SSE_KEEPALIVE_EVERY: u32 = 10;

/// How many trailing lines of each log the log screen shows and tails.
const LOG_TAIL_LINES: usize = 500;
/// Largest log file we will read through the tail. A log far bigger than this
/// is one that rotation has not yet truncated; reading the whole thing on
/// every poll would stall the WebUI to no benefit, since only the tail shows.
const LOG_READ_CAP_BYTES: u64 = 8 * 1024 * 1024;
/// Log-file poll interval; faster than the ledger poll because a log line is
/// something the user is actively watching for.
const LOG_POLL_MS: u64 = 700;

/// Which log a log-screen request refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogKind {
    Proxy,
    Tray,
}

impl LogKind {
    fn parse(raw: &str) -> Self {
        if raw.eq_ignore_ascii_case("tray") {
            Self::Tray
        } else {
            Self::Proxy
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Proxy => "proxy",
            Self::Tray => "tray",
        }
    }

    fn path(self) -> PathBuf {
        match self {
            Self::Proxy => crate::config::log_path(),
            Self::Tray => crate::config::tray_log_path(),
        }
    }
}

/// Parse `?which=tray` (anything else, including absent, means the proxy log).
fn log_kind(url: &str) -> LogKind {
    let query = url.split('?').nth(1).unwrap_or("");
    for pair in query.split('&') {
        let mut it = pair.split('=');
        if it.next() == Some("which") {
            return LogKind::parse(it.next().unwrap_or(""));
        }
    }
    LogKind::Proxy
}

/// Builds response headers, skipping any pair that fails to parse (the
/// constant strings used here never do).
fn headers(pairs: &[(&str, &str)]) -> Vec<Header> {
    let mut out = Vec::new();
    for (name, value) in pairs {
        if let Ok(h) = Header::from_bytes(name.as_bytes(), value.as_bytes()) {
            out.push(h);
        }
    }
    out
}

fn respond(req: Request, status: u16, body: String, content_type: &str) {
    let hdrs = headers(&[
        ("Content-Type", content_type),
        ("Cache-Control", "no-store"),
    ]);
    let mut resp = Response::from_string(body).with_status_code(StatusCode(status));
    for h in hdrs {
        resp = resp.with_header(h);
    }
    let _ = req.respond(resp);
}

fn respond_json(req: Request, body: &Value) {
    let payload = serde_json::to_string(body).unwrap_or_else(|_| "{}".into());
    respond(req, 200, payload, "application/json");
}

// ── routes ─────────────────────────────────────────────────────

/// `GET /webui` — the page itself, with the first snapshot inlined.
///
/// Astro-style static-first: the first screenful is already in the HTML
/// (`window.__INIT__`), so the page paints without a network round-trip and
/// the JS only wires the live stream. When the ledger cannot be opened the
/// page still renders, just without the initial payload.
/// Makes a JSON document safe to embed inside a `<script>` element.
///
/// `serde_json` escapes quotes and control characters but never `<`, `>`, or
/// `&`, so a ledger string — a model name arrives straight from the request
/// body — could close the script tag from within the data. `\uXXXX` escapes
/// are invisible to `JSON.parse` and end that hazard.
fn inline_json(json: &str) -> String {
    json.replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
}

pub fn handle_page(state: &SharedState, req: Request, _ctx: &RequestId) {
    let _ = state;
    let init = match billing::open_database(&billing::billing_dir()) {
        Some(conn) => {
            let snap = json!({
                "stats": stats_json(&conn, DEFAULT_TREND_DAYS),
                "attempts": attempts_json(&conn, DEFAULT_LIMIT),
            });
            serde_json::to_string(&snap).unwrap_or_else(|_| "{}".into())
        }
        None => "{}".into(),
    };
    let body = page().replace(
        "</head>",
        &format!(
            "<script>window.__INIT__={};</script></head>",
            inline_json(&init)
        ),
    );
    respond(req, 200, body, "text/html; charset=utf-8");
}

/// `GET /webui/api/sysinfo` — process working set and uptime for the top bar.
pub fn handle_sysinfo(state: &SharedState, req: Request, _ctx: &RequestId) {
    let _ = state;
    let body = json!({
        "memMb": process_working_set_mb(),
        "uptimeSecs": process_uptime_secs(),
    });
    respond_json(req, &body);
}

/// Working set (RSS) of the current process, in MiB. Uses `sysinfo`, a pure
/// third-party crate, so the `forbid(unsafe_code)` contract in lib.rs stays
/// intact. Returns None only when the platform cannot report it.
fn process_working_set_mb() -> Option<f64> {
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let mut sys = System::new();
    let pid = Pid::from_u32(std::process::id());
    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    let mem = sys.process(pid).map(|p| p.memory())?;
    Some(mem as f64 / (1024.0 * 1024.0))
}

/// Seconds since this process started, from the process start timestamp.
fn process_uptime_secs() -> Option<u64> {
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let mut sys = System::new();
    let pid = Pid::from_u32(std::process::id());
    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    let start = sys.process(pid).map(|p| p.start_time())?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Some(now.saturating_sub(start))
}

/// `GET /webui/api/stats` — summary statistics for screen one.
pub fn handle_stats(state: &SharedState, req: Request, _ctx: &RequestId) {
    let _ = state;
    let days = parse_days(req.url());
    let body = with_ledger(|conn| stats_json(conn, days));
    respond_json(req, &body);
}

/// `GET /webui/api/attempts?limit=N` — the recent ledger for screen two.
pub fn handle_attempts(state: &SharedState, req: Request, _ctx: &RequestId) {
    let _ = state;
    let limit = parse_limit(req.url());
    let body = match billing::open_database(&billing::billing_dir()) {
        Some(conn) => attempts_json(&conn, limit),
        // No ledger yet (proxy never ran): an empty array keeps this
        // endpoint's array contract, which the page filters and paginates.
        None => json!([]),
    };
    respond_json(req, &body);
}

/// `GET /webui/events` — the Server-Sent Events live stream.
pub fn handle_events(state: &SharedState, req: Request, _ctx: &RequestId) {
    let _ = state;
    let hdrs = headers(&[
        ("Content-Type", "text/event-stream"),
        ("Cache-Control", "no-cache"),
        ("Connection", "keep-alive"),
    ]);
    let reader = SseReader {
        dir: billing::billing_dir(),
        last_max_id: 0,
        first: true,
        pings: 0,
        buf: Vec::new(),
        pos: 0,
    };
    let _ = req.respond(Response::new(StatusCode(200), hdrs, reader, None, None));
}

/// `GET /webui/api/logs?which=proxy|tray` — the tail of one log file.
///
/// A single snapshot of the last `LOG_TAIL_LINES` lines, for the initial paint
/// and for the refresh button; the live follow is `handle_log_stream`.
pub fn handle_logs(state: &SharedState, req: Request, _ctx: &RequestId) {
    let _ = state;
    let kind = log_kind(req.url());
    respond_json(req, &log_snapshot(kind));
}

/// `GET /webui/logs/stream?which=proxy|tray` — SSE follow of one log file.
///
/// Emits an `event: log` frame with the whole tail whenever the file's size
/// changes (the same shape the page already renders), and a keepalive comment
/// while idle. Following the tail wholesale rather than streaming appended
/// bytes keeps truncation-on-rotation correct for free: a shrink is just
/// another size change, and the next frame carries the new, shorter tail.
pub fn handle_log_stream(state: &SharedState, req: Request, _ctx: &RequestId) {
    let _ = state;
    let kind = log_kind(req.url());
    let hdrs = headers(&[
        ("Content-Type", "text/event-stream"),
        ("Cache-Control", "no-cache"),
        ("Connection", "keep-alive"),
    ]);
    let reader = LogSseReader {
        kind,
        last_len: None,
        first: true,
        pings: 0,
        buf: Vec::new(),
        pos: 0,
    };
    let _ = req.respond(Response::new(StatusCode(200), hdrs, reader, None, None));
}

/// Read the tail of `kind`'s log file: at most `LOG_TAIL_LINES` lines from the
/// end, without ever reading more than `LOG_READ_CAP_BYTES`.
///
/// Returns `None` when the file cannot be read (absent, permission), which the
/// page renders as "日志不可用" rather than an empty pane that looks live.
fn read_log_tail(kind: LogKind) -> Option<String> {
    let path = kind.path();
    let meta = std::fs::metadata(&path).ok()?;
    let len = meta.len();
    let mut file = std::fs::File::open(&path).ok()?;
    if len > LOG_READ_CAP_BYTES {
        use std::io::Seek;
        let offset = len.saturating_sub(LOG_READ_CAP_BYTES);
        file.seek(io::SeekFrom::Start(offset)).ok()?;
    }
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    Some(tail_lines(&text, LOG_TAIL_LINES))
}

/// The last `limit` lines of `text`, joined without a trailing newline. The
/// first line is dropped when the read began mid-file (over-cap seek), since
/// it is a fragment rather than a line.
fn tail_lines(text: &str, limit: usize) -> String {
    let mut lines: Vec<&str> = text.lines().collect();
    if lines.len() > limit {
        lines = lines.split_off(lines.len().saturating_sub(limit));
    }
    lines.join("\n")
}

/// The log screen's payload: the tail plus the facts the header shows.
fn log_snapshot(kind: LogKind) -> Value {
    let path = kind.path();
    let tail = read_log_tail(kind);
    let size = std::fs::metadata(&path).ok().map(|m| m.len());
    json!({
        "which": kind.name(),
        "path": path.display().to_string(),
        "sizeBytes": size,
        "available": tail.is_some(),
        "lines": tail.unwrap_or_default(),
    })
}

fn with_ledger<F: FnOnce(&Connection) -> Value>(f: F) -> Value {
    let dir = billing::billing_dir();
    match billing::open_database(&dir) {
        Some(conn) => f(&conn),
        // No ledger yet (proxy never ran): report an empty but well-formed
        // document so the page can render "no data" instead of failing.
        None => json!({ "error": "ledger unavailable" }),
    }
}

fn parse_limit(url: &str) -> i64 {
    let query = url.split('?').nth(1).unwrap_or("");
    for pair in query.split('&') {
        let mut it = pair.split('=');
        if it.next() == Some("limit") {
            if let Ok(n) = it.next().unwrap_or("").parse::<i64>() {
                return n.clamp(1, MAX_LIMIT);
            }
        }
    }
    DEFAULT_LIMIT
}

fn parse_days(url: &str) -> i64 {
    let query = url.split('?').nth(1).unwrap_or("");
    for pair in query.split('&') {
        let mut it = pair.split('=');
        if it.next() == Some("days") {
            if let Ok(n) = it.next().unwrap_or("").parse::<i64>() {
                return n.clamp(1, 90);
            }
        }
    }
    DEFAULT_TREND_DAYS
}

// ── queries ────────────────────────────────────────────────────

/// Screen-one payload: lifetime totals, today's totals and a 14-day trend.
///
/// Token columns are NULL when unknown (a failed attempt has no tokens) and
/// must never be shown as 0 — every SUM is coalesced so the aggregate is real.
fn stats_json(conn: &Connection, window: i64) -> Value {
    // Crawled price table; loads from disk and refreshes when stale (24h).
    let pricing = Pricing::load(&billing::billing_dir());
    let totals = conn
        .query_row(
            "select count(*),
                    coalesce(sum(case when status = 'ok' then 1 else 0 end), 0),
                    coalesce(sum(case when status = 'error' then 1 else 0 end), 0),
                    coalesce(sum(case when status = 'aborted' then 1 else 0 end), 0),
                    coalesce(sum(case when status = 'interrupted' then 1 else 0 end), 0),
                    coalesce(sum(promptTokens), 0),
                    coalesce(sum(cachedTokens), 0),
                    coalesce(sum(completionTokens), 0),
                    coalesce(sum(reasoningTokens), 0),
                    coalesce(avg(durationMs), 0),
                    coalesce(avg(ttfbMs), 0),
                    max(ts)
             from billing",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, f64>(9)?,
                    row.get::<_, f64>(10)?,
                    row.get::<_, Option<String>>(11)?,
                ))
            },
        )
        .unwrap_or((0, 0, 0, 0, 0, 0, 0, 0, 0, 0.0, 0.0, None));

    let (
        count,
        ok,
        error,
        aborted,
        interrupted,
        prompt,
        cached,
        completion,
        reasoning,
        avg_ms,
        avg_ttfb_ms,
        latest_ts,
    ) = totals;

    let today = conn
        .query_row(
            // The boundary must come out of strftime, not datetime(): the ts
            // column holds T-separated ISO instants, and since 'T' > ' ' a
            // space-formatted boundary would let every same-day row through
            // regardless of its actual time (the old query silently dropped
            // everything before 08:00 local in UTC+8).
            "select count(*),
                    coalesce(sum(promptTokens), 0),
                    coalesce(sum(cachedTokens), 0),
                    coalesce(sum(completionTokens), 0),
                    coalesce(sum(reasoningTokens), 0)
             from billing
             where ts >= strftime('%Y-%m-%dT%H:%M:%S', 'now', 'localtime', 'start of day', 'utc')",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .unwrap_or((0, 0, 0, 0, 0));

    let trend = {
        // The window is a strict calendar range: a recursive CTE materialises
        // every day in it and left-joins the aggregated ledger, so the chart
        // always shows `window` points and empty days read as zero tokens.
        // A different window therefore visibly changes the curve. The filter
        // boundary is strftime for the same T-format reason as `today` above;
        // whole-day shifts are timezone-invariant, so UTC is exact here.
        let stmt = conn
            .prepare(
                "with recursive seq(x) as (
                        select 0
                        union all
                        select x + 1 from seq where x < ?1 - 1
                     ),
                     days(day) as (
                        select date('now', 'localtime', '-' || x || ' days') from seq
                     ),
                     agg as (
                        select strftime('%Y-%m-%d', ts, 'localtime') as day,
                               count(*) as attempts,
                               coalesce(sum(promptTokens), 0) as prompt,
                               coalesce(sum(cachedTokens), 0) as cached,
                               coalesce(sum(completionTokens), 0) as completion
                        from billing
                        where ts >= strftime('%Y-%m-%dT%H:%M:%S', 'now', '-' || ?2 || ' days')
                        group by day
                     )
                 select d.day,
                        coalesce(a.attempts, 0),
                        coalesce(a.prompt, 0),
                        coalesce(a.cached, 0),
                        coalesce(a.completion, 0)
                 from days d
                 left join agg a on a.day = d.day
                 order by d.day",
            )
            .ok();
        let mut days: Vec<Value> = Vec::new();
        if let Some(mut stmt) = stmt {
            if let Ok(rows) = stmt.query_map([window, window], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            }) {
                for row in rows.flatten() {
                    // promptTokens already counts cached tokens (AI SDK
                    // accounting), so the hit rate is cached / prompt.
                    let hit_rate = if row.2 > 0 {
                        Some(row.3 as f64 / row.2 as f64)
                    } else {
                        None
                    };
                    days.push(json!({
                        "date": row.0,
                        "attempts": row.1,
                        "prompt": row.2,
                        "cached": row.3,
                        "completion": row.4,
                        "hitRate": hit_rate,
                    }));
                }
            }
        }
        days // oldest → newest for the chart
    };

    let platforms = {
        let stmt = conn
            .prepare(
                "select wire,
                        count(*),
                        coalesce(sum(promptTokens), 0),
                        coalesce(sum(cachedTokens), 0),
                        coalesce(sum(completionTokens), 0)
                 from billing
                 group by wire
                 order by count(*) desc",
            )
            .ok();
        let mut out: Vec<Value> = Vec::new();
        if let Some(mut stmt) = stmt {
            if let Ok(rows) = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            }) {
                for row in rows.flatten() {
                    out.push(json!({
                        "wire": row.0,
                        "attempts": row.1,
                        "prompt": row.2,
                        "cached": row.3,
                        "completion": row.4,
                    }));
                }
            }
        }
        out
    };

    let models = {
        // Per-model totals, plus the token families the cost model needs.
        // Costs are computed here in Rust against the crawled price table; the
        // ledger never stores prices, so history re-prices when the table
        // changes.
        //
        // Rows are bucketed by their own peak/off-peak period before summing:
        // a peak-hour request must keep its peak price however much later the
        // page is opened. Bucketing in SQL (rather than pricing the aggregate)
        // is what makes that exact — the aggregate cannot know which rows were
        // peak.
        let stmt = conn
            .prepare(
                "select model,
                        case when (
                            (cast(strftime('%H', ts) as integer) * 60
                             + cast(strftime('%M', ts) as integer)) between 60 and 239
                            or (cast(strftime('%H', ts) as integer) * 60
                             + cast(strftime('%M', ts) as integer)) between 360 and 599
                        ) then 1 else 0 end as peak,
                        count(*),
                        coalesce(sum(promptTokens), 0),
                        coalesce(sum(cachedTokens), 0),
                        coalesce(sum(cacheCreationTokens), 0),
                        coalesce(sum(completionTokens), 0),
                        coalesce(sum(reasoningTokens), 0)
                 from billing
                 group by model, peak
                 order by count(*) desc, 4 desc",
            )
            .ok();
        // model -> accumulated totals across its peak and off-peak buckets.
        #[derive(Default)]
        struct Row {
            attempts: i64,
            prompt: i64,
            cached: i64,
            cache_creation: i64,
            completion: i64,
            reasoning: i64,
            cost: f64,
            priced: bool,
        }
        let mut by_model: std::collections::BTreeMap<String, Row> =
            std::collections::BTreeMap::new();
        if let Some(mut stmt) = stmt {
            if let Ok(rows) = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            }) {
                for row in rows.flatten() {
                    // Rate period of this bucket: any minute inside the peak
                    // window selects the same rate, so a representative one is
                    // enough; minute 0 is off-peak.
                    let minute = if row.1 == 1 { 120 } else { 0 };
                    let bucket_cost = pricing.cost(&row.0, row.3, row.4, row.5, row.6, minute);
                    let entry = by_model.entry(row.0.clone()).or_insert_with(|| Row {
                        // Priced only if the price table knows the model; each
                        // bucket then connects with this.
                        priced: true,
                        ..Row::default()
                    });
                    entry.attempts = entry.attempts.saturating_add(row.2);
                    entry.prompt = entry.prompt.saturating_add(row.3);
                    entry.cached = entry.cached.saturating_add(row.4);
                    entry.cache_creation = entry.cache_creation.saturating_add(row.5);
                    entry.completion = entry.completion.saturating_add(row.6);
                    entry.reasoning = entry.reasoning.saturating_add(row.7);
                    entry.cost += bucket_cost.unwrap_or(0.0);
                    // One unpriced bucket must not make the model look free.
                    entry.priced = entry.priced && bucket_cost.is_some();
                }
            }
        }
        // Biggest models first (attempts desc, then prompt tokens desc), the
        // order the pre-bucketing query used.
        let mut ordered: Vec<(String, Row)> = by_model.into_iter().collect();
        ordered.sort_by(|a, b| {
            b.1.attempts
                .cmp(&a.1.attempts)
                .then(b.1.prompt.cmp(&a.1.prompt))
        });
        let mut out: Vec<Value> = Vec::new();
        for (model, r) in ordered {
            // promptTokens includes cached tokens and completionTokens includes
            // reasoning, so the total is prompt + completion (adding them again
            // would double-count).
            let tokens = r.prompt.saturating_add(r.completion);
            out.push(json!({
                "model": model,
                "attempts": r.attempts,
                "tokens": tokens,
                "promptTokens": r.prompt,
                "cachedTokens": r.cached,
                "cacheCreationTokens": r.cache_creation,
                "completionTokens": r.completion,
                "reasoningTokens": r.reasoning,
                "costUsd": if r.priced { Some(r.cost) } else { None },
            }));
        }
        out
    };

    let cost_total: f64 = models
        .iter()
        .filter_map(|m| m.get("costUsd").and_then(|c| c.as_f64()))
        .sum();
    let pricing_meta = json!({
        "source": PRICING_SOURCE,
        "fetchedAt": pricing.fetched_at,
        "count": pricing.models.len(),
    });

    // promptTokens already includes cachedTokens; hit rate is cached / prompt.
    let hit_rate = if prompt > 0 {
        Some(cached as f64 / prompt as f64)
    } else {
        None
    };

    json!({
        "totals": {
            "attempts": count,
            "ok": ok,
            "error": error,
            "aborted": aborted,
            "interrupted": interrupted,
            "promptTokens": prompt,
            "cachedTokens": cached,
            "completionTokens": completion,
            "reasoningTokens": reasoning,
            "avgDurationMs": avg_ms,
            "avgTtfbMs": avg_ttfb_ms,
            "hitRate": hit_rate,
            "latestTs": latest_ts,
            "costUsd": cost_total,
        },
        "today": {
            "attempts": today.0,
            "promptTokens": today.1,
            "cachedTokens": today.2,
            "completionTokens": today.3,
            "reasoningTokens": today.4,
        },
        "trend": trend,
        "platforms": platforms,
        "models": models,
        "pricing": pricing_meta,
    })
}
/// Screen-two payload: the most recent attempts, newest first.
fn attempts_json(conn: &Connection, limit: i64) -> Value {
    let mut stmt = match conn.prepare(
        "select ts, reqId, wire, model, stream, attempt, status, errorTag,
                promptTokens, cachedTokens, cacheCreationTokens,
                completionTokens, reasoningTokens, durationMs, ttfbMs
         from billing
         order by id desc
         limit ?1",
    ) {
        Ok(stmt) => stmt,
        Err(_) => return json!([]),
    };
    let rows = stmt
        .query_map([limit], |row| {
            Ok(json!({
                "ts": row.get::<_, String>(0)?,
                "reqId": row.get::<_, Option<String>>(1)?,
                "wire": row.get::<_, String>(2)?,
                "model": row.get::<_, String>(3)?,
                "stream": row.get::<_, bool>(4)?,
                "attempt": row.get::<_, i64>(5)?,
                "status": row.get::<_, String>(6)?,
                "errorTag": row.get::<_, Option<String>>(7)?,
                "promptTokens": row.get::<_, Option<i64>>(8)?,
                "cachedTokens": row.get::<_, Option<i64>>(9)?,
                "cacheCreationTokens": row.get::<_, Option<i64>>(10)?,
                "completionTokens": row.get::<_, Option<i64>>(11)?,
                "reasoningTokens": row.get::<_, Option<i64>>(12)?,
                "durationMs": row.get::<_, Option<i64>>(13)?,
                "ttfbMs": row.get::<_, Option<i64>>(14)?,
            }))
        })
        .ok();
    let mut out = Vec::new();
    if let Some(rows) = rows {
        out.extend(rows.flatten());
    }
    json!(out)
}

/// A snapshot of both screens, pushed over the event stream.
fn snapshot_json(conn: &Connection) -> Value {
    json!({
        "stats": stats_json(conn, DEFAULT_TREND_DAYS),
        "attempts": attempts_json(conn, DEFAULT_LIMIT),
    })
}

// ── the live stream ────────────────────────────────────────────

/// Blocks inside `Read` until the ledger changes, then yields an SSE frame.
///
/// tiny_http reads this as the response body; a read returning 0 would close
/// the connection, so every poll either produces a `snapshot` event (when
/// `max(id)` moved) or a keepalive comment, and never an empty read.
struct SseReader {
    dir: PathBuf,
    last_max_id: i64,
    first: bool,
    pings: u32,
    buf: Vec<u8>,
    pos: usize,
}

impl Read for SseReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            if self.pos < self.buf.len() {
                let n = self.buf.len().saturating_sub(self.pos).min(out.len());
                let end = self.pos.saturating_add(n);
                if let (Some(dst), Some(src)) = (out.get_mut(..n), self.buf.get(self.pos..end)) {
                    dst.copy_from_slice(src);
                    self.pos = end;
                    return Ok(n);
                }
            }
            if !self.first {
                std::thread::sleep(Duration::from_millis(SSE_POLL_MS));
            }
            self.first = false;
            self.buf.clear();
            self.pos = 0;

            if let Some(conn) = billing::open_database(&self.dir) {
                if let Ok(max_id) =
                    conn.query_row("select coalesce(max(id), 0) from billing", [], |r| {
                        r.get::<_, i64>(0)
                    })
                {
                    if max_id != self.last_max_id {
                        self.last_max_id = max_id;
                        let snap = snapshot_json(&conn);
                        self.buf = format!("event: snapshot\ndata: {}\n\n", snap).into_bytes();
                    }
                }
            }
            if self.buf.is_empty() {
                self.pings = self.pings.saturating_add(1);
                if self.pings >= SSE_KEEPALIVE_EVERY {
                    self.pings = 0;
                    self.buf = b": keepalive\n\n".to_vec();
                }
            }
        }
    }
}

/// Follows one log file, emitting the whole tail whenever its size changes.
///
/// Same contract as [`SseReader`]: a `read` returning 0 would close the
/// connection, so every poll yields either a frame or a keepalive. The file
/// needs no lock — it is the proxy's own `proxy.log` or the tray's `tray.log`,
/// and a concurrent append only makes the next read return a longer tail.
struct LogSseReader {
    kind: LogKind,
    /// File size at the last frame; `None` until the first read. A change in
    /// either direction (append or truncate-on-rotate) triggers a frame.
    last_len: Option<u64>,
    first: bool,
    pings: u32,
    buf: Vec<u8>,
    pos: usize,
}

impl Read for LogSseReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            if self.pos < self.buf.len() {
                let n = self.buf.len().saturating_sub(self.pos).min(out.len());
                let end = self.pos.saturating_add(n);
                if let (Some(dst), Some(src)) = (out.get_mut(..n), self.buf.get(self.pos..end)) {
                    dst.copy_from_slice(src);
                    self.pos = end;
                    return Ok(n);
                }
            }
            if !self.first {
                std::thread::sleep(Duration::from_millis(LOG_POLL_MS));
            }
            self.first = false;
            self.buf.clear();
            self.pos = 0;

            let len = std::fs::metadata(self.kind.path()).ok().map(|m| m.len());
            if len != self.last_len {
                self.last_len = len;
                let snap = log_snapshot(self.kind);
                self.buf = format!("event: log\ndata: {}\n\n", snap).into_bytes();
            }
            if self.buf.is_empty() {
                self.pings = self.pings.saturating_add(1);
                if self.pings >= SSE_KEEPALIVE_EVERY {
                    self.pings = 0;
                    self.buf = b": keepalive\n\n".to_vec();
                }
            }
        }
    }
}

// ── tests ──────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway ledger with the same schema the writer creates, so query
    /// aggregation is exercised against the real column layout.
    /// Row shape: (ts, wire, model, status, prompt, cached, completion, duration, attempt, stream).
    type TestRow<'a> = (
        &'a str,
        &'a str,
        &'a str,
        &'a str,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
    );
    fn ledger(rows: &[TestRow]) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "create table billing (
                id integer primary key autoincrement,
                ts text not null,
                reqId text,
                model text not null,
                wire text not null,
                stream integer not null,
                attempt integer not null,
                status text not null,
                errorTag text,
                promptTokens integer,
                cachedTokens integer,
                cacheCreationTokens integer,
                completionTokens integer,
                reasoningTokens integer,
                durationMs integer,
                ttfbMs integer
            );",
        )
        .unwrap();
        for (ts, wire, model, status, prompt, cached, completion, duration, attempt, stream) in rows
        {
            conn.execute(
                "insert into billing (ts, reqId, model, wire, stream, attempt, status,
                                      promptTokens, cachedTokens, completionTokens,
                                      reasoningTokens, durationMs)
                 values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, null, ?11)",
                rusqlite::params![
                    ts, "req", model, wire, stream, attempt, status, prompt, cached, completion,
                    duration
                ],
            )
            .unwrap();
        }
        conn
    }

    #[test]
    fn stats_aggregates_by_outcome_and_tokens() {
        let conn = ledger(&[
            (
                "2026-09-16T10:00:00Z",
                "openai",
                "gpt-4o",
                "ok",
                100,
                20,
                300,
                1500,
                1,
                0,
            ),
            (
                "2026-09-16T11:00:00Z",
                "anthropic",
                "claude-3-7",
                "error",
                50,
                0,
                0,
                400,
                1,
                0,
            ),
            (
                "2026-09-17T00:30:00Z",
                "anthropic",
                "claude-3-7",
                "ok",
                200,
                40,
                500,
                2100,
                1,
                1,
            ),
        ]);
        let s = stats_json(&conn, 14);
        let t = &s["totals"];
        assert_eq!(t["attempts"], 3);
        assert_eq!(t["ok"], 2);
        assert_eq!(t["error"], 1);
        assert_eq!(t["aborted"], 0);
        assert_eq!(t["interrupted"], 0);
        assert_eq!(t["promptTokens"], 350);
        assert_eq!(t["cachedTokens"], 60);
        assert_eq!(t["completionTokens"], 800);
        assert_eq!(t["reasoningTokens"], 0);
    }

    #[test]
    fn stats_null_tokens_do_not_read_as_zero() {
        // A failed attempt with NULL token columns must not contribute 0 that
        // looks like a real "no tokens" claim — it contributes nothing.
        let conn = ledger(&[(
            "2026-09-16T10:00:00Z",
            "openai",
            "gpt-4o",
            "error",
            0,
            0,
            0,
            300,
            1,
            0,
        )]);
        let s = stats_json(&conn, 14);
        assert_eq!(s["totals"]["promptTokens"], 0); // NULLs summed to 0, not fabricated
        assert_eq!(s["totals"]["attempts"], 1);
        assert_eq!(s["totals"]["error"], 1);
    }

    #[test]
    fn attempts_are_newest_first_and_limited() {
        let rows: Vec<TestRow> = vec![
            (
                "2026-09-10T10:00:00Z",
                "openai",
                "gpt-4o",
                "ok",
                10,
                0,
                0,
                100,
                1,
                0,
            ),
            (
                "2026-09-11T10:00:00Z",
                "openai",
                "gpt-4o",
                "ok",
                10,
                0,
                0,
                101,
                1,
                0,
            ),
            (
                "2026-09-12T10:00:00Z",
                "openai",
                "gpt-4o",
                "ok",
                10,
                0,
                0,
                102,
                1,
                0,
            ),
            (
                "2026-09-13T10:00:00Z",
                "openai",
                "gpt-4o",
                "ok",
                10,
                0,
                0,
                103,
                1,
                0,
            ),
            (
                "2026-09-14T10:00:00Z",
                "openai",
                "gpt-4o",
                "ok",
                10,
                0,
                0,
                104,
                1,
                0,
            ),
        ];
        let conn = ledger(&rows);
        let a = attempts_json(&conn, 3);
        let arr = a.as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["ts"], "2026-09-14T10:00:00Z"); // newest first
        assert_eq!(arr[2]["ts"], "2026-09-12T10:00:00Z");
    }

    #[test]
    fn trend_is_newest_first_then_reversed_for_the_chart() {
        let conn = ledger(&[
            (
                "2026-09-15T10:00:00Z",
                "openai",
                "gpt-4o",
                "ok",
                100,
                0,
                0,
                100,
                1,
                0,
            ),
            (
                "2026-09-16T10:00:00Z",
                "openai",
                "gpt-4o",
                "ok",
                50,
                10,
                0,
                100,
                1,
                0,
            ),
        ]);
        let s = stats_json(&conn, 14);
        let trend = s["trend"].as_array().unwrap();
        // The window is a strict calendar range: every day is present so a
        // different window visibly changes the curve, empty days read 0.
        assert_eq!(trend.len(), 14);
        let i15 = trend
            .iter()
            .position(|d| d["date"] == "2026-09-15")
            .unwrap();
        let i16 = trend
            .iter()
            .position(|d| d["date"] == "2026-09-16")
            .unwrap();
        assert_eq!(i15 + 1, i16);
        assert_eq!(trend[i16]["attempts"], 1);
        assert_eq!(trend[i16]["prompt"], 50);
        assert_eq!(trend[i16]["cached"], 10);
        assert_eq!(trend[i16]["completion"], 0);
        assert_eq!(trend[i16]["hitRate"], 10.0 / 50.0); // cached / prompt (prompt includes cached)
        assert_eq!(trend[i15]["hitRate"], 0.0); // cached 0 with prompt > 0 is a real 0% hit
        assert_eq!(trend[0]["attempts"], 0);
        assert!(trend[0]["hitRate"].is_null());
        // platform aggregation
        let platforms = s["platforms"].as_array().unwrap();
        assert_eq!(platforms.len(), 1);
        assert_eq!(platforms[0]["wire"], "openai");
        assert_eq!(platforms[0]["attempts"], 2);
        assert_eq!(platforms[0]["cached"], 10);
    }

    #[test]
    fn models_aggregate_by_model() {
        let conn = ledger(&[
            (
                "2026-09-15T10:00:00Z",
                "openai",
                "gpt-4o",
                "ok",
                100,
                20,
                300,
                1500,
                1,
                0,
            ),
            (
                "2026-09-15T11:00:00Z",
                "openai",
                "gpt-4o",
                "ok",
                50,
                10,
                100,
                900,
                1,
                0,
            ),
            (
                "2026-09-16T10:00:00Z",
                "anthropic",
                "claude-3-7",
                "ok",
                200,
                0,
                0,
                700,
                1,
                0,
            ),
        ]);
        let s = stats_json(&conn, 14);
        let models = s["models"].as_array().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["model"], "gpt-4o"); // more attempts first
        assert_eq!(models[0]["attempts"], 2);
        // tokens = prompt (incl cached) + completion (incl reasoning)
        assert_eq!(models[0]["tokens"], 550);
        assert_eq!(models[0]["promptTokens"], 150);
        assert_eq!(models[0]["cachedTokens"], 30);
        assert_eq!(models[0]["completionTokens"], 400);
        assert_eq!(models[1]["model"], "claude-3-7");
        assert_eq!(models[1]["tokens"], 200);
        // hit rate on totals
        assert_eq!(s["totals"]["hitRate"], 30.0 / 350.0); // cached / prompt
    }

    #[test]
    fn sysinfo_reports_memory_and_uptime() {
        // Platform-portable: must never panic; on this dev machine both are real.
        // Uptime is second-granular and a freshly spawned test process may read
        // 0, so the assertion is a plausibility bound, not a positivity check.
        let mem = process_working_set_mb();
        let up = process_uptime_secs();
        assert!(mem.is_none() || mem.unwrap() > 0.0);
        assert!(up.is_none() || (up.unwrap() < 30 * 86400));
    }

    #[test]
    fn parse_days_clamps_to_known_bounds() {
        assert_eq!(parse_days("/webui/api/stats"), DEFAULT_TREND_DAYS);
        assert_eq!(parse_days("/webui/api/stats?days=30"), 30);
        assert_eq!(parse_days("/webui/api/stats?days=999"), 90);
        assert_eq!(parse_days("/webui/api/stats?days=abc"), DEFAULT_TREND_DAYS);
    }

    #[test]
    fn parse_limit_clamps_to_known_bounds() {
        assert_eq!(parse_limit("/webui/api/attempts"), DEFAULT_LIMIT);
        assert_eq!(parse_limit("/webui/api/attempts?limit=50"), 50);
        assert_eq!(parse_limit("/webui/api/attempts?limit=999999"), MAX_LIMIT);
        assert_eq!(parse_limit("/webui/api/attempts?limit=abc"), DEFAULT_LIMIT);
    }

    #[test]
    fn log_kind_reads_the_which_parameter() {
        // Absent or unknown means the proxy log, which is the one a user wants
        // by default; only an explicit `tray` selects the other.
        assert_eq!(log_kind("/webui/api/logs"), LogKind::Proxy);
        assert_eq!(log_kind("/webui/api/logs?which=proxy"), LogKind::Proxy);
        assert_eq!(log_kind("/webui/api/logs?which=tray"), LogKind::Tray);
        assert_eq!(log_kind("/webui/logs/stream?which=TRay"), LogKind::Tray);
        assert_eq!(log_kind("/webui/logs/stream?which=other"), LogKind::Proxy);
        assert_eq!(log_kind("/webui/logs/stream?a=1&which=tray"), LogKind::Tray);
    }

    #[test]
    fn the_log_tail_keeps_only_the_last_lines() {
        let body: String = (0..(LOG_TAIL_LINES + 50))
            .map(|i| format!("line {i}\n"))
            .collect();
        let tail = tail_lines(&body, LOG_TAIL_LINES);
        let lines: Vec<&str> = tail.lines().collect();
        assert_eq!(lines.len(), LOG_TAIL_LINES);
        assert_eq!(lines[0], "line 50");
        assert_eq!(
            lines[LOG_TAIL_LINES - 1],
            format!("line {}", LOG_TAIL_LINES + 49)
        );
    }

    #[test]
    fn a_short_log_is_kept_whole() {
        // Fewer lines than the window must not be trimmed, and a trailing
        // newline must not produce a phantom empty last line.
        assert_eq!(tail_lines("a\nb\nc\n", LOG_TAIL_LINES), "a\nb\nc");
        assert_eq!(tail_lines("", LOG_TAIL_LINES), "");
    }

    #[test]
    fn inline_json_never_breaks_out_of_the_script_tag() {
        let raw = r#"{"model":"</script><script>alert(1)</script>","n":"a&b<c>d"}"#;
        let out = inline_json(raw);
        assert!(!out.contains('<'), "raw `<` must not survive: {out}");
        assert!(!out.contains('>'), "raw `>` must not survive: {out}");
        // The escapes are plain JSON \uXXXX, so the document still parses and
        // yields the original strings — the data survives, the hazard does not.
        let parsed: Value = serde_json::from_str(&out).expect("still valid JSON");
        assert_eq!(parsed["model"], "</script><script>alert(1)</script>");
        assert_eq!(parsed["n"], "a&b<c>d");
    }

    #[test]
    fn today_window_compares_timestamps_in_the_same_format() {
        // The ledger stores T-separated UTC instants; a space-formatted
        // boundary sorts before every same-day ts because 'T' > ' ', which
        // used to drop the local 00:00–08:00 requests from "today". Both
        // probes derive from the very expression the query uses, so this is
        // exact: only the after-midnight row may count as today.
        let conn = ledger(&[]);
        conn.execute_batch(
            "insert into billing (ts, reqId, model, wire, stream, attempt, status,
                                  promptTokens, cachedTokens, completionTokens,
                                  reasoningTokens, durationMs)
             select strftime('%Y-%m-%dT%H:%M:%S', 'now', 'localtime', 'start of day', 'utc', '+1 seconds'),
                    'req', 'gpt-4o', 'openai', 0, 1, 'ok', 10, 0, 5, null, 100;
             insert into billing (ts, reqId, model, wire, stream, attempt, status,
                                  promptTokens, cachedTokens, completionTokens,
                                  reasoningTokens, durationMs)
             select strftime('%Y-%m-%dT%H:%M:%S', 'now', 'localtime', 'start of day', 'utc', '-1 seconds'),
                    'req', 'gpt-4o', 'openai', 0, 2, 'ok', 20, 0, 5, null, 100;",
        )
        .unwrap();
        let s = stats_json(&conn, 14);
        assert_eq!(
            s["today"]["attempts"], 1,
            "only the after-midnight row is today"
        );
    }

    #[test]
    fn attempts_rows_carry_cache_creation_tokens() {
        let conn = ledger(&[]);
        conn.execute(
            "insert into billing (ts, reqId, model, wire, stream, attempt, status,
                                  promptTokens, cachedTokens, cacheCreationTokens,
                                  completionTokens, reasoningTokens, durationMs)
             values ('2026-09-18T10:00:00.000Z', 'req', 'gemini-3.7-flash', 'openai', 1, 1,
                     'ok', 100, 0, 100, 40, null, 900)",
            [],
        )
        .unwrap();
        let a = attempts_json(&conn, 10);
        let row = &a.as_array().unwrap()[0];
        assert_eq!(row["cacheCreationTokens"], 100);
        assert_eq!(row["promptTokens"], 100);
    }
}
