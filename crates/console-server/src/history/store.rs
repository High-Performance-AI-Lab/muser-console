//! The rolling history store: one sqlite file, one writer thread.
//!
//! The async side never touches the connection. Sampler tasks push batches
//! down a `std::sync::mpsc` channel; the owning thread applies them in a
//! transaction, and answers queries and maintenance passes on the same
//! channel so reads and writes can never interleave badly.
//!
//! Nothing in here invents a value. Downsampling replaces a run of real
//! samples with their aggregate over a fixed bucket (gauge mean, counter
//! last); retention deletes. No pass ever fills a gap.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Mutex;

use rusqlite::{Connection, OpenFlags};

use crate::config::HistoryConfig;
use crate::history::catalog::{self, Honesty, Kind, Source};
use crate::logging::log;

/// Downsample bucket for rows past the raw-retention window.
pub const BUCKET_MS: i64 = 60_000;

/// Raw 1 s samples are kept for this long; older rows collapse to `BUCKET_MS`.
pub const RAW_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;

/// Native store resolution. A query `step_s` at or below this returns the
/// stored points untouched — no re-aggregation of already-native data.
pub const NATIVE_STEP_S: i64 = 1;

/// Hard cap on points returned per series; the caller is told to raise
/// `step_s` rather than being handed a truncated (and so misleading) range.
pub const MAX_POINTS_PER_SERIES: i64 = 20_000;

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS samples (
    instance TEXT NOT NULL,
    series TEXT NOT NULL,
    ts INTEGER NOT NULL,
    value REAL NOT NULL,
    source TEXT NOT NULL,
    honesty TEXT NOT NULL,
    PRIMARY KEY (instance, series, ts))";

/// One stored measurement.
#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    pub instance: String,
    pub series: &'static str,
    pub ts_ms: i64,
    pub value: f64,
    pub source: Source,
    pub honesty: Honesty,
}

/// A range read. `series` names are catalog-validated by the caller.
#[derive(Clone, Debug)]
pub struct Query {
    pub instance: String,
    pub series: Vec<&'static str>,
    /// Inclusive lower bound, unix ms.
    pub from_ms: i64,
    /// Inclusive upper bound, unix ms.
    pub to_ms: i64,
    pub step_s: i64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SeriesData {
    pub points: Vec<(i64, f64)>,
    /// Honesty tag of the newest row in range. `None` when the range holds
    /// no rows — the store makes no claim about data it does not have.
    pub honesty: Option<Honesty>,
}

pub type QueryResult = Vec<(&'static str, SeriesData)>;

/// What one maintenance pass did. Returned so operators (and tests) can see
/// the store shrink instead of taking it on faith.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Maintenance {
    pub buckets_rewritten: usize,
    pub rows_collapsed: usize,
    pub rows_expired: usize,
}

#[derive(Debug)]
pub enum StoreError {
    Sqlite(String),
    /// The writer thread is gone (shutdown, or it died).
    Closed,
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Sqlite(message) => write!(formatter, "history store: {message}"),
            StoreError::Closed => write!(formatter, "history store is closed"),
        }
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> StoreError {
        StoreError::Sqlite(error.to_string())
    }
}

enum Command {
    Write(Vec<Sample>),
    Query(
        Query,
        tokio::sync::oneshot::Sender<Result<QueryResult, StoreError>>,
    ),
    Maintain(
        i64,
        u64,
        tokio::sync::oneshot::Sender<Result<Maintenance, StoreError>>,
    ),
}

/// Handle to the store. Cloneable through the `Arc` the server holds; the
/// writer thread stops when the last handle drops.
pub struct HistoryStore {
    commands: Mutex<Sender<Command>>,
    path: PathBuf,
    retention_days: u64,
}

impl HistoryStore {
    /// Opens (creating if needed) the store and starts its writer thread.
    pub fn open(config: &HistoryConfig) -> Result<HistoryStore, String> {
        if let Some(parent) = config.db_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| format!("create {}: {error}", parent.display()))?;
            }
        }
        // Create the file ourselves, at 0600, before SQLite can create it
        // at whatever the umask allows: chmod-after-open would leave the
        // fleet's telemetry world-readable for the width of that window.
        precreate_private(&config.db_path)?;
        let connection = Connection::open_with_flags(
            &config.db_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|error| format!("open history store {}: {error}", config.db_path.display()))?;
        // The store holds the fleet's telemetry; it is as sensitive as the
        // console config that points at it. (Also narrows a store that an
        // earlier run left with looser permissions.)
        restrict_permissions(&config.db_path)?;
        configure(&connection).map_err(|error| {
            format!(
                "prepare history store {}: {error}",
                config.db_path.display()
            )
        })?;

        let (sender, receiver) = std::sync::mpsc::channel();
        let path = config.db_path.clone();
        std::thread::Builder::new()
            .name("history-writer".to_owned())
            .spawn(move || writer_loop(connection, receiver))
            .map_err(|error| format!("start history writer thread: {error}"))?;

        Ok(HistoryStore {
            commands: Mutex::new(sender),
            path,
            retention_days: config.retention_days,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Hands a tick's samples to the writer. Fire-and-forget: a sampler tick
    /// must never block on disk.
    pub fn write(&self, samples: Vec<Sample>) {
        if samples.is_empty() {
            return;
        }
        self.send(Command::Write(samples));
    }

    pub async fn query(&self, request: Query) -> Result<QueryResult, StoreError> {
        let (reply, answer) = tokio::sync::oneshot::channel();
        if !self.send(Command::Query(request, reply)) {
            return Err(StoreError::Closed);
        }
        answer.await.unwrap_or(Err(StoreError::Closed))
    }

    /// Runs one downsample + retention pass. `now_ms` is passed in so the
    /// pass is deterministic under test.
    pub async fn maintain_at(&self, now_ms: i64) -> Result<Maintenance, StoreError> {
        let (reply, answer) = tokio::sync::oneshot::channel();
        if !self.send(Command::Maintain(now_ms, self.retention_days, reply)) {
            return Err(StoreError::Closed);
        }
        answer.await.unwrap_or(Err(StoreError::Closed))
    }

    fn send(&self, command: Command) -> bool {
        let sender = self
            .commands
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        sender.send(command).is_ok()
    }
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("chmod history store {}: {error}", path.display()))
}

/// Creates the store file at 0600 if it does not exist yet. An existing
/// file is left alone here — `restrict_permissions` narrows it after open.
#[cfg(unix)]
fn precreate_private(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt as _;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(format!("create history store {}: {error}", path.display())),
    }
}

#[cfg(not(unix))]
fn precreate_private(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn configure(connection: &Connection) -> rusqlite::Result<()> {
    // WAL keeps the hourly maintenance pass from blocking sampler writes;
    // busy_timeout covers an operator poking the file with sqlite3(1).
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    connection.pragma_update(None, "busy_timeout", 5_000)?;
    connection.execute_batch(SCHEMA)?;
    Ok(())
}

fn writer_loop(connection: Connection, receiver: Receiver<Command>) {
    // One log line per failure state change, not per tick: a store that is
    // unhappy for an hour must not drown the operator's terminal.
    let mut last_failure: Option<String> = None;
    while let Ok(command) = receiver.recv() {
        match command {
            Command::Write(samples) => {
                let outcome = write_batch(&connection, &samples);
                note_failure(&mut last_failure, outcome.err().map(|e| e.to_string()));
            }
            Command::Query(request, reply) => {
                let _ = reply.send(read_range(&connection, &request));
            }
            Command::Maintain(now_ms, retention_days, reply) => {
                let _ = reply.send(maintain(&connection, now_ms, retention_days));
            }
        }
    }
}

fn note_failure(previous: &mut Option<String>, current: Option<String>) {
    if *previous == current {
        return;
    }
    if let Some(message) = &current {
        log(&format!("history store write failed: {message}"));
    } else if previous.is_some() {
        log("history store writes recovered");
    }
    *previous = current;
}

fn write_batch(connection: &Connection, samples: &[Sample]) -> Result<(), StoreError> {
    let transaction = connection.unchecked_transaction()?;
    {
        // INSERT OR REPLACE: a re-sample landing on a millisecond already
        // stored overwrites it rather than failing the whole batch.
        let mut statement = transaction.prepare_cached(
            "INSERT OR REPLACE INTO samples (instance, series, ts, value, source, honesty)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for sample in samples {
            statement.execute(rusqlite::params![
                sample.instance,
                sample.series,
                sample.ts_ms,
                sample.value,
                sample.source.as_str(),
                sample.honesty.as_str(),
            ])?;
        }
    }
    transaction.commit()?;
    Ok(())
}

fn read_range(connection: &Connection, request: &Query) -> Result<QueryResult, StoreError> {
    let mut statement = connection.prepare_cached(
        "SELECT ts, value, honesty FROM samples
         WHERE instance = ?1 AND series = ?2 AND ts >= ?3 AND ts <= ?4
         ORDER BY ts",
    )?;
    let mut result = QueryResult::new();
    for name in &request.series {
        let kind = catalog::lookup(name).map_or(Kind::Gauge, |series| series.kind);
        let rows = statement.query_map(
            rusqlite::params![request.instance, name, request.from_ms, request.to_ms],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, f64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?;
        let mut points: Vec<(i64, f64)> = Vec::new();
        let mut honesty: Option<Honesty> = None;
        for row in rows {
            let (ts, value, tag) = row?;
            points.push((ts, value));
            // Rows arrive oldest-first, so the last one wins: the tag most
            // recently stored for this series.
            honesty = Honesty::from_tag(&tag);
        }
        if request.step_s > NATIVE_STEP_S {
            points = bucket(&points, request.step_s * 1000, kind);
        }
        result.push((*name, SeriesData { points, honesty }));
    }
    Ok(result)
}

/// Collapses `points` (ordered by ts) into epoch-anchored buckets of
/// `width_ms`. Gauges average, counters keep the bucket's last value. Empty
/// buckets stay empty — a bucket with no samples yields no point, so a gap
/// in the store stays a gap in the answer.
fn bucket(points: &[(i64, f64)], width_ms: i64, kind: Kind) -> Vec<(i64, f64)> {
    let mut out: Vec<(i64, f64)> = Vec::new();
    let mut index = 0usize;
    while index < points.len() {
        let start = points[index].0.div_euclid(width_ms) * width_ms;
        let mut end = index;
        while end < points.len() && points[end].0.div_euclid(width_ms) * width_ms == start {
            end += 1;
        }
        let slice = &points[index..end];
        let value = match kind {
            Kind::Gauge => slice.iter().map(|(_, value)| value).sum::<f64>() / slice.len() as f64,
            Kind::Counter => slice[slice.len() - 1].1,
        };
        out.push((start, value));
        index = end;
    }
    out
}

fn maintain(
    connection: &Connection,
    now_ms: i64,
    retention_days: u64,
) -> Result<Maintenance, StoreError> {
    let mut report = Maintenance::default();

    // Retention first: expired rows need no downsampling. Saturating all
    // the way through, so an absurd retention_days keeps everything instead
    // of wrapping into a cutoff that deletes everything.
    let retention_ms = i64::try_from(retention_days)
        .unwrap_or(i64::MAX)
        .saturating_mul(24 * 60 * 60 * 1000);
    let expiry = now_ms.saturating_sub(retention_ms);
    report.rows_expired = connection.execute(
        "DELETE FROM samples WHERE ts < ?1",
        rusqlite::params![expiry],
    )?;

    // Align the cutoff down to a bucket boundary so every bucket the pass
    // touches is complete: a half-collapsed bucket would get averaged again
    // on the next pass and quietly distort its own value.
    let cutoff = (now_ms - RAW_WINDOW_MS).div_euclid(BUCKET_MS) * BUCKET_MS;
    // This grouping walks every row below the cutoff, most of which are
    // already collapsed and skipped below. That is a once-an-hour index
    // scan on the writer thread; sampler batches queue behind it and are
    // written afterwards with the timestamps they were taken at, so a slow
    // pass costs latency, never data.
    let candidates = {
        // Exactly one max() aggregate, so the bare `value`, `source` and
        // `honesty` columns come from the bucket's newest row (documented
        // sqlite behaviour) — that is the counter's "last" and the tag most
        // recently stored.
        let mut statement = connection.prepare_cached(
            "SELECT instance, series, (ts / ?1) * ?1 AS bucket, COUNT(*), MAX(ts),
                    AVG(value), value, source, honesty
             FROM samples WHERE ts < ?2
             GROUP BY instance, series, bucket",
        )?;
        let rows = statement.query_map(rusqlite::params![BUCKET_MS, cutoff], |row| {
            Ok(Candidate {
                instance: row.get(0)?,
                series: row.get(1)?,
                bucket: row.get(2)?,
                count: row.get(3)?,
                newest_ts: row.get(4)?,
                mean: row.get(5)?,
                last: row.get(6)?,
                source: row.get(7)?,
                honesty: row.get(8)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<Candidate>>>()?
    };

    let transaction = connection.unchecked_transaction()?;
    for candidate in &candidates {
        // Already a single aligned row: collapsing it again would be a
        // no-op write. Keeps repeated passes idempotent and cheap.
        if candidate.count == 1 && candidate.newest_ts == candidate.bucket {
            continue;
        }
        let kind = catalog::lookup(&candidate.series).map_or(Kind::Gauge, |series| series.kind);
        let value = match kind {
            Kind::Gauge => candidate.mean,
            Kind::Counter => candidate.last,
        };
        report.rows_collapsed += transaction.execute(
            "DELETE FROM samples
             WHERE instance = ?1 AND series = ?2 AND ts >= ?3 AND ts < ?4",
            rusqlite::params![
                candidate.instance,
                candidate.series,
                candidate.bucket,
                candidate.bucket + BUCKET_MS
            ],
        )?;
        transaction.execute(
            "INSERT OR REPLACE INTO samples (instance, series, ts, value, source, honesty)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                candidate.instance,
                candidate.series,
                candidate.bucket,
                value,
                candidate.source,
                candidate.honesty
            ],
        )?;
        report.buckets_rewritten += 1;
    }
    transaction.commit()?;
    Ok(report)
}

struct Candidate {
    instance: String,
    series: String,
    bucket: i64,
    count: i64,
    newest_ts: i64,
    mean: f64,
    last: f64,
    source: String,
    honesty: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Timestamps and 1.0/2.0/3.0 values here are structural: these tests
    /// exercise bucket arithmetic and row lifetimes, and none of it is ever
    /// rendered. The engine-value paths are covered end to end against real
    /// fixture numbers in tests/history.rs.
    fn memory_store() -> Connection {
        let connection = Connection::open_in_memory().expect("in-memory sqlite");
        connection.execute_batch(SCHEMA).expect("schema");
        connection
    }

    fn put(connection: &Connection, series: &'static str, ts_ms: i64, value: f64) {
        write_batch(
            connection,
            &[Sample {
                instance: "gx".to_owned(),
                series,
                ts_ms,
                value,
                source: Source::Metrics,
                honesty: Honesty::Measured,
            }],
        )
        .expect("write");
    }

    fn rows(connection: &Connection, series: &str) -> Vec<(i64, f64)> {
        let mut statement = connection
            .prepare("SELECT ts, value FROM samples WHERE series = ?1 ORDER BY ts")
            .expect("prepare");
        let rows = statement
            .query_map(rusqlite::params![series], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .expect("query");
        rows.map(|row| row.expect("row")).collect()
    }

    #[test]
    fn downsample_averages_gauges_and_keeps_counter_last() {
        let connection = memory_store();
        let now = 100 * RAW_WINDOW_MS;
        let old_bucket = (now - RAW_WINDOW_MS - 5 * BUCKET_MS).div_euclid(BUCKET_MS) * BUCKET_MS;
        for (offset, value) in [(0, 1.0), (1_000, 2.0), (2_000, 3.0)] {
            put(&connection, "decode_tok_s", old_bucket + offset, value);
            put(
                &connection,
                "completion_tokens_total",
                old_bucket + offset,
                value,
            );
        }
        // A fresh sample inside the raw window must survive untouched.
        put(&connection, "decode_tok_s", now - 1_000, 9.0);

        let report = maintain(&connection, now, 7).expect("maintain");
        assert_eq!(report.buckets_rewritten, 2);
        assert_eq!(report.rows_collapsed, 6);
        assert_eq!(report.rows_expired, 0);

        assert_eq!(
            rows(&connection, "decode_tok_s"),
            [(old_bucket, 2.0), (now - 1_000, 9.0)],
            "gauge collapses to the bucket mean; the raw-window row is untouched"
        );
        assert_eq!(
            rows(&connection, "completion_tokens_total"),
            [(old_bucket, 3.0)],
            "a cumulative counter keeps the bucket's last value, never a mean"
        );
    }

    #[test]
    fn downsample_is_idempotent() {
        let connection = memory_store();
        let now = 100 * RAW_WINDOW_MS;
        let old_bucket = (now - RAW_WINDOW_MS - 5 * BUCKET_MS).div_euclid(BUCKET_MS) * BUCKET_MS;
        for (offset, value) in [(0, 1.0), (1_000, 2.0), (2_000, 3.0)] {
            put(&connection, "decode_tok_s", old_bucket + offset, value);
        }
        maintain(&connection, now, 7).expect("first pass");
        let before = rows(&connection, "decode_tok_s");
        let report = maintain(&connection, now, 7).expect("second pass");
        assert_eq!(
            report,
            Maintenance::default(),
            "a second pass over aligned buckets must do nothing"
        );
        assert_eq!(rows(&connection, "decode_tok_s"), before);
    }

    #[test]
    fn downsample_leaves_partial_buckets_at_the_cutoff_alone() {
        let connection = memory_store();
        let now = 100 * RAW_WINDOW_MS;
        // The bucket straddling the raw-window cutoff must not be collapsed
        // while it is still filling, or its later rows would be averaged in
        // against an already-averaged value.
        let cutoff = (now - RAW_WINDOW_MS).div_euclid(BUCKET_MS) * BUCKET_MS;
        put(&connection, "decode_tok_s", cutoff + 1_000, 1.0);
        put(&connection, "decode_tok_s", cutoff + 2_000, 3.0);
        let report = maintain(&connection, now, 7).expect("maintain");
        assert_eq!(report, Maintenance::default());
        assert_eq!(
            rows(&connection, "decode_tok_s"),
            [(cutoff + 1_000, 1.0), (cutoff + 2_000, 3.0)]
        );
    }

    #[test]
    fn retention_deletes_rows_past_the_window_and_keeps_the_rest() {
        let connection = memory_store();
        let now = 100 * RAW_WINDOW_MS;
        let day = 24 * 60 * 60 * 1000i64;
        put(&connection, "decode_tok_s", now - 8 * day, 1.0);
        put(&connection, "decode_tok_s", now - 7 * day + 1, 2.0);
        put(&connection, "decode_tok_s", now - 1_000, 3.0);

        let report = maintain(&connection, now, 7).expect("maintain");
        assert_eq!(report.rows_expired, 1, "only the 8-day-old row expires");
        let kept: Vec<i64> = rows(&connection, "decode_tok_s")
            .into_iter()
            .map(|(ts, _)| ts)
            .collect();
        assert_eq!(kept.len(), 2);
        assert!(kept.iter().all(|ts| *ts >= now - 7 * day));
    }

    #[test]
    fn query_returns_stored_points_untouched_at_native_step() {
        let connection = memory_store();
        put(&connection, "decode_tok_s", 1_000, 1.5);
        put(&connection, "decode_tok_s", 2_000, 2.5);
        let result = read_range(
            &connection,
            &Query {
                instance: "gx".to_owned(),
                series: vec!["decode_tok_s"],
                from_ms: 0,
                to_ms: 10_000,
                step_s: 1,
            },
        )
        .expect("query");
        assert_eq!(result[0].1.points, [(1_000, 1.5), (2_000, 2.5)]);
        assert_eq!(result[0].1.honesty, Some(Honesty::Measured));
    }

    #[test]
    fn query_buckets_by_step_and_leaves_gaps_empty() {
        let connection = memory_store();
        // 0..2 s populated, 2..4 s missing, 4..6 s populated.
        put(&connection, "decode_tok_s", 0, 1.0);
        put(&connection, "decode_tok_s", 1_000, 3.0);
        put(&connection, "decode_tok_s", 4_000, 5.0);
        put(&connection, "completion_tokens_total", 0, 10.0);
        put(&connection, "completion_tokens_total", 1_000, 11.0);

        let result = read_range(
            &connection,
            &Query {
                instance: "gx".to_owned(),
                series: vec!["decode_tok_s", "completion_tokens_total"],
                from_ms: 0,
                to_ms: 10_000,
                step_s: 2,
            },
        )
        .expect("query");
        assert_eq!(
            result[0].1.points,
            [(0, 2.0), (4_000, 5.0)],
            "gauge mean per bucket; the empty 2..4 s bucket yields no point"
        );
        assert_eq!(result[1].1.points, [(0, 11.0)], "counter keeps bucket last");
    }

    #[test]
    fn query_of_an_unsampled_series_is_empty_not_zero() {
        let connection = memory_store();
        put(&connection, "decode_tok_s", 1_000, 1.5);
        let result = read_range(
            &connection,
            &Query {
                instance: "gx".to_owned(),
                series: vec!["wire_ingress_gbps"],
                from_ms: 0,
                to_ms: 10_000,
                step_s: 1,
            },
        )
        .expect("query");
        assert!(result[0].1.points.is_empty());
        assert_eq!(
            result[0].1.honesty, None,
            "no rows means no honesty claim, not a default tag"
        );
    }

    #[test]
    fn query_is_scoped_to_one_instance() {
        let connection = memory_store();
        put(&connection, "decode_tok_s", 1_000, 1.5);
        let result = read_range(
            &connection,
            &Query {
                instance: "mac".to_owned(),
                series: vec!["decode_tok_s"],
                from_ms: 0,
                to_ms: 10_000,
                step_s: 1,
            },
        )
        .expect("query");
        assert!(result[0].1.points.is_empty());
    }

    #[test]
    fn resampling_the_same_millisecond_replaces_rather_than_duplicates() {
        let connection = memory_store();
        put(&connection, "decode_tok_s", 1_000, 1.5);
        put(&connection, "decode_tok_s", 1_000, 2.5);
        assert_eq!(rows(&connection, "decode_tok_s"), [(1_000, 2.5)]);
    }
}
