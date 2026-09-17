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
use crate::server::{RequestId, SharedState};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::io::{self, Read};
use std::path::PathBuf;
use std::time::Duration;
use tiny_http::{Header, Request, Response, StatusCode};

/// The two-screen page, embedded at compile time — one self-contained file
/// with inline CSS/JS, no external assets, so it works with the proxy offline.
const PAGE: &str = include_str!("webui.html");

/// How many ledger rows the detail screen shows by default.
const DEFAULT_LIMIT: i64 = 200;
/// Default trend window (days) for the overview screen.
const DEFAULT_TREND_DAYS: i64 = 14;
const MAX_LIMIT: i64 = 1000;
/// Poll interval of the live stream.
const SSE_POLL_MS: u64 = 1000;
/// Emit a keepalive comment every N idle polls (~10 s).
const SSE_KEEPALIVE_EVERY: u32 = 10;

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

/// `GET /webui` — the page itself.
pub fn handle_page(state: &SharedState, req: Request, _ctx: &RequestId) {
    let _ = state;
    respond(req, 200, PAGE.to_string(), "text/html; charset=utf-8");
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
    let body = with_ledger(|conn| attempts_json(conn, limit));
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
            "select count(*),
                    coalesce(sum(promptTokens), 0),
                    coalesce(sum(cachedTokens), 0),
                    coalesce(sum(completionTokens), 0),
                    coalesce(sum(reasoningTokens), 0)
             from billing
             where ts >= datetime('now', 'localtime', 'start of day')",
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
        let stmt = conn
            .prepare(
                "select strftime('%Y-%m-%d', ts, 'localtime') as day,
                        count(*),
                        coalesce(sum(promptTokens), 0),
                        coalesce(sum(cachedTokens), 0),
                        coalesce(sum(completionTokens), 0)
                 from billing
                 group by day
                 order by day desc
                 limit ?1",
            )
            .ok();
        let mut days: Vec<Value> = Vec::new();
        if let Some(mut stmt) = stmt {
            if let Ok(rows) = stmt.query_map([window], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            }) {
                for row in rows.flatten() {
                    let denom = row.2.saturating_add(row.3);
                    let hit_rate = if denom > 0 {
                        Some(row.3 as f64 / denom as f64)
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
        days.reverse(); // oldest → newest for the chart
        days
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
        let stmt = conn
            .prepare(
                "select model,
                        count(*),
                        coalesce(sum(promptTokens), 0)
                            + coalesce(sum(cachedTokens), 0)
                            + coalesce(sum(completionTokens), 0)
                            + coalesce(sum(reasoningTokens), 0)
                 from billing
                 group by model
                 order by count(*) desc, 3 desc
                 limit 10",
            )
            .ok();
        let mut out: Vec<Value> = Vec::new();
        if let Some(mut stmt) = stmt {
            if let Ok(rows) = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            }) {
                for row in rows.flatten() {
                    out.push(json!({ "model": row.0, "attempts": row.1, "tokens": row.2 }));
                }
            }
        }
        out
    };

    let hit_denom = prompt.saturating_add(cached);
    let hit_rate = if hit_denom > 0 {
        Some(cached as f64 / hit_denom as f64)
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
    })
}
/// Screen-two payload: the most recent attempts, newest first.
fn attempts_json(conn: &Connection, limit: i64) -> Value {
    let mut stmt = match conn.prepare(
        "select ts, reqId, wire, model, stream, attempt, status, errorTag,
                promptTokens, cachedTokens, completionTokens, reasoningTokens,
                durationMs, ttfbMs
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
                "completionTokens": row.get::<_, Option<i64>>(10)?,
                "reasoningTokens": row.get::<_, Option<i64>>(11)?,
                "durationMs": row.get::<_, Option<i64>>(12)?,
                "ttfbMs": row.get::<_, Option<i64>>(13)?,
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
        assert_eq!(trend.len(), 2);
        assert_eq!(trend[0]["date"], "2026-09-15");
        assert_eq!(trend[1]["date"], "2026-09-16");
        assert_eq!(trend[1]["attempts"], 1);
        assert_eq!(trend[1]["prompt"], 50);
        assert_eq!(trend[1]["cached"], 10);
        assert_eq!(trend[1]["completion"], 0);
        assert_eq!(trend[1]["hitRate"], 10.0 / 60.0);
        assert_eq!(trend[0]["hitRate"], 0.0); // cached 0 with prompt > 0 is a real 0% hit
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
        assert_eq!(models[0]["tokens"], 580);
        assert_eq!(models[1]["model"], "claude-3-7");
        assert_eq!(models[1]["tokens"], 200);
        // hit rate on totals
        assert_eq!(s["totals"]["hitRate"], 30.0 / 380.0);
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
}
