//! The recovery wiring, end to end: the reconnect closure + `SseBody`'s Read
//! loop + the billing ledger, all real.
//!
//! The encoder tests and `stream_golden` drive encoders directly; `record.mjs`
//! drives the whole binary from outside. The bugs this file is after live in
//! neither place — they live in how the pieces are joined: whether a spliced
//! recovery leaves exactly two ledger rows (interrupted/NULL, then ok/usage),
//! whether a refused retry leaves one, and whether the outcome reaches the
//! ledger through the same `Arc<dyn Sink>` clones the server actually uses.

use ccproxy::billing::{RequestLedger, Wire};
use ccproxy::sse::StreamFailure;
use ccproxy::stream_body::{Dialect, SseBody};
use ccproxy::upstream::UpstreamStream;
use rusqlite::Connection;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// The tests install a process-wide ledger, so they must not run concurrently.
fn lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A temp database installed as the process ledger, plus the request ledger
/// the server would hold, wired to it exactly as `server.rs` wires it.
struct ScopedLedger {
    dir: PathBuf,
    ledger: Arc<RequestLedger>,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl ScopedLedger {
    fn install(tag: &str) -> Self {
        let guard = lock();
        let dir =
            std::env::temp_dir().join(format!("ccproxy-splice-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let writer = ccproxy::billing::Ledger::open(&dir).expect("ledger opens");
        ccproxy::billing::use_ledger_for_tests(writer);
        let ledger = Arc::new(RequestLedger::new("req-1", "m", Wire::Anthropic, true));
        Self {
            dir,
            ledger,
            _guard: guard,
        }
    }

    fn rows(&self) -> Vec<(String, Option<i64>, Option<i64>)> {
        ccproxy::billing::flush();
        let conn = Connection::open(self.dir.join("billing.db")).expect("reopen");
        let mut stmt = conn
            .prepare("select status, promptTokens, completionTokens from billing order by id")
            .expect("prepare");
        let mapped = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get(1)?, row.get(2)?))
            })
            .expect("query");
        mapped.filter_map(Result::ok).collect()
    }
}

impl Drop for ScopedLedger {
    fn drop(&mut self) {
        ccproxy::billing::clear_ledger_override_for_tests();
    }
}

/// A reader that yields scripted bytes and then reports a transport failure.
struct Scripted {
    data: Vec<u8>,
    pos: usize,
    fail_after: Option<std::io::ErrorKind>,
}

impl Scripted {
    fn clean(data: &str) -> Box<Scripted> {
        Box::new(Self {
            data: data.as_bytes().to_vec(),
            pos: 0,
            fail_after: None,
        })
    }

    /// Bytes, then a connection reset — a mid-body transport death.
    fn then_reset(data: &str) -> Box<Scripted> {
        Box::new(Self {
            data: data.as_bytes().to_vec(),
            pos: 0,
            fail_after: Some(std::io::ErrorKind::ConnectionReset),
        })
    }
}

impl Read for Scripted {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos < self.data.len() {
            let take = (self.data.len() - self.pos).min(buf.len());
            buf[..take].copy_from_slice(&self.data[self.pos..self.pos + take]);
            self.pos += take;
            return Ok(take);
        }
        match self.fail_after {
            Some(kind) => Err(std::io::Error::new(kind, "scripted failure")),
            None => Ok(0),
        }
    }
}

fn ndjson(events: &[&str]) -> String {
    events.iter().map(|e| format!("data: {e}\n")).collect()
}

/// The 2026-09-15 production shape: reasoning delivered, then an in-band
/// error, before any answer text.
const FIRST_ATTEMPT: &[&str] = &[
    r#"{"type":"start","data":{}}"#,
    r#"{"type":"reasoning-delta","data":{"text":"thinking..."}}"#,
    r#"{"type":"error","data":{"message":"Network connection lost."}}"#,
];

const SECOND_ATTEMPT: &[&str] = &[
    r#"{"type":"start","data":{}}"#,
    r#"{"type":"reasoning-delta","data":{"text":"replayed, must be dropped"}}"#,
    r#"{"type":"text-delta","data":{"text":"Here is the answer."}}"#,
    r#"{"type":"finish","data":{"finishReason":"stop","totalUsage":{"inputTokens":1234,"outputTokens":56}}}"#,
];

#[test]
fn a_spliced_recovery_leaves_two_rows_and_one_continuous_stream() {
    let scoped = ScopedLedger::install("splice-ok");
    let ledger = Arc::clone(&scoped.ledger);

    let first = UpstreamStream::new(Scripted::clean(&ndjson(FIRST_ATTEMPT)), 800);
    let resend_count = Arc::new(AtomicUsize::new(0));
    let reconnect = {
        let count = Arc::clone(&resend_count);
        let boxed: Box<dyn FnMut() -> Result<UpstreamStream, StreamFailure> + Send> =
            Box::new(move || {
                count.fetch_add(1, Ordering::Relaxed);
                // The replacement reports to the same ledger, exactly as the
                // server's reconnect closure does.
                Ok(UpstreamStream::new(
                    Scripted::clean(&ndjson(SECOND_ATTEMPT)),
                    800,
                ))
            });
        boxed
    };
    let usage = SseBody::new_slot();
    let mut body = SseBody::with_outcome(
        first,
        Dialect::Anthropic,
        "m",
        Some(reconnect),
        Arc::clone(&usage),
        Some(Arc::clone(&ledger) as Arc<dyn ccproxy::stream_body::StreamOutcomeSink>),
    );

    let mut out = Vec::new();
    body.read_to_end(&mut out).expect("body reads");
    let text = String::from_utf8_lossy(&out).into_owned();

    // The stream: one message_start, the first attempt's thinking kept, the
    // second attempt's answer appended, the replayed thinking gone. Each SSE
    // record carries the name twice (event line + type field), so one record
    // means exactly two occurrences.
    assert_eq!(resend_count.load(Ordering::Relaxed), 1, "one re-send");
    assert_eq!(
        text.matches("message_start").count(),
        2,
        "exactly one message_start record: {text}"
    );
    assert!(text.contains("thinking..."), "{text}");
    assert!(!text.contains("replayed, must be dropped"), "{text}");
    assert!(text.contains("Here is the answer."), "{text}");
    assert!(text.contains("message_stop"), "{text}");
    assert!(!text.contains("Network connection lost"), "{text}");

    // The server settles after respond: this is the wiring under test, so it
    // happens here too, with the usage the body published.
    let observed = usage.lock().ok().and_then(|g| g.clone());
    ledger.settle_ok(observed.as_ref());

    // The ledger: the died attempt is interrupted/NULL, the replacement is
    // ok with the usage it reported.
    let rows = scoped.rows();
    assert_eq!(rows.len(), 2, "one row per attempt: {rows:?}");
    assert_eq!(rows[0], ("interrupted".into(), None, None));
    assert_eq!(rows[1], ("ok".into(), Some(1234), Some(56)));

    // And the settlement rule: a later settle_ok must not add a third row.
    ledger.settle_ok(None);
    assert_eq!(scoped.rows().len(), 2, "settlement is not double-written");
}

#[test]
fn a_refused_retry_leaves_one_row_and_reports_the_failure() {
    let scoped = ScopedLedger::install("splice-refused");
    let ledger = Arc::clone(&scoped.ledger);

    // Answer text delivered, then the transport dies: nothing to recover.
    let only = ndjson(&[
        r#"{"type":"start","data":{}}"#,
        r#"{"type":"text-delta","data":{"text":"partial answer"}}"#,
    ]);
    let first = UpstreamStream::new(Scripted::then_reset(&only), 800);
    let resend_count = Arc::new(AtomicUsize::new(0));
    let reconnect = {
        let count = Arc::clone(&resend_count);
        let boxed: Box<dyn FnMut() -> Result<UpstreamStream, StreamFailure> + Send> =
            Box::new(move || {
                count.fetch_add(1, Ordering::Relaxed);
                Ok(UpstreamStream::new(Scripted::clean(""), 800))
            });
        boxed
    };
    let usage = SseBody::new_slot();
    let mut body = SseBody::with_outcome(
        first,
        Dialect::Anthropic,
        "m",
        Some(reconnect),
        Arc::clone(&usage),
        Some(Arc::clone(&ledger) as Arc<dyn ccproxy::stream_body::StreamOutcomeSink>),
    );

    let mut out = Vec::new();
    body.read_to_end(&mut out).expect("body reads");
    let text = String::from_utf8_lossy(&out).into_owned();

    assert_eq!(resend_count.load(Ordering::Relaxed), 0, "never re-sent");
    assert!(text.contains("partial answer"), "{text}");
    assert!(text.contains("connection-reset"), "{text}");
    assert!(text.contains("message_stop"), "{text}");

    let rows = scoped.rows();
    assert_eq!(rows.len(), 1, "one attempt, one row: {rows:?}");
    assert_eq!(rows[0].0, "error");

    // The server's post-respond settle_ok must not relabel the failure.
    ledger.settle_ok(None);
    let rows = scoped.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "error", "the failure row stands");
}
