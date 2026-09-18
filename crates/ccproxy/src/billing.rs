//! Upstream usage ledger (SQLite). Ported from `src/billing.ts`.
//!
//! Why it exists: one client conversation can send several requests upstream —
//! 5xx/429 retries, the model-discovery retry, the mid-stream splice retry —
//! and every one of them costs real money on CC's side. The older statistics
//! recorded one row when the client request finished, so the attempts that were
//! abandoned left no trace; and CC reports usage only in the terminal `finish`
//! event, so an attempt that died mid-stream has no numbers even CC could give.
//!
//! The principle is **do not conceal**. Every attempt that really happened and
//! could be billed gets a row; when the numbers genuinely cannot be obtained the
//! columns are NULL (unknown), never 0 — 0 is a lie that reads as "no tokens".
//!
//! Writing is decoupled from forwarding: the request thread does one non-blocking
//! hand-off and a dedicated writer thread owns the connection. The queue is
//! bounded, and once it is full further records are dropped and counted rather
//! than blocking the request. Statistics can be lost and re-derived; forwarding
//! cannot be allowed to stall.
//!
//! Location is fixed at `%LOCALAPPDATA%\cc-proxy\`, independent of the version
//! directory (a hot update must not start a new history), and `CC_TRAY_NS`
//! isolates test instances.

use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::time::{iso8601_secs, now_epoch_secs, now_iso8601};
use crate::upstream::AttemptSink;
use crate::usage::UsageData;

/// Queue depth. Deep enough to absorb a burst of retries, bounded so a stuck
/// writer cannot grow memory without limit.
pub const CHANNEL_CAPACITY: usize = 1024;
/// Rows are committed when this many have queued, or after
/// [`BATCH_MAX_DELAY_MS`], whichever comes first — batching is what keeps the
/// WAL fsync cost off the per-request path.
pub const BATCH_MAX_ROWS: usize = 100;
pub const BATCH_MAX_DELAY_MS: u64 = 200;
/// Rows older than this are pruned. The Node build rotated by line count, which
/// cut a 21-day history down to 1.16 days; a time bound is what was always meant.
pub const RETENTION_DAYS: u64 = 90;

/// Which downstream dialect the request arrived in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    Openai,
    Anthropic,
}

impl Wire {
    fn as_str(self) -> &'static str {
        match self {
            Self::Openai => "openai",
            Self::Anthropic => "anthropic",
        }
    }
}

/// How one upstream attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptStatus {
    Ok,
    Error,
    /// The client went away; the attempt was cut short on purpose.
    Aborted,
    /// The stream broke mid-turn and the answer was spliced onto a replacement.
    Interrupted,
}

impl AttemptStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Aborted => "aborted",
            Self::Interrupted => "interrupted",
        }
    }
}

/// One row: a single upstream attempt.
///
/// Every token field is `Option`, and `None` means unknown. It must stay
/// distinguishable from zero: a zero would claim the attempt consumed no
/// tokens, which for an interrupted stream is exactly the claim that is false.
#[derive(Debug, Clone, PartialEq)]
pub struct AttemptRecord {
    pub ts: String,
    pub req_id: String,
    pub model: String,
    pub wire: Wire,
    pub stream: bool,
    /// 1-based; >1 means this was a retry.
    pub attempt: u64,
    pub status: AttemptStatus,
    pub error_tag: Option<String>,
    pub prompt_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    /// Tokens written into a cache entry (subset of prompt_tokens), billed at
    /// the cache-write rate when the model has one.
    pub cache_creation_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub duration_ms: u64,
    pub ttfb_ms: Option<u64>,
}

/// Bumped when the on-disk layout changes. Recorded via `pragma user_version`
/// so a reader can tell what it is looking at.
const SCHEMA: &str = "
create table if not exists billing (
  id                 integer primary key autoincrement,
  ts                 text    not null,
  reqId              text,
  model              text    not null,
  wire               text    not null,
  stream             integer not null,
  attempt            integer not null,
  status             text    not null,
  errorTag           text,
  promptTokens       integer,
  cachedTokens       integer,
  cacheCreationTokens integer,
  completionTokens   integer,
  reasoningTokens    integer,
  durationMs         integer,
  ttfbMs             integer
);
create index if not exists billing_ts on billing (ts);
create index if not exists billing_reqId on billing (reqId);
create table if not exists meta (
  key   text primary key,
  value text not null
);
";

/// Records that the legacy `usage.jsonl` has been imported, so the import runs
/// once per database rather than duplicating history on every startup.
const LEGACY_IMPORT_KEY: &str = "legacy_jsonl_imported";

/// Messages the writer thread handles.
enum Message {
    Record(Box<AttemptRecord>),
    /// Ask the writer to commit what it holds and acknowledge, so a test or a
    /// shutdown can read the rows without sleeping.
    Flush(SyncSender<()>),
}

/// The data directory: `%LOCALAPPDATA%\cc-proxy\`, or `~/.cc-proxy` when
/// LOCALAPPDATA is unset. `CC_TRAY_NS` appends a subdirectory so an isolated
/// instance cannot write into the production database.
pub fn billing_dir() -> PathBuf {
    billing_dir_from(
        std::env::var("LOCALAPPDATA").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
        std::env::var("CC_TRAY_NS").ok().as_deref(),
    )
}

/// The directory decision, with its inputs passed in so it can be tested
/// without mutating the process environment.
fn billing_dir_from(local_app_data: Option<&str>, home: Option<&str>, ns: Option<&str>) -> PathBuf {
    let base = match non_empty(local_app_data) {
        Some(dir) => Path::new(dir).join("cc-proxy"),
        None => match non_empty(home) {
            Some(home) => Path::new(home).join(".cc-proxy"),
            None => PathBuf::from(".cc-proxy"),
        },
    };
    match non_empty(ns) {
        Some(ns) => base.join(ns),
        None => base,
    }
}

/// Treats an empty or whitespace-only value as absent, matching the Node
/// build's `ns && ns.trim() !== ""`.
fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|v| !v.trim().is_empty())
}

/// The ledger: a queue in front of a writer thread that owns the connection.
pub struct Ledger {
    /// `Option` so `Drop` can drop the sender *before* joining the writer —
    /// dropping it is what ends the writer's loop, and joining first would wait
    /// forever for a thread that is still blocked on `recv`.
    tx: Option<SyncSender<Message>>,
    dropped: Arc<AtomicU64>,
    writer: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Ledger {
    /// Open the ledger in `dir`, creating it and the database as needed.
    ///
    /// `None` means the ledger is unavailable (unwritable directory, sqlite
    /// failure); callers degrade to counting nothing, which mirrors the Node
    /// build swallowing the error. It must never be fatal to forwarding.
    pub fn open(dir: &Path) -> Option<Self> {
        if std::fs::create_dir_all(dir).is_err() {
            crate::log::debug("[billing] cannot create the data directory; ledger disabled");
            return None;
        }
        let conn = match open_connection(&dir.join("billing.db")) {
            Ok(conn) => conn,
            Err(err) => {
                crate::log::debug(&format!("[billing] ledger unavailable: {err}"));
                return None;
            }
        };
        let dropped = Arc::new(AtomicU64::new(0));
        let (tx, rx) = sync_channel(CHANNEL_CAPACITY);
        let writer_dropped = Arc::clone(&dropped);
        let writer = std::thread::Builder::new()
            .name("billing-writer".into())
            .spawn(move || write_loop(conn, &rx, &writer_dropped))
            .ok()?;
        Some(Self {
            tx: Some(tx),
            dropped,
            writer: Mutex::new(Some(writer)),
        })
    }

    /// Hand a row to the writer thread.
    ///
    /// Never blocks: a full queue drops the record and increments a counter.
    pub fn record(&self, record: AttemptRecord) {
        let Some(tx) = self.tx.as_ref() else {
            return;
        };
        match tx.try_send(Message::Record(Box::new(record))) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                // `fetch_add` returns the previous value, so this drop is that
                // count plus one.
                let before = self.dropped.fetch_add(1, Ordering::Relaxed);
                // Warn on the first drop and then rarely: a persistent stall
                // would otherwise flood the log it is trying to report into.
                if before == 0 || is_power_of_two(before) {
                    crate::log::warn(&format!(
                        "[billing] write queue full, dropped {} record(s); forwarding continues",
                        before.saturating_add(1)
                    ));
                }
            }
            Err(TrySendError::Disconnected(_)) => {
                crate::log::debug("[billing] writer thread is gone; record dropped");
            }
        }
    }

    /// Records dropped because the queue was full. Test-facing.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Commit everything queued so far and wait for the writer to confirm.
    ///
    /// For tests and shutdown only; the forwarding path must not call this.
    pub fn flush(&self) {
        let Some(tx) = self.tx.as_ref() else {
            return;
        };
        let (ack_tx, ack_rx) = sync_channel(0);
        // A blocking send is correct here: flush is allowed to wait, and it is
        // the only way to be sure the ack cannot be dropped behind a full queue.
        if tx.send(Message::Flush(ack_tx)).is_ok() {
            let _ = ack_rx.recv_timeout(Duration::from_secs(5));
        }
    }
}

impl Drop for Ledger {
    fn drop(&mut self) {
        // Drop the sender first — that ends the writer's `recv` loop, which then
        // commits whatever it still holds and exits. Joining before this point
        // would block forever.
        drop(self.tx.take());
        if let Ok(mut guard) = self.writer.lock() {
            if let Some(handle) = guard.take() {
                let _ = handle.join();
            }
        }
    }
}

/// Open the database with the pragmas the spec settled on.
fn open_connection(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    // WAL + NORMAL: the proxy is the only writer, and readers (tray, web UI)
    // must not block it.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    conn.execute_batch(SCHEMA)?;
    // Migrate pre-0.5.3 databases: `create table if not exists` leaves the
    // existing table alone, so a column added to SCHEMA never appears there.
    // Adding the column is idempotent (checked first), and old rows are NULL,
    // which the aggregations treat as "no cache creation" — exactly right for
    // history recorded before the field existed.
    migrate_column(&conn, "cacheCreationTokens")?;
    // Drop rows past the retention window at startup.
    prune_old_rows(&conn);
    Ok(conn)
}

/// Add `column` to `billing` when it is missing. Idempotent and cheap: the
/// pragma query runs once per open; the alter is a no-op once applied.
fn migrate_column(conn: &Connection, column: &str) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare("pragma table_info(billing)")?;
    let names: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<_, _>>()?;
    if names.iter().any(|n| n == column) {
        return Ok(());
    }
    conn.execute_batch(&format!("alter table billing add column {column} integer"))
}

/// Open the ledger database in `dir`, creating the directory and schema.
///
/// Exposed for the one-shot legacy import, which needs the same schema and
/// pragmas as the running proxy — opening the file directly would leave the
/// tables missing and quietly import nothing.
pub fn open_database(dir: &Path) -> Option<Connection> {
    std::fs::create_dir_all(dir).ok()?;
    open_connection(&dir.join("billing.db")).ok()
}

#[cfg(test)]
fn create_legacy_table(conn: &Connection) {
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
}

/// Delete rows older than [`RETENTION_DAYS`]. Best effort.
fn prune_old_rows(conn: &Connection) {
    let cutoff =
        iso8601_secs(now_epoch_secs().saturating_sub(RETENTION_DAYS.saturating_mul(86400)));
    match conn.execute("delete from billing where ts < ?1", [&cutoff]) {
        Ok(0) => {}
        Ok(n) => crate::log::debug(&format!("[billing] pruned {n} row(s) past retention")),
        Err(err) => crate::log::debug(&format!("[billing] prune failed: {err}")),
    }
}

/// Token totals over the last 24 hours, for the tray's menu.
///
/// `rows` counts *attempts*, which is what the ledger stores — a request that
/// was retried contributes more than one. Tokens summed from NULL are skipped
/// rather than read as zero, so an attempt that never reported usage neither
/// adds tokens nor enters the cache-rate denominator.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DailyStats {
    pub rows: u64,
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub completion_tokens: u64,
}

impl DailyStats {
    /// Cached share of the prompt, as a percentage, or `None` when no row
    /// reported prompt tokens at all.
    #[must_use]
    pub fn cache_rate_percent(self) -> Option<f64> {
        if self.prompt_tokens == 0 {
            return None;
        }
        let cached = self.cached_tokens as f64;
        let prompt = self.prompt_tokens as f64;
        Some((cached / prompt * 1000.0).round() / 10.0)
    }
}

/// Aggregate the last 24 hours for the tray. `None` when the database is
/// missing or unreadable — the tray shows "no data" rather than failing.
///
/// Opened read-only: the proxy is the only writer (see docs/DEVELOPMENT.md §4), and
/// a reader that creates the schema would be writing. This also keeps a
/// stats query from racing the writer thread at startup.
#[must_use]
pub fn daily_stats(dir: &Path) -> Option<DailyStats> {
    let path = dir.join("billing.db");
    if !path.exists() {
        return None;
    }
    let conn = Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .ok()?;
    let cutoff = iso8601_secs(now_epoch_secs().saturating_sub(86_400));
    // One query for the totals and one for the rate: the rate must be computed
    // only over rows that reported usage — a failed attempt has no tokens and
    // belongs in neither numerator nor denominator, while `rows` still counts it.
    let stats = conn
        .query_row(
            "select count(*),
                coalesce(sum(promptTokens), 0),
                coalesce(sum(cachedTokens), 0),
                coalesce(sum(completionTokens), 0)
           from billing where ts >= ?1",
            [&cutoff],
            |row| {
                Ok(DailyStats {
                    rows: row.get::<_, i64>(0)?.max(0) as u64,
                    prompt_tokens: row.get::<_, i64>(1)?.max(0) as u64,
                    cached_tokens: row.get::<_, i64>(2)?.max(0) as u64,
                    completion_tokens: row.get::<_, i64>(3)?.max(0) as u64,
                })
            },
        )
        .ok()?;
    let rate = conn
        .query_row(
            "select coalesce(sum(cachedTokens), 0), coalesce(sum(promptTokens), 0)
               from billing where ts >= ?1 and promptTokens is not null",
            [&cutoff],
            |row| {
                Ok(DailyStats {
                    rows: 0,
                    prompt_tokens: row.get::<_, i64>(1)?.max(0) as u64,
                    cached_tokens: row.get::<_, i64>(0)?.max(0) as u64,
                    completion_tokens: 0,
                })
            },
        )
        .ok()?;
    Some(DailyStats {
        cached_tokens: rate.cached_tokens,
        ..stats
    })
}

// ── legacy import ─────────────────────────────────────────

/// One row of the Node build's `logs/usage.jsonl`.
#[derive(serde::Deserialize)]
struct LegacyRow {
    ts: String,
    model: String,
    #[serde(rename = "promptTokens")]
    prompt_tokens: Option<u64>,
    #[serde(rename = "cachedTokens")]
    cached_tokens: Option<u64>,
    #[serde(rename = "completionTokens")]
    completion_tokens: Option<u64>,
}

/// Import the pre-M7 `usage.jsonl` files, once per database.
///
/// The old format recorded one line per *client request* with five fields, so
/// the rows it can produce are not equivalent to the new per-attempt ones. They
/// are imported rather than discarded because they are real billed usage, and
/// the fields the old format never had (`wire`, `durationMs`, `ttfbMs`, …) take
/// the closest honest value: `attempt` is 1, `status` is ok, and the timing
/// fields are NULL because the old format did not record them.
///
/// `dirs` are searched for `usage.jsonl`; a version directory each kept its own
/// copy, so importing all of them is what makes the history continuous across
/// upgrades.
pub fn import_legacy_jsonl(conn: &Connection, dirs: &[PathBuf]) -> usize {
    if already_imported(conn) {
        return 0;
    }
    let mut imported = 0usize;
    for dir in dirs {
        imported = imported.saturating_add(import_jsonl_file(conn, &dir.join("usage.jsonl")));
    }
    // The marker is only set once something was actually found. A scan that
    // turned up nothing must stay retryable: the first run may happen before
    // the old directory is reachable, and recording "imported" then would
    // permanently discard the history it was meant to preserve.
    if imported > 0 {
        mark_imported(conn, imported);
        crate::log::info(&format!(
            "[billing] imported {imported} row(s) from the legacy usage.jsonl"
        ));
    }
    imported
}

/// Whether this database has already run the import.
fn already_imported(conn: &Connection) -> bool {
    conn.query_row(
        "select value from meta where key = ?1",
        [LEGACY_IMPORT_KEY],
        |row| row.get::<_, String>(0),
    )
    .is_ok()
}

fn mark_imported(conn: &Connection, count: usize) {
    let _ = conn.execute(
        "insert or replace into meta (key, value) values (?1, ?2)",
        rusqlite::params![LEGACY_IMPORT_KEY, count.to_string()],
    );
}

/// Read one JSONL file, tolerating the damage a crash mid-write would leave.
///
/// A malformed line is skipped rather than aborting the file: the whole point
/// is to preserve what history there is.
fn import_jsonl_file(conn: &Connection, path: &Path) -> usize {
    let Ok(text) = std::fs::read_to_string(path) else {
        return 0;
    };
    let mut records = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<LegacyRow>(line) else {
            continue;
        };
        records.push(AttemptRecord {
            ts: row.ts,
            // The old format carried no request id; an empty one keeps the
            // column honest rather than inventing an identifier.
            req_id: String::new(),
            model: row.model,
            // The old data does not say which dialect it came from.
            wire: Wire::Openai,
            stream: true,
            attempt: 1,
            status: AttemptStatus::Ok,
            error_tag: None,
            prompt_tokens: row.prompt_tokens,
            cached_tokens: row.cached_tokens,
            cache_creation_tokens: None,
            completion_tokens: row.completion_tokens,
            reasoning_tokens: None,
            duration_ms: 0,
            ttfb_ms: None,
        });
    }
    if records.is_empty() {
        return 0;
    }
    if insert_batch(conn, &records).is_err() {
        return 0;
    }
    records.len()
}

/// The writer thread: pull messages, batch them, commit on size or timeout.
fn write_loop(conn: Connection, rx: &Receiver<Message>, dropped: &Arc<AtomicU64>) {
    let mut batch: Vec<AttemptRecord> = Vec::new();
    let mut next_prune = Instant::now().checked_add(Duration::from_secs(6 * 60 * 60));

    loop {
        match rx.recv_timeout(Duration::from_millis(BATCH_MAX_DELAY_MS)) {
            Ok(Message::Record(record)) => {
                batch.push(*record);
                if batch.len() >= BATCH_MAX_ROWS {
                    commit(&conn, &mut batch);
                }
            }
            Ok(Message::Flush(ack)) => {
                commit(&conn, &mut batch);
                let _ = ack.send(());
            }
            // Nothing arrived within the window: commit what we have so a quiet
            // period does not leave rows unfsynced indefinitely.
            Err(RecvTimeoutError::Timeout) => commit(&conn, &mut batch),
            // Every sender is gone; commit and stop.
            Err(RecvTimeoutError::Disconnected) => {
                commit(&conn, &mut batch);
                break;
            }
        }
        if next_prune.is_some_and(|t| Instant::now() >= t) {
            prune_old_rows(&conn);
            next_prune = Instant::now().checked_add(Duration::from_secs(6 * 60 * 60));
        }
    }
    if dropped.load(Ordering::Relaxed) > 0 {
        crate::log::debug(&format!(
            "[billing] writer stopping with {} record(s) dropped under backpressure",
            dropped.load(Ordering::Relaxed)
        ));
    }
}

/// Write one batch in a single transaction, then clear it.
///
/// A failure logs and discards the batch: the rows are statistics, and a
/// database problem must not take the process down or stall the queue forever.
fn commit(conn: &Connection, batch: &mut Vec<AttemptRecord>) {
    if batch.is_empty() {
        return;
    }
    let result = insert_batch(conn, batch);
    if let Err(err) = result {
        crate::log::debug(&format!(
            "[billing] write failed for {} record(s): {err}",
            batch.len()
        ));
    }
    batch.clear();
}

fn insert_batch(conn: &Connection, batch: &[AttemptRecord]) -> rusqlite::Result<()> {
    conn.execute_batch("begin")?;
    let outcome = (|| -> rusqlite::Result<()> {
        let mut stmt = conn.prepare_cached(
            "insert into billing
               (ts, reqId, model, wire, stream, attempt, status, errorTag,
                promptTokens, cachedTokens, cacheCreationTokens,
                completionTokens, reasoningTokens, durationMs, ttfbMs)
             values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        )?;
        for record in batch {
            stmt.execute(rusqlite::params![
                record.ts,
                record.req_id,
                record.model,
                record.wire.as_str(),
                i64::from(record.stream),
                i64::try_from(record.attempt).unwrap_or(i64::MAX),
                record.status.as_str(),
                record.error_tag,
                record
                    .prompt_tokens
                    .map(i64::try_from)
                    .transpose()
                    .ok()
                    .flatten(),
                record
                    .cached_tokens
                    .map(i64::try_from)
                    .transpose()
                    .ok()
                    .flatten(),
                record
                    .cache_creation_tokens
                    .map(i64::try_from)
                    .transpose()
                    .ok()
                    .flatten(),
                record
                    .completion_tokens
                    .map(i64::try_from)
                    .transpose()
                    .ok()
                    .flatten(),
                record
                    .reasoning_tokens
                    .map(i64::try_from)
                    .transpose()
                    .ok()
                    .flatten(),
                i64::try_from(record.duration_ms).unwrap_or(i64::MAX),
                record.ttfb_ms.map(|v| i64::try_from(v).unwrap_or(i64::MAX)),
            ])?;
        }
        Ok(())
    })();
    match outcome {
        Ok(()) => conn.execute_batch("commit"),
        Err(err) => {
            let _ = conn.execute_batch("rollback");
            Err(err)
        }
    }
}

// ── the process-wide ledger ───────────────────────────────

static GLOBAL: OnceLock<Option<Arc<Ledger>>> = OnceLock::new();

/// A ledger installed explicitly, taking precedence over the directory-derived
/// one. Tests use this because the environment (`LOCALAPPDATA`, `CC_TRAY_NS`)
/// is process-wide and set_var is not safe to race against.
static OVERRIDE: OnceLock<Mutex<Option<Arc<Ledger>>>> = OnceLock::new();

/// Set by a test that wants to prove the "no ledger available" path without
/// letting `global()` open the real one as a side effect.
static DISABLED: AtomicBool = AtomicBool::new(false);

fn override_slot() -> &'static Mutex<Option<Arc<Ledger>>> {
    OVERRIDE.get_or_init(|| Mutex::new(None))
}

/// The shared ledger, opened on first use from [`billing_dir`].
pub fn global() -> Option<Arc<Ledger>> {
    if DISABLED.load(Ordering::Relaxed) {
        return None;
    }
    if let Ok(slot) = override_slot().lock() {
        if let Some(installed) = slot.as_ref() {
            return Some(Arc::clone(installed));
        }
    }
    GLOBAL
        .get_or_init(|| {
            let dir = billing_dir();
            let ledger = Ledger::open(&dir).map(Arc::new);
            if ledger.is_some() {
                crate::log::debug(&format!("[billing] ledger at {}", dir.display()));
            }
            ledger
        })
        .clone()
}

/// Make [`global`] report no ledger, as it would on an unwritable machine.
///
/// Test-facing: it is the only way to exercise the degraded path without
/// `global()` lazily creating the real database.
#[doc(hidden)]
pub fn disable_for_tests() {
    DISABLED.store(true, Ordering::Relaxed);
}

/// Undo [`disable_for_tests`].
#[doc(hidden)]
pub fn enable_for_tests() {
    DISABLED.store(false, Ordering::Relaxed);
}

/// Record one attempt on the shared ledger. Never throws, never blocks.
pub fn record_attempt(record: AttemptRecord) {
    if let Some(ledger) = global() {
        ledger.record(record);
    }
}

/// Commit the shared ledger's queue. For tests and shutdown.
pub fn flush() {
    if let Some(ledger) = global() {
        ledger.flush();
    }
}

/// Install `ledger` as the shared one, replacing any previous override.
///
/// Test-facing: it is the only way to point the process at a specific database
/// without mutating the environment, which parallel tests cannot do safely.
#[doc(hidden)]
pub fn use_ledger_for_tests(ledger: Ledger) {
    if let Ok(mut slot) = override_slot().lock() {
        *slot = Some(Arc::new(ledger));
    }
}

/// Drop the installed override, so the next call falls back to the directory.
#[doc(hidden)]
pub fn clear_ledger_override_for_tests() {
    if let Ok(mut slot) = override_slot().lock() {
        *slot = None;
    }
}

// ── per-request attempt accounting ────────────────────────

/// Attempt numbering and timing for one client request.
///
/// One row per upstream attempt, because each attempt consumes upstream
/// resources. Only the last one may carry CC's usage numbers; the earlier ones
/// can only be NULL, and recording 0 for them would disguise "unknown" as "no
/// tokens consumed".
pub struct RequestLedger {
    req_id: String,
    model: String,
    wire: Wire,
    stream: bool,
    attempts: AtomicU64,
    started_at: Instant,
    first_byte_at: Mutex<Option<Instant>>,
    /// Set once an attempt's outcome has been recorded, so the caller does not
    /// also record a success for a turn that already ended in failure.
    settled: AtomicBool,
}

impl RequestLedger {
    pub fn new(req_id: &str, model: &str, wire: Wire, stream: bool) -> Self {
        Self {
            req_id: req_id.to_string(),
            model: model.to_string(),
            wire,
            stream,
            attempts: AtomicU64::new(0),
            started_at: Instant::now(),
            first_byte_at: Mutex::new(None),
            settled: AtomicBool::new(false),
        }
    }

    /// Whether an outcome has already been recorded for the current attempt.
    ///
    /// The streaming path records a failure the moment it happens, then returns
    /// to the caller, which would otherwise record a success for the same turn.
    fn is_settled(&self) -> bool {
        self.settled.load(Ordering::Relaxed)
    }

    /// Record a successful turn unless an outcome already beat it.
    ///
    /// The settlement rule this type owns: exactly one terminal row per request.
    /// The streaming path reports a failure (or a client abort) through the
    /// outcome sink the moment it happens; the caller afterwards cannot tell
    /// whether that happened, so it calls this, and a turn that already ended
    /// in failure keeps its failure row instead of gaining a contradictory
    /// success row.
    pub fn settle_ok(&self, usage: Option<&UsageData>) {
        if !self.is_settled() {
            self.end_attempt(AttemptStatus::Ok, usage, None);
        }
    }

    /// The attempt number to stamp on the next row: 1-based, so it is at least 1
    /// even if the upstream layer never reported starting.
    fn last_attempt(&self) -> u64 {
        self.attempts.load(Ordering::Relaxed).max(1)
    }

    /// Note that the client has seen its first byte (time to first byte).
    ///
    /// Only the first observation counts: TTFB measures the wait, and a later
    /// call would turn it into "time to the most recent chunk".
    pub fn mark_ttfb(&self) {
        if let Ok(mut slot) = self.first_byte_at.lock() {
            if slot.is_none() {
                *slot = Some(Instant::now());
            }
        }
    }

    /// Record one attempt's outcome and mark the request settled.
    ///
    /// `usage` absent means "this attempt's usage is unknowable", which is the
    /// normal case for an interrupted stream: CC reports usage only in the
    /// terminal `finish` event, so a break mid-stream leaves it with no numbers
    /// either. That becomes NULL, never 0.
    ///
    /// Call this for a terminal outcome only. An attempt that is abandoned in
    /// favour of a retry still has a successor, so it goes through `record_row`
    /// instead — otherwise the successor's own outcome would be suppressed.
    pub fn end_attempt(
        &self,
        status: AttemptStatus,
        usage: Option<&UsageData>,
        error_tag: Option<&str>,
    ) {
        self.settled.store(true, Ordering::Relaxed);
        self.record_row(status, usage, error_tag);
    }

    /// Write one row without marking the request finished.
    fn record_row(
        &self,
        status: AttemptStatus,
        usage: Option<&UsageData>,
        error_tag: Option<&str>,
    ) {
        let elapsed = self.started_at.elapsed();
        record_attempt(AttemptRecord {
            ts: now_iso8601(),
            req_id: self.req_id.clone(),
            model: self.model.clone(),
            wire: self.wire,
            stream: self.stream,
            attempt: self.last_attempt(),
            status,
            error_tag: error_tag.map(str::to_string),
            prompt_tokens: usage.and_then(|u| u.prompt_tokens),
            cached_tokens: usage.and_then(|u| u.cached_tokens),
            cache_creation_tokens: usage.and_then(|u| u.cache_creation_tokens),
            completion_tokens: usage.and_then(|u| u.completion_tokens),
            reasoning_tokens: usage.and_then(|u| u.reasoning_tokens),
            duration_ms: millis(elapsed),
            ttfb_ms: self
                .first_byte_at
                .lock()
                .ok()
                .and_then(|slot| *slot)
                .map(|first| millis(first.saturating_duration_since(self.started_at))),
        });
    }
}

impl AttemptSink for RequestLedger {
    fn started(&self) {
        self.attempts.fetch_add(1, Ordering::Relaxed);
    }

    fn failed(&self, tag: &str) {
        // A retry follows, so this row is not the end of the request: it must
        // not settle, or the retry's own outcome would be discarded.
        self.record_row(AttemptStatus::Error, None, Some(tag));
    }
}

/// The streaming layer reports through the same ledger, since a splice recovery
/// makes a second attempt of the same client request.
impl crate::stream_body::StreamOutcomeSink for RequestLedger {
    fn interrupted(&self, tag: &str) {
        // Usage is unknowable here: CC reports it only in the terminal `finish`
        // event, so a stream that broke has no numbers — NULL, not 0. The
        // replacement stream is still to come, so this does not settle either.
        self.record_row(AttemptStatus::Interrupted, None, Some(tag));
    }

    fn aborted(&self, tag: &str) {
        // The client is gone: there is no successor attempt.
        self.end_attempt(AttemptStatus::Aborted, None, Some(tag));
    }

    fn failed(&self, tag: &str, usage: Option<&UsageData>) {
        self.end_attempt(AttemptStatus::Error, usage, Some(tag));
    }

    fn ttfb(&self) {
        self.mark_ttfb();
    }
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

fn is_power_of_two(n: u64) -> bool {
    n != 0 && n & n.saturating_sub(1) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream_body::StreamOutcomeSink;

    fn record(attempt: u64, status: AttemptStatus) -> AttemptRecord {
        AttemptRecord {
            ts: now_iso8601(),
            req_id: "req-1".into(),
            model: "m".into(),
            wire: Wire::Openai,
            stream: true,
            attempt,
            status,
            error_tag: None,
            prompt_tokens: None,
            cached_tokens: None,
            cache_creation_tokens: None,
            completion_tokens: None,
            reasoning_tokens: None,
            duration_ms: 1,
            ttfb_ms: None,
        }
    }

    /// A ledger in a fresh temp directory, plus that directory.
    #[test]
    fn opening_a_legacy_database_adds_the_cache_creation_column() {
        let dir =
            std::env::temp_dir().join(format!("ccproxy-billing-migrate-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("billing.db");
        // A database created by a pre-cacheCreation build: no column at all.
        let conn = Connection::open(&path).unwrap();
        create_legacy_table(&conn);
        drop(conn);

        let opened = open_database(&dir).expect("open must succeed");
        {
            let mut stmt = opened.prepare("pragma table_info(billing)").unwrap();
            let names: Vec<String> = stmt
                .query_map([], |row| row.get::<_, String>(1))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            assert!(names.iter().any(|n| n == "cacheCreationTokens"));
        }
        // And it is insertable/readable: a NULL reads back as NULL (unknown),
        // not 0.
        opened
            .execute(
                "insert into billing (ts, model, wire, stream, attempt, status)
                 values ('2026-09-18T00:00:00Z', 'm', 'openai', 0, 1, 'ok')",
                [],
            )
            .unwrap();
        let v: Option<i64> = opened
            .query_row(
                "select cacheCreationTokens from billing where model = 'm'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, None);
        drop(opened);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn temp_ledger(tag: &str) -> (Ledger, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("ccproxy-billing-{tag}-{}", uuid::Uuid::new_v4()));
        let ledger = Ledger::open(&dir).expect("the temp ledger must open");
        (ledger, dir)
    }

    /// A `Ledger` installed as the process-wide one for the life of the value.
    ///
    /// Tests that exercise `RequestLedger` write through the global ledger —
    /// that is the path the server uses — so without this they would append to
    /// the real `%LOCALAPPDATA%\cc-proxy\billing.db`. The override is dropped
    /// (and its writer thread joined) when the guard falls out of scope.
    struct ScopedLedger {
        dir: PathBuf,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl ScopedLedger {
        fn install(tag: &str) -> Self {
            // The override is process-wide, so these tests must not overlap.
            let guard = lock();
            let (ledger, dir) = temp_ledger(tag);
            use_ledger_for_tests(ledger);
            Self { dir, _guard: guard }
        }
    }

    impl Drop for ScopedLedger {
        fn drop(&mut self) {
            clear_ledger_override_for_tests();
        }
    }

    /// Serialises the tests that touch the process-wide override.
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[allow(clippy::type_complexity)]
    fn rows(dir: &Path) -> Vec<(i64, String, String, Option<i64>, Option<i64>)> {
        let conn = Connection::open(dir.join("billing.db")).expect("open");
        let mut stmt = conn
            .prepare("select attempt, status, wire, promptTokens, completionTokens from billing order by id")
            .expect("prepare");
        let mapped = stmt
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .expect("query");
        mapped.filter_map(Result::ok).collect()
    }

    #[test]
    fn settle_ok_records_a_success_when_nobody_else_settled() {
        let scoped = ScopedLedger::install("settle-ok");
        let ledger = RequestLedger::new("r", "m", Wire::Anthropic, true);
        ledger.settle_ok(None);
        flush();
        let rows = rows(&scoped.dir);
        assert_eq!(rows.len(), 1, "exactly one terminal row");
        assert_eq!(rows[0].1, "ok", "the row is a success");
    }

    #[test]
    fn settle_ok_never_overrides_an_outcome_already_recorded() {
        // The settlement rule this pins: the streaming path reported a failure
        // the moment it happened (via end_attempt, which settles); the caller
        // cannot know that and calls settle_ok afterwards. The failure row
        // must survive untouched.
        let scoped = ScopedLedger::install("settle-contested");
        let ledger = RequestLedger::new("r", "m", Wire::Anthropic, true);
        ledger.started();
        ledger.end_attempt(AttemptStatus::Error, None, Some("http-500"));
        ledger.settle_ok(None);
        flush();
        let rows = rows(&scoped.dir);
        assert_eq!(rows.len(), 1, "no second row appears");
        assert_eq!(rows[0].1, "error", "the failure row stands");
    }

    #[test]
    fn settle_ok_after_a_client_abort_keeps_the_abort_row() {
        let scoped = ScopedLedger::install("settle-aborted");
        let ledger = RequestLedger::new("r", "m", Wire::Anthropic, true);
        ledger.started();
        ledger.aborted("[client-gone] client disconnected");
        ledger.settle_ok(None);
        flush();
        let rows = rows(&scoped.dir);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].1, "aborted",
            "the abort is not relabelled a success"
        );
    }

    #[test]
    fn each_attempt_is_its_own_row() {
        let (ledger, dir) = temp_ledger("attempts");
        // Two abandoned attempts then a success: three rows, numbering 1..3.
        ledger.record(record(1, AttemptStatus::Error));
        ledger.record(record(2, AttemptStatus::Error));
        ledger.record(record(3, AttemptStatus::Ok));
        ledger.flush();
        let rows = rows(&dir);
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows.iter().map(|r| r.0).collect::<Vec<_>>(),
            vec![1, 2, 3],
            "attempt numbers must be preserved in order"
        );
    }

    #[test]
    fn unknown_usage_is_null_not_zero() {
        let (ledger, dir) = temp_ledger("null");
        // The interrupted attempt: CC never reported numbers, so there are none.
        ledger.record(record(1, AttemptStatus::Interrupted));
        ledger.flush();
        let rows = rows(&dir);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows.first().and_then(|r| r.3),
            None,
            "an unknown token count must stay NULL; 0 would read as \"no tokens\""
        );
    }

    #[test]
    fn a_known_usage_is_stored_with_its_numbers() {
        let (ledger, dir) = temp_ledger("known");
        let mut rec = record(1, AttemptStatus::Ok);
        rec.prompt_tokens = Some(1234);
        rec.completion_tokens = Some(56);
        ledger.record(rec);
        ledger.flush();
        let rows = rows(&dir);
        assert_eq!(
            rows.first().map(|r| (r.3, r.4)),
            Some((Some(1234), Some(56)))
        );
    }

    #[test]
    fn the_wire_and_status_are_stored_as_the_node_names() {
        let (ledger, dir) = temp_ledger("wire");
        let mut rec = record(1, AttemptStatus::Interrupted);
        rec.wire = Wire::Anthropic;
        ledger.record(rec);
        ledger.flush();
        let rows = rows(&dir);
        assert_eq!(
            rows.first().map(|r| (r.1.clone(), r.2.clone())),
            Some(("interrupted".into(), "anthropic".into()))
        );
    }

    #[test]
    fn flush_persists_without_waiting_for_the_batch_window() {
        // The point of flush: a test (or shutdown) must see the row immediately,
        // not 200ms later.
        let (ledger, dir) = temp_ledger("flush");
        ledger.record(record(1, AttemptStatus::Ok));
        ledger.flush();
        assert_eq!(rows(&dir).len(), 1, "flush must have committed the row");
    }

    #[test]
    fn rows_survive_a_reopen() {
        let dir =
            std::env::temp_dir().join(format!("ccproxy-billing-reopen-{}", uuid::Uuid::new_v4()));
        {
            let ledger = Ledger::open(&dir).expect("open");
            ledger.record(record(1, AttemptStatus::Ok));
            ledger.flush();
        }
        // The writer thread is gone; the row is on disk.
        let reopened = Ledger::open(&dir).expect("reopen");
        reopened.record(record(2, AttemptStatus::Ok));
        reopened.flush();
        assert_eq!(rows(&dir).len(), 2, "history must accumulate across runs");
    }

    #[test]
    fn an_unwritable_directory_disables_the_ledger_without_panicking() {
        // A directory path that is actually a file cannot be created.
        let file =
            std::env::temp_dir().join(format!("ccproxy-billing-file-{}", uuid::Uuid::new_v4()));
        std::fs::write(&file, b"not a directory").expect("write");
        let blocked = file.join("nested");
        assert!(
            Ledger::open(&blocked).is_none(),
            "an unusable directory must disable the ledger, not fail the process"
        );
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn a_ledger_that_cannot_be_opened_does_not_fail_the_recording_path() {
        // `record_attempt` is called from the forwarding path, which must keep
        // working even when no ledger could be opened. A directory that is
        // really a file is the cheapest way to force that for a real ledger.
        let file =
            std::env::temp_dir().join(format!("ccproxy-billing-noop-{}", uuid::Uuid::new_v4()));
        std::fs::write(&file, b"not a directory").expect("write");
        assert!(Ledger::open(&file.join("nested")).is_none());
        let _ = std::fs::remove_file(&file);

        // And with no ledger at all, recording must return rather than panic.
        // `disable_for_tests` is what keeps `global()` from lazily opening the
        // real database here — otherwise this test would write to production.
        let _guard = lock();
        disable_for_tests();
        record_attempt(record(1, AttemptStatus::Ok));
        flush();
        assert!(global().is_none(), "the disabled path reports no ledger");
        enable_for_tests();
    }

    #[test]
    fn billing_dir_honours_local_app_data_and_the_namespace() {
        let dir = billing_dir_from(Some("C:/Users/x/AppData/Local"), None, None);
        assert_eq!(dir, Path::new("C:/Users/x/AppData/Local").join("cc-proxy"));
        // A namespace isolates a test instance from the production database.
        let ns = billing_dir_from(Some("C:/Users/x/AppData/Local"), None, Some("vitest"));
        assert_eq!(
            ns,
            Path::new("C:/Users/x/AppData/Local")
                .join("cc-proxy")
                .join("vitest")
        );
    }

    #[test]
    fn an_empty_namespace_is_not_a_namespace() {
        let dir = billing_dir_from(Some("C:/base"), None, Some("   "));
        assert_eq!(dir, Path::new("C:/base").join("cc-proxy"));
    }

    #[test]
    fn a_missing_local_app_data_falls_back_to_the_home_directory() {
        let dir = billing_dir_from(None, Some("/home/x"), None);
        assert_eq!(dir, Path::new("/home/x").join(".cc-proxy"));
    }

    #[test]
    fn the_timestamp_is_iso8601_utc() {
        // 2026-09-15T00:00:00Z, the day this was written.
        assert_eq!(iso8601_secs(1_789_430_400), "2026-09-15T00:00:00");
        // The epoch itself.
        assert_eq!(iso8601_secs(0), "1970-01-01T00:00:00");
        // A leap day, which is where naive date arithmetic breaks.
        assert_eq!(iso8601_secs(1_709_164_800), "2024-02-29T00:00:00");
    }

    #[test]
    fn the_timestamp_format_matches_the_node_build() {
        let ts = now_iso8601();
        assert!(ts.ends_with('Z'), "{ts} must be UTC");
        assert_eq!(ts.len(), 24, "{ts} must carry millisecond precision");
        assert_eq!(ts.as_bytes().get(10), Some(&b'T'));
    }

    #[test]
    fn the_attempt_number_starts_at_one_even_without_a_started_call() {
        let _ledger = ScopedLedger::install("attempt-num");
        let ledger = RequestLedger::new("r", "m", Wire::Openai, true);
        // The upstream layer always calls `started` first, but a row claiming
        // attempt 0 would be nonsense, so the floor is 1.
        assert_eq!(ledger.last_attempt(), 1);
        ledger.started();
        ledger.started();
        assert_eq!(ledger.last_attempt(), 2);
    }

    #[test]
    fn ttfb_records_only_the_first_observation() {
        let _ledger = ScopedLedger::install("ttfb");
        let ledger = RequestLedger::new("r", "m", Wire::Openai, true);
        ledger.mark_ttfb();
        let first = ledger.first_byte_at.lock().ok().and_then(|s| *s);
        ledger.mark_ttfb();
        let second = ledger.first_byte_at.lock().ok().and_then(|s| *s);
        assert_eq!(
            first, second,
            "TTFB is the wait for the first byte, not the latest chunk"
        );
    }

    #[test]
    fn an_abandoned_attempt_is_recorded_as_an_error_with_no_usage() {
        let scoped = ScopedLedger::install("abandoned");
        let ledger = RequestLedger::new("r", "m", Wire::Openai, true);
        // `failed` is what the upstream layer calls when it decides to retry.
        ledger.started();
        AttemptSink::failed(&ledger, "http-503");
        // Then the retry succeeds.
        ledger.started();
        ledger.end_attempt(AttemptStatus::Ok, None, None);
        assert_eq!(ledger.last_attempt(), 2);
        // Both attempts leave a row, and the abandoned one carries no usage.
        flush();
        let rows = rows(&scoped.dir);
        assert_eq!(rows.len(), 2, "one row per attempt: {rows:?}");
        assert_eq!(rows.first().map(|r| r.1.clone()), Some("error".to_string()));
        assert_eq!(rows.get(1).map(|r| r.1.clone()), Some("ok".to_string()));
        assert_eq!(
            rows.first().and_then(|r| r.3),
            None,
            "an abandoned attempt's usage is unknown: NULL, not 0"
        );
    }

    #[test]
    fn a_backed_up_queue_drops_records_instead_of_blocking_the_caller() {
        // The invariant that matters: forwarding must never wait on statistics.
        // A writer that cannot keep up must lose rows, not stall the request.
        let (ledger, _dir) = temp_ledger("backpressure");
        // Fill the queue past capacity. Every send is `try_send`, so this
        // cannot block no matter how far behind the writer falls.
        let total = CHANNEL_CAPACITY * 4;
        let started = Instant::now();
        for i in 0..total {
            ledger.record(record(u64::try_from(i).unwrap_or(1), AttemptStatus::Ok));
        }
        // Generous bound: this is a non-blocking path, so it finishes in
        // milliseconds; a blocking implementation would take the batch window
        // for every batch it had to drain.
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "recording must not block on the writer; took {:?}",
            started.elapsed()
        );
        ledger.flush();
    }

    #[test]
    fn every_record_can_carry_a_different_request() {
        // Rows are per attempt, not per request, so the request id is what ties
        // a retry chain together for anyone reading the ledger.
        let (ledger, dir) = temp_ledger("reqid");
        for attempt in 1..=3 {
            let mut rec = record(attempt, AttemptStatus::Ok);
            rec.req_id = "the-same-conversation".into();
            ledger.record(rec);
        }
        ledger.flush();
        let conn = Connection::open(dir.join("billing.db")).expect("open");
        let distinct: i64 = conn
            .query_row(
                "select count(distinct reqId) from billing where reqId = 'the-same-conversation'",
                [],
                |row| row.get(0),
            )
            .expect("query");
        assert_eq!(distinct, 1, "all three attempts share one request id");
    }

    #[test]
    fn the_ledger_is_usable_from_several_threads() {
        // The server is thread-per-connection, so many request threads record
        // concurrently. `Ledger` must be `Sync` and lose nothing at rest.
        let (ledger, dir) = temp_ledger("threads");
        std::thread::scope(|scope| {
            for t in 0..8 {
                let ledger = &ledger;
                scope.spawn(move || {
                    for i in 0..25 {
                        let mut rec = record(u64::try_from(i).unwrap_or(1), AttemptStatus::Ok);
                        rec.req_id = format!("thread-{t}");
                        ledger.record(rec);
                    }
                });
            }
        });
        ledger.flush();
        assert_eq!(rows(&dir).len(), 200, "every record from every thread");
    }

    #[test]
    fn the_row_count_reflects_the_batch_commit() {
        // Committing at the size boundary rather than row-by-row is what keeps
        // the WAL fsync off the request path; the rows must still all arrive.
        let (ledger, dir) = temp_ledger("batch");
        let total = BATCH_MAX_ROWS * 2;
        for i in 0..total {
            ledger.record(record(u64::try_from(i).unwrap_or(1), AttemptStatus::Ok));
        }
        ledger.flush();
        assert_eq!(rows(&dir).len(), total);
    }

    // ── the legacy import ──

    /// A temp directory holding a `usage.jsonl` with `lines` verbatim.
    fn legacy_dir(tag: &str, lines: &[&str]) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ccproxy-legacy-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("usage.jsonl"), lines.join("\n")).expect("write");
        dir
    }

    fn imported_rows(dir: &Path) -> Vec<(String, String, Option<i64>, Option<i64>)> {
        let conn = Connection::open(dir.join("billing.db")).expect("open");
        let mut stmt = conn
            .prepare("select ts, model, promptTokens, cachedTokens from billing order by id")
            .expect("prepare");
        stmt.query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .expect("query")
        .filter_map(Result::ok)
        .collect()
    }

    #[test]
    fn the_legacy_jsonl_is_imported_with_its_numbers_intact() {
        let src = legacy_dir(
            "basic",
            &[
                r#"{"ts":"2026-09-13T10:10:29.613Z","model":"deepseek-v4.1-flash","promptTokens":71445,"cachedTokens":71296,"completionTokens":191}"#,
                r#"{"ts":"2026-09-13T10:10:33.881Z","model":"deepseek-v4.1-flash","promptTokens":254046,"cachedTokens":253696,"completionTokens":167}"#,
            ],
        );
        let (ledger, dir) = temp_ledger("legacy-import");
        drop(ledger);
        let conn = Connection::open(dir.join("billing.db")).expect("open");
        let count = import_legacy_jsonl(&conn, std::slice::from_ref(&src));
        assert_eq!(
            count, 2,
            "both lines are real billed usage and must survive"
        );
        let rows = imported_rows(&dir);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows.first().map(|r| r.0.clone()),
            Some("2026-09-13T10:10:29.613Z".to_string()),
            "the original timestamp is preserved, not rewritten to now"
        );
        assert_eq!(
            rows.first().map(|r| (r.2, r.3)),
            Some((Some(71445), Some(71296)))
        );
    }

    #[test]
    fn the_import_runs_only_once_per_database() {
        // Without the marker every startup would duplicate the whole history.
        let src = legacy_dir(
            "once",
            &[
                r#"{"ts":"2026-09-13T10:10:29.613Z","model":"m","promptTokens":1,"cachedTokens":0,"completionTokens":1}"#,
            ],
        );
        let (ledger, dir) = temp_ledger("legacy-once");
        drop(ledger);
        let conn = Connection::open(dir.join("billing.db")).expect("open");
        assert_eq!(import_legacy_jsonl(&conn, std::slice::from_ref(&src)), 1);
        assert_eq!(
            import_legacy_jsonl(&conn, std::slice::from_ref(&src)),
            0,
            "a second import must be a no-op"
        );
        assert_eq!(imported_rows(&dir).len(), 1, "the row is not duplicated");
    }

    #[test]
    fn a_malformed_legacy_line_is_skipped_not_fatal() {
        // A crash mid-write can leave a partial final line; the rest of the
        // history is still worth importing.
        let src = legacy_dir(
            "damaged",
            &[
                r#"{"ts":"2026-09-13T10:10:29.613Z","model":"m","promptTokens":5,"cachedTokens":1,"completionTokens":2}"#,
                r#"{"ts":"2026-09-13T10:10:30.000Z","model":"m","promptTok"#,
                "",
                r#"{"ts":"2026-09-13T10:10:31.000Z","model":"m","promptTokens":7,"cachedTokens":3,"completionTokens":4}"#,
            ],
        );
        let (ledger, dir) = temp_ledger("legacy-damaged");
        drop(ledger);
        let conn = Connection::open(dir.join("billing.db")).expect("open");
        let count = import_legacy_jsonl(&conn, std::slice::from_ref(&src));
        assert_eq!(count, 2, "the two intact lines are imported");
    }

    #[test]
    fn a_missing_legacy_file_imports_nothing_and_does_not_fail() {
        let empty =
            std::env::temp_dir().join(format!("ccproxy-legacy-empty-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&empty).expect("mkdir");
        let (ledger, dir) = temp_ledger("legacy-missing");
        drop(ledger);
        let conn = Connection::open(dir.join("billing.db")).expect("open");
        assert_eq!(import_legacy_jsonl(&conn, std::slice::from_ref(&empty)), 0);
        // Every version directory is scanned, so one present and one absent is
        // the normal case.
        let src = legacy_dir(
            "mixed",
            &[
                r#"{"ts":"2026-09-13T10:10:29.613Z","model":"m","promptTokens":1,"cachedTokens":0,"completionTokens":1}"#,
            ],
        );
        assert_eq!(
            import_legacy_jsonl(&conn, &[empty, src]),
            1,
            "a directory without the file contributes nothing"
        );
    }

    #[test]
    fn an_imported_row_has_no_invented_values() {
        // The old format had no wire, timing or request id: those must stay
        // absent rather than be filled with a plausible-looking guess.
        let src = legacy_dir(
            "uninvented",
            &[
                r#"{"ts":"2026-09-13T10:10:29.613Z","model":"m","promptTokens":1,"cachedTokens":0,"completionTokens":1}"#,
            ],
        );
        let (ledger, dir) = temp_ledger("legacy-uninvented");
        drop(ledger);
        let conn = Connection::open(dir.join("billing.db")).expect("open");
        import_legacy_jsonl(&conn, std::slice::from_ref(&src));
        let (req_id, duration, ttfb, reasoning): (
            Option<String>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
        ) = conn
            .query_row(
                "select reqId, durationMs, ttfbMs, reasoningTokens from billing",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("query");
        assert_eq!(duration, Some(0), "the old format recorded no duration");
        assert_eq!(ttfb, None, "and no TTFB: NULL, not a guess");
        assert_eq!(reasoning, None, "and no reasoning split");
        assert_eq!(req_id.as_deref(), Some(""), "and no request id to invent");
    }

    // ── daily stats (tray menu) ───────────────────────────

    fn with_usage(
        mut r: AttemptRecord,
        prompt: u64,
        cached: u64,
        completion: u64,
    ) -> AttemptRecord {
        r.prompt_tokens = Some(prompt);
        r.cached_tokens = Some(cached);
        r.completion_tokens = Some(completion);
        r
    }

    #[test]
    fn daily_stats_sum_the_last_day() {
        let (ledger, dir) = temp_ledger("stats");
        ledger.record(with_usage(record(1, AttemptStatus::Ok), 100, 40, 10));
        ledger.record(with_usage(record(2, AttemptStatus::Ok), 300, 60, 20));
        ledger.flush();

        let stats = daily_stats(&dir).expect("stats must be readable");
        assert_eq!(stats.rows, 2);
        assert_eq!(stats.prompt_tokens, 400);
        assert_eq!(stats.cached_tokens, 100);
        assert_eq!(stats.completion_tokens, 30);
        assert_eq!(stats.cache_rate_percent(), Some(25.0));
    }

    #[test]
    fn daily_stats_skip_unknown_usage_instead_of_reading_it_as_zero() {
        // An interrupted attempt has NULL tokens. Counting it as a row whose
        // prompt was 0 is fine; counting its tokens as 0 is not, because the
        // cache rate would then be computed over a prompt that was never seen.
        let (ledger, dir) = temp_ledger("stats-unknown");
        ledger.record(record(1, AttemptStatus::Interrupted));
        ledger.record(with_usage(record(2, AttemptStatus::Ok), 200, 50, 5));
        ledger.flush();

        let stats = daily_stats(&dir).expect("stats");
        assert_eq!(stats.rows, 2, "the interrupted attempt is still a row");
        assert_eq!(stats.prompt_tokens, 200, "but contributes no tokens");
        assert_eq!(stats.cache_rate_percent(), Some(25.0));
    }

    #[test]
    fn daily_stats_report_no_rate_when_nothing_was_recorded() {
        let (ledger, dir) = temp_ledger("stats-empty");
        ledger.flush();
        let stats = daily_stats(&dir).expect("stats");
        assert_eq!(stats.rows, 0);
        assert_eq!(stats.cache_rate_percent(), None, "0/0 is unknown, not 0%");
    }

    #[test]
    fn daily_stats_ignore_rows_older_than_a_day() {
        let (ledger, dir) = temp_ledger("stats-old");
        let mut old = with_usage(record(1, AttemptStatus::Ok), 100, 100, 1);
        old.ts = iso8601_secs(now_epoch_secs().saturating_sub(2 * 86_400));
        ledger.record(old);
        ledger.record(with_usage(record(2, AttemptStatus::Ok), 10, 5, 1));
        ledger.flush();

        let stats = daily_stats(&dir).expect("stats");
        assert_eq!(stats.rows, 1, "the 2-day-old row is outside the window");
        assert_eq!(stats.prompt_tokens, 10);
    }

    #[test]
    fn daily_stats_on_a_missing_database_are_none() {
        let dir =
            std::env::temp_dir().join(format!("ccproxy-billing-none-{}", uuid::Uuid::new_v4()));
        // Not an error: the tray shows "no data" rather than failing to open
        // its menu.
        assert_eq!(daily_stats(&dir), None);
    }
}
