#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::missing_panics_doc,
    clippy::doc_markdown,
    clippy::cast_possible_truncation,
    clippy::integer_division,
    clippy::let_underscore_must_use,
    clippy::non_ascii_literal,
    clippy::manual_assert,
    dead_code
)]

//! Outbox throughput benchmarks with per-partition ordering verification.
//!
//! **Profiles:**
//!   Validation (default) — 1p1c + 16p16c, 100K msgs, 10 samples. ~90s/engine.
//!   Long-haul (opt-in)   — 1M msgs, 10 samples. ~5 min/engine.
//!     Enable via `BENCH_LONGHAUL=1`.
//!
//! **Run:**
//!   `cargo bench --bench outbox_throughput --features preview-outbox`
//!   `cargo bench --bench outbox_throughput --features preview-outbox -- "postgres/16p16c_single"`
//!   `BENCH_LONGHAUL=1 cargo bench --bench outbox_throughput --features preview-outbox`

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use criterion::{Criterion, Throughput, criterion_group};
use dashmap::DashMap;
use tokio::runtime::Runtime;
use tokio::sync::Notify;

use modkit_db::outbox::{
    EnqueueMessage, Handler, HandlerResult, Outbox, OutboxHandle, OutboxMessage, OutboxProfile,
    Partitions, outbox_migrations,
};
use modkit_db::{ConnectOpts, Db, connect_db, migration_runner::run_migrations_for_testing};
use tokio_util::sync::CancellationToken;

// Global counter shared across all bench_fn! expansions.
// Each macro expansion previously declared its own static ITER_COUNTER,
// restarting at 0 — so bench families produced colliding queue names
// like "bench_0", "bench_1" that reused rows from earlier families.
// A single global counter ensures process-wide unique queue names.
static GLOBAL_ITER_COUNTER: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

// ── Message counts ──────────────────────────────────────────────────
// All multiples of PARTITIONS (64) for even distribution.

/// 1P/1C validation — 10K msgs.
const MSG_1P: usize = 64 * 157; // 10_048

/// 16P/16C validation — 100K msgs.
const MSG_16P: usize = 64 * 1_563; // 100_032

/// Long-haul — 1M msgs.
const MSG_1M: usize = 64 * 15_625; // 1_000_000

/// SQLite long-haul cap — 100K (single-writer bottleneck).
const MSG_SQLITE_LONGHAUL: usize = 64 * 1_563; // 100_032

// ── Pipeline sizing ────────────────────────────────────────────────

const BATCH_SIZE: usize = 100;
const PARTITIONS: u16 = 64;
const NUM_PRODUCERS: usize = 16;
const MAX_CONCURRENT_PARTITIONS: usize = 16;

// ── Timeouts ───────────────────────────────────────────────────────

/// Validation benchmarks.
const TIMEOUT_STANDARD: Duration = Duration::from_secs(120);

/// Long-haul (1M) benchmarks.
const TIMEOUT_1M: Duration = Duration::from_secs(300);

// ---------------------------------------------------------------------------
// BenchState — shared state for handler + verification
// ---------------------------------------------------------------------------

struct BenchState {
    /// partition_id → payload sequence numbers in consumption order
    consumed: Arc<DashMap<i64, Vec<u64>>>,
    /// partition_id → DB-assigned seq values in consumption order
    db_seqs: Arc<DashMap<i64, Vec<i64>>>,
    /// Total messages consumed so far
    counter: Arc<AtomicUsize>,
    /// Signaled when counter reaches expected_total
    notify: Arc<Notify>,
    /// Target message count
    expected_total: usize,
}

impl BenchState {
    fn new(expected_total: usize) -> Self {
        Self {
            consumed: Arc::new(DashMap::new()),
            db_seqs: Arc::new(DashMap::new()),
            counter: Arc::new(AtomicUsize::new(0)),
            notify: Arc::new(Notify::new()),
            expected_total,
        }
    }
}

// ---------------------------------------------------------------------------
// BenchHandler — captures consumed messages for verification
// ---------------------------------------------------------------------------

struct BenchHandler {
    consumed: Arc<DashMap<i64, Vec<u64>>>,
    db_seqs: Arc<DashMap<i64, Vec<i64>>>,
    counter: Arc<AtomicUsize>,
    notify: Arc<Notify>,
    expected_total: usize,
}

#[async_trait::async_trait]
impl Handler for BenchHandler {
    async fn handle(&self, msgs: &[OutboxMessage], _cancel: CancellationToken) -> HandlerResult {
        for msg in msgs {
            // Parse payload "partition:seq"
            let payload = std::str::from_utf8(&msg.payload).unwrap();
            let (_, seq_str) = payload.split_once(':').unwrap();
            let seq: u64 = seq_str.parse().unwrap();

            self.consumed.entry(msg.partition_id).or_default().push(seq);
            self.db_seqs
                .entry(msg.partition_id)
                .or_default()
                .push(msg.seq);

            let prev = self.counter.fetch_add(1, Ordering::Relaxed);
            if prev + 1 >= self.expected_total {
                self.notify.notify_one();
            }
        }
        HandlerResult::Success
    }
}

// ---------------------------------------------------------------------------
// Wait for consumption completion
// ---------------------------------------------------------------------------

async fn wait_for_completion(state: &BenchState, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if state.counter.load(Ordering::Acquire) >= state.expected_total {
            return;
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::ZERO);
        if remaining.is_zero() {
            let got = state.counter.load(Ordering::Acquire);
            panic!(
                "Timeout: expected {} messages consumed, got {got} ({} missing)",
                state.expected_total,
                state.expected_total - got
            );
        }
        // Sleep until notified or remaining timeout
        let _ = tokio::time::timeout(remaining, state.notify.notified()).await;
    }
}

// ---------------------------------------------------------------------------
// Ordering & completeness verification
// ---------------------------------------------------------------------------

fn verify_ordering(state: &BenchState, total: usize) {
    let msgs_per_partition = total / PARTITIONS as usize;

    // All 64 partitions present
    assert_eq!(
        state.consumed.len(),
        PARTITIONS as usize,
        "expected {PARTITIONS} partitions in results, got {}",
        state.consumed.len()
    );

    for entry in state.consumed.iter() {
        let partition = *entry.key();
        let seqs = entry.value();

        // Completeness: correct count
        assert_eq!(
            seqs.len(),
            msgs_per_partition,
            "partition {partition}: expected {msgs_per_partition} messages, got {}",
            seqs.len()
        );

        // Payload-seq completeness: all expected sequences present (order may differ in 16P)
        let actual: HashSet<u64> = seqs.iter().copied().collect();
        let expected: HashSet<u64> = (0..msgs_per_partition as u64).collect();
        assert_eq!(
            actual,
            expected,
            "partition {partition}: payload sequence mismatch — \
             missing: {:?}, extra: {:?}",
            expected.difference(&actual).collect::<Vec<_>>(),
            actual.difference(&expected).collect::<Vec<_>>()
        );
    }

    // DB-seq ordering: must be strictly monotonically increasing per partition
    for entry in state.db_seqs.iter() {
        let partition = *entry.key();
        let db_seqs = entry.value();
        for i in 1..db_seqs.len() {
            assert!(
                db_seqs[i] > db_seqs[i - 1],
                "partition {partition}: DB seq ordering violation at position {i}: \
                 seq[{}]={} >= seq[{i}]={}",
                i - 1,
                db_seqs[i - 1],
                db_seqs[i]
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Producer functions
// ---------------------------------------------------------------------------

async fn produce_single_range(
    outbox: &Arc<Outbox>,
    db: &Db,
    queue: &str,
    start: usize,
    end: usize,
) {
    for i in start..end {
        let partition = (i % PARTITIONS as usize) as u32;
        let seq = i / PARTITIONS as usize;
        let payload = format!("{partition}:{seq}").into_bytes();
        let o = Arc::clone(outbox);
        let q = queue.to_owned();
        let (_, result) = o
            .transaction(db.clone(), |tx| {
                let o2 = Arc::clone(&o);
                Box::pin(async move {
                    o2.enqueue(tx, &q, partition, payload, "bench/seq")
                        .await
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                    Ok(())
                })
            })
            .await;
        result.unwrap();
    }
}

async fn produce_batch_range(outbox: &Arc<Outbox>, db: &Db, queue: &str, start: usize, end: usize) {
    for chunk_start in (start..end).step_by(BATCH_SIZE) {
        let chunk_end = (chunk_start + BATCH_SIZE).min(end);
        let messages: Vec<EnqueueMessage<'_>> = (chunk_start..chunk_end)
            .map(|i| {
                let partition = (i % PARTITIONS as usize) as u32;
                let seq = i / PARTITIONS as usize;
                EnqueueMessage {
                    partition,
                    payload: format!("{partition}:{seq}").into_bytes(),
                    payload_type: "bench/seq",
                }
            })
            .collect();
        let o = Arc::clone(outbox);
        let q = queue.to_owned();
        let (_, result) = o
            .transaction(db.clone(), |tx| {
                let o2 = Arc::clone(&o);
                Box::pin(async move {
                    o2.enqueue_batch(tx, &q, &messages)
                        .await
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                    Ok(())
                })
            })
            .await;
        result.unwrap();
    }
}

async fn produce_single(outbox: &Arc<Outbox>, db: &Db, queue: &str, total: usize) {
    produce_single_range(outbox, db, queue, 0, total).await;
}

async fn produce_batch(outbox: &Arc<Outbox>, db: &Db, queue: &str, total: usize) {
    produce_batch_range(outbox, db, queue, 0, total).await;
}

async fn produce_concurrent_single(outbox: &Arc<Outbox>, db: &Db, queue: &str, total: usize) {
    let per_producer = total / NUM_PRODUCERS;
    let mut handles = Vec::with_capacity(NUM_PRODUCERS);
    for producer_id in 0..NUM_PRODUCERS {
        let start = producer_id * per_producer;
        let end = if producer_id == NUM_PRODUCERS - 1 {
            total
        } else {
            start + per_producer
        };
        let outbox = Arc::clone(outbox);
        let db = db.clone();
        let q = queue.to_owned();
        handles.push(tokio::spawn(async move {
            produce_single_range(&outbox, &db, &q, start, end).await;
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
}

async fn produce_concurrent_batch(outbox: &Arc<Outbox>, db: &Db, queue: &str, total: usize) {
    let per_producer = total / NUM_PRODUCERS;
    let mut handles = Vec::with_capacity(NUM_PRODUCERS);
    for producer_id in 0..NUM_PRODUCERS {
        let start = producer_id * per_producer;
        let end = if producer_id == NUM_PRODUCERS - 1 {
            total
        } else {
            start + per_producer
        };
        let outbox = Arc::clone(outbox);
        let db = db.clone();
        let q = queue.to_owned();
        handles.push(tokio::spawn(async move {
            produce_batch_range(&outbox, &db, &q, start, end).await;
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
}

// ---------------------------------------------------------------------------
// Pipeline setup
// ---------------------------------------------------------------------------

/// Each iteration uses a unique queue name to avoid interference from
/// previous iterations' leftover data. The sequencer and processors only
/// operate on partitions belonging to their queue.
fn iter_queue_name(iter: u64) -> String {
    format!("bench_{iter}")
}

async fn setup_pipeline(
    db_url: &str,
    expected_total: usize,
    queue_name: &str,
) -> (Db, OutboxHandle, Arc<BenchState>) {
    setup_pipeline_with(db_url, expected_total, queue_name).await
}

async fn setup_pipeline_with(
    db_url: &str,
    expected_total: usize,
    queue_name: &str,
) -> (Db, OutboxHandle, Arc<BenchState>) {
    // Pool budget: producers + processors + sequencers + vacuum + margin.
    // 16 producers + 8 processors + 2 sequencers + 1 vacuum + 5 margin = 32.
    let max_conns = if db_url.starts_with("sqlite") { 1 } else { 32 };
    let db = connect_db(
        db_url,
        ConnectOpts {
            max_conns: Some(max_conns),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let state = Arc::new(BenchState::new(expected_total));
    let handler = BenchHandler {
        consumed: Arc::clone(&state.consumed),
        db_seqs: Arc::clone(&state.db_seqs),
        counter: Arc::clone(&state.counter),
        notify: Arc::clone(&state.notify),
        expected_total,
    };

    let handle = Outbox::builder(db.clone())
        .profile(OutboxProfile::high_throughput())
        .processors(8)
        .maintenance(2, 1)
        .stats_interval(Duration::from_secs(10))
        .queue(queue_name, Partitions::of(PARTITIONS))
        .batch_decoupled(handler)
        .start()
        .await
        .unwrap();

    (db, handle, state)
}

/// Delete all data from outbox tables after each iteration. Prevents
/// unbounded row accumulation across iterations in the shared database.
///
/// Deletion order respects FK constraints: child tables first, then parents.
async fn cleanup_outbox_tables(db_url: &str) {
    use sea_orm::{ConnectionTrait, Database, Statement};

    // Tables in FK-safe deletion order (children before parents).
    const TABLES: &[&str] = &[
        "modkit_outbox_dead_letters",
        "modkit_outbox_outgoing",
        "modkit_outbox_incoming",
        "modkit_outbox_vacuum_counter",
        "modkit_outbox_processor",
        "modkit_outbox_body",
        "modkit_outbox_partitions",
    ];

    let db = Database::connect(db_url).await.unwrap();
    let backend = db.get_database_backend();
    for table in TABLES {
        let sql = match backend {
            sea_orm::DbBackend::Postgres => format!("TRUNCATE TABLE {table} CASCADE"),
            sea_orm::DbBackend::Sqlite | sea_orm::DbBackend::MySql => {
                format!("DELETE FROM {table}")
            }
        };
        db.execute(Statement::from_string(backend, sql))
            .await
            .unwrap();
    }
}

// ---------------------------------------------------------------------------
// Container helpers + TCP wait
// ---------------------------------------------------------------------------

async fn wait_for_tcp(host: &str, port: u16, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if tokio::net::TcpStream::connect((host, port)).await.is_ok() {
            return;
        }
        if Instant::now() >= deadline {
            panic!("Timeout waiting for {host}:{port}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

// --- Postgres ---

#[cfg(feature = "pg")]
mod pg_container {
    use super::{
        ConnectOpts, Duration, OnceLock, Runtime, connect_db, outbox_migrations,
        run_migrations_for_testing, wait_for_tcp,
    };
    use testcontainers::{ContainerAsync, ContainerRequest, ImageExt, runners::AsyncRunner};
    use testcontainers_modules::postgres::Postgres;

    pub struct PgContainer {
        pub url: String,
        _container: ContainerAsync<Postgres>,
    }

    static PG: OnceLock<PgContainer> = OnceLock::new();

    pub fn get_pg(rt: &Runtime) -> &'static PgContainer {
        PG.get_or_init(|| {
            rt.block_on(async {
                // Default Postgres image — no custom config.
                // Always bench against stock PG settings (fsync=on, etc.)
                // so numbers reflect realistic deployment, not synthetic peaks.
                let container = ContainerRequest::from(Postgres::default())
                    .with_env_var("POSTGRES_PASSWORD", "pass")
                    .with_env_var("POSTGRES_USER", "user")
                    .with_env_var("POSTGRES_DB", "bench")
                    .start()
                    .await
                    .unwrap();
                let port = container.get_host_port_ipv4(5432).await.unwrap();
                wait_for_tcp("127.0.0.1", port, Duration::from_secs(30)).await;
                let url = format!("postgres://user:pass@127.0.0.1:{port}/bench");

                // Run migrations once
                let db = connect_db(&url, ConnectOpts::default()).await.unwrap();
                run_migrations_for_testing(&db, outbox_migrations())
                    .await
                    .unwrap();

                PgContainer {
                    url,
                    _container: container,
                }
            })
        })
    }
}

// --- MySQL ---

#[cfg(feature = "mysql")]
mod mysql_container {
    use super::{
        ConnectOpts, Duration, OnceLock, Runtime, connect_db, outbox_migrations,
        run_migrations_for_testing, wait_for_tcp,
    };
    use testcontainers::{ContainerAsync, ContainerRequest, ImageExt, runners::AsyncRunner};
    use testcontainers_modules::mysql::Mysql;

    pub struct MysqlContainer {
        pub url: String,
        _container: ContainerAsync<Mysql>,
    }

    static MYSQL: OnceLock<MysqlContainer> = OnceLock::new();

    pub fn get_mysql(rt: &Runtime) -> &'static MysqlContainer {
        MYSQL.get_or_init(|| {
            rt.block_on(async {
                // MySQL tuning for benchmark parity with Postgres:
                // - READ-COMMITTED: eliminates InnoDB gap-lock deadlocks when
                //   multiple sequencers claim/delete from adjacent partitions
                // - skip-log-bin: disables binary log fsync, the #1 bottleneck
                //   (COMMIT is 105x slower than PG with binlog enabled)
                // - innodb-flush-log-at-trx-commit=2: flush redo log once/sec
                //   instead of per-commit (ok for benchmarks, not production)
                //
                // Profiling showed MySQL COMMIT at 9.5ms vs PG at 0.09ms.
                // claim_incoming (SELECT+DELETE) was 8x slower due to InnoDB
                // row-level lock manager overhead vs Postgres MVCC.
                let container = ContainerRequest::from(Mysql::default())
                    .with_env_var("MYSQL_ROOT_PASSWORD", "root")
                    .with_env_var("MYSQL_USER", "user")
                    .with_env_var("MYSQL_PASSWORD", "pass")
                    .with_env_var("MYSQL_DATABASE", "bench")
                    .with_cmd([
                        "--transaction-isolation=READ-COMMITTED",
                        "--skip-log-bin",
                        "--innodb-flush-log-at-trx-commit=2",
                    ])
                    .start()
                    .await
                    .unwrap();
                let port = container.get_host_port_ipv4(3306).await.unwrap();
                wait_for_tcp("127.0.0.1", port, Duration::from_secs(60)).await;
                let url = format!("mysql://user:pass@127.0.0.1:{port}/bench");

                let db = connect_db(&url, ConnectOpts::default()).await.unwrap();
                run_migrations_for_testing(&db, outbox_migrations())
                    .await
                    .unwrap();

                MysqlContainer {
                    url,
                    _container: container,
                }
            })
        })
    }
}

// --- MariaDB ---

#[cfg(feature = "mysql")]
mod mariadb_container {
    use super::{
        ConnectOpts, Duration, OnceLock, Runtime, connect_db, outbox_migrations,
        run_migrations_for_testing, wait_for_tcp,
    };
    use testcontainers::core::{ContainerPort, WaitFor};
    use testcontainers::{
        ContainerAsync, ContainerRequest, GenericImage, ImageExt, runners::AsyncRunner,
    };

    pub struct MariaContainer {
        pub url: String,
        _container: ContainerAsync<GenericImage>,
    }

    static MARIADB: OnceLock<MariaContainer> = OnceLock::new();

    fn mariadb_image() -> GenericImage {
        GenericImage::new("mariadb", "lts")
            .with_wait_for(WaitFor::message_on_stderr("ready for connections"))
            .with_exposed_port(ContainerPort::Tcp(3306))
    }

    pub fn get_mariadb(rt: &Runtime) -> &'static MariaContainer {
        MARIADB.get_or_init(|| {
            rt.block_on(async {
                let container = ContainerRequest::from(mariadb_image())
                    .with_env_var("MYSQL_ROOT_PASSWORD", "root")
                    .with_env_var("MYSQL_USER", "user")
                    .with_env_var("MYSQL_PASSWORD", "pass")
                    .with_env_var("MYSQL_DATABASE", "bench")
                    .with_cmd([
                        "--transaction-isolation=READ-COMMITTED",
                        "--skip-log-bin",
                        "--innodb-flush-log-at-trx-commit=2",
                    ])
                    .start()
                    .await
                    .unwrap();
                let port = container.get_host_port_ipv4(3306).await.unwrap();
                wait_for_tcp("127.0.0.1", port, Duration::from_secs(60)).await;
                let url = format!("mysql://user:pass@127.0.0.1:{port}/bench");

                let db = connect_db(&url, ConnectOpts::default()).await.unwrap();
                run_migrations_for_testing(&db, outbox_migrations())
                    .await
                    .unwrap();

                MariaContainer {
                    url,
                    _container: container,
                }
            })
        })
    }
}

// --- SQLite ---

#[cfg(feature = "sqlite")]
mod sqlite_setup {
    use super::{
        ConnectOpts, Db, OnceLock, Runtime, connect_db, outbox_migrations,
        run_migrations_for_testing,
    };

    /// In-memory SQLite with shared cache. We keep `_db` alive so the
    /// in-memory database survives across `setup_pipeline` reconnections.
    pub struct SqliteDb {
        pub url: String,
        _db: Db,
    }

    static SQLITE: OnceLock<SqliteDb> = OnceLock::new();

    pub fn get_sqlite(rt: &Runtime) -> &'static SqliteDb {
        SQLITE.get_or_init(|| {
            rt.block_on(async {
                let url = "sqlite:file:outbox_bench?mode=memory&cache=shared".to_owned();

                let db = connect_db(
                    &url,
                    ConnectOpts {
                        max_conns: Some(1),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
                run_migrations_for_testing(&db, outbox_migrations())
                    .await
                    .unwrap();

                SqliteDb { url, _db: db }
            })
        })
    }
}

// ---------------------------------------------------------------------------
// Generic benchmark runner — uses a macro to avoid lifetime issues with closures
// ---------------------------------------------------------------------------

/// Runs a benchmark for a given database URL and producer function.
///
/// We use an iteration counter to give each criterion iteration a unique queue
/// name, ensuring complete isolation between iterations without needing to
/// truncate tables.
macro_rules! bench_fn {
    ($c:expr, $group_name:expr, $bench_name:expr, $db_url:expr, $msg_count:expr, $rt:expr, $producer:path) => {
        bench_fn!(
            $c,
            $group_name,
            $bench_name,
            $db_url,
            $msg_count,
            $rt,
            $producer,
            TIMEOUT_STANDARD,
            10,
            15,
            2
        )
    };
    ($c:expr, $group_name:expr, $bench_name:expr, $db_url:expr, $msg_count:expr, $rt:expr, $producer:path, $timeout:expr, $samples:expr, $measure_secs:expr, $warmup_secs:expr) => {{
        let mut group = $c.benchmark_group($group_name);
        group.throughput(Throughput::Elements($msg_count as u64));
        group.sample_size($samples);
        group.measurement_time(Duration::from_secs($measure_secs));
        group.warm_up_time(Duration::from_secs($warmup_secs));

        group.bench_function($bench_name, |b| {
            b.iter_custom(|iters| {
                $rt.block_on(async {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        let iter_id = GLOBAL_ITER_COUNTER.fetch_add(1, Ordering::Relaxed);
                        let queue = iter_queue_name(iter_id);
                        let (db, handle, state) =
                            setup_pipeline_with($db_url, $msg_count, &queue).await;

                        let start = Instant::now();
                        $producer(handle.outbox(), &db, &queue, $msg_count).await;
                        wait_for_completion(&state, $timeout).await;
                        total += start.elapsed();

                        verify_ordering(&state, $msg_count);
                        handle.stop().await;
                        drop(db); // close pool before cleanup
                        cleanup_outbox_tables($db_url).await;
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    total
                })
            });
        });
        group.finish();
    }};
}

// ---------------------------------------------------------------------------
// Postgres benchmarks
// ---------------------------------------------------------------------------
//
// Validation (<90s): 1p1c_single + 16p16c_single + 16p16c_batch
//   Quick regression check during development.
//
// Long-haul (~5 min): 1m_16p_single + 1m_16p_batch
//   Stable baseline numbers for performance reporting.

#[cfg(feature = "pg")]
fn pg_1p1c_single(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let pg = pg_container::get_pg(&rt);
    bench_fn!(
        c,
        "postgres",
        "1p1c_single",
        &pg.url,
        MSG_1P,
        rt,
        produce_single
    );
}

#[cfg(feature = "pg")]
fn pg_16p16c_single(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(16)
        .enable_all()
        .build()
        .unwrap();
    let pg = pg_container::get_pg(&rt);
    bench_fn!(
        c,
        "postgres",
        "16p16c_single",
        &pg.url,
        MSG_16P,
        rt,
        produce_concurrent_single
    );
}

#[cfg(feature = "pg")]
fn pg_16p16c_batch(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(16)
        .enable_all()
        .build()
        .unwrap();
    let pg = pg_container::get_pg(&rt);
    bench_fn!(
        c,
        "postgres",
        "16p16c_batch",
        &pg.url,
        MSG_16P,
        rt,
        produce_concurrent_batch
    );
}

// --- Postgres long-haul (1M) ---

#[cfg(feature = "pg")]
fn pg_1m_single(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(16)
        .enable_all()
        .build()
        .unwrap();
    let pg = pg_container::get_pg(&rt);
    bench_fn!(
        c,
        "postgres_longhaul",
        "1m_16p_single",
        &pg.url,
        MSG_1M,
        rt,
        produce_concurrent_single,
        TIMEOUT_1M,
        10,
        120,
        1
    );
}

#[cfg(feature = "pg")]
fn pg_1m_batch(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(16)
        .enable_all()
        .build()
        .unwrap();
    let pg = pg_container::get_pg(&rt);
    bench_fn!(
        c,
        "postgres_longhaul",
        "1m_16p_batch",
        &pg.url,
        MSG_1M,
        rt,
        produce_concurrent_batch,
        TIMEOUT_1M,
        10,
        120,
        1
    );
}

#[cfg(feature = "pg")]
criterion_group!(
    postgres_bench,
    pg_1p1c_single,
    pg_16p16c_single,
    pg_16p16c_batch
);

#[cfg(feature = "pg")]
criterion_group!(postgres_longhaul, pg_1m_single, pg_1m_batch);

// ---------------------------------------------------------------------------
// MySQL benchmarks
// ---------------------------------------------------------------------------

#[cfg(feature = "mysql")]
fn mysql_1p1c_single(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mysql = mysql_container::get_mysql(&rt);
    bench_fn!(
        c,
        "mysql",
        "1p1c_single",
        &mysql.url,
        MSG_1P,
        rt,
        produce_single
    );
}

#[cfg(feature = "mysql")]
fn mysql_16p16c_single(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(16)
        .enable_all()
        .build()
        .unwrap();
    let mysql = mysql_container::get_mysql(&rt);
    bench_fn!(
        c,
        "mysql",
        "16p16c_single",
        &mysql.url,
        MSG_16P,
        rt,
        produce_concurrent_single
    );
}

#[cfg(feature = "mysql")]
fn mysql_16p16c_batch(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(16)
        .enable_all()
        .build()
        .unwrap();
    let mysql = mysql_container::get_mysql(&rt);
    bench_fn!(
        c,
        "mysql",
        "16p16c_batch",
        &mysql.url,
        MSG_16P,
        rt,
        produce_concurrent_batch
    );
}

// --- MySQL long-haul (1M) ---

#[cfg(feature = "mysql")]
fn mysql_1m_single(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(16)
        .enable_all()
        .build()
        .unwrap();
    let mysql = mysql_container::get_mysql(&rt);
    bench_fn!(
        c,
        "mysql_longhaul",
        "1m_16p_single",
        &mysql.url,
        MSG_1M,
        rt,
        produce_concurrent_single,
        TIMEOUT_1M,
        10,
        120,
        1
    );
}

#[cfg(feature = "mysql")]
fn mysql_1m_batch(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(16)
        .enable_all()
        .build()
        .unwrap();
    let mysql = mysql_container::get_mysql(&rt);
    bench_fn!(
        c,
        "mysql_longhaul",
        "1m_16p_batch",
        &mysql.url,
        MSG_1M,
        rt,
        produce_concurrent_batch,
        TIMEOUT_1M,
        10,
        120,
        1
    );
}

#[cfg(feature = "mysql")]
criterion_group!(
    mysql_bench,
    mysql_1p1c_single,
    mysql_16p16c_single,
    mysql_16p16c_batch
);

#[cfg(feature = "mysql")]
criterion_group!(mysql_longhaul, mysql_1m_single, mysql_1m_batch);

// ---------------------------------------------------------------------------
// MariaDB benchmarks
// ---------------------------------------------------------------------------

#[cfg(feature = "mysql")]
fn maria_1p1c_single(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let maria = mariadb_container::get_mariadb(&rt);
    bench_fn!(
        c,
        "mariadb",
        "1p1c_single",
        &maria.url,
        MSG_1P,
        rt,
        produce_single
    );
}

#[cfg(feature = "mysql")]
fn maria_16p16c_single(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(16)
        .enable_all()
        .build()
        .unwrap();
    let maria = mariadb_container::get_mariadb(&rt);
    bench_fn!(
        c,
        "mariadb",
        "16p16c_single",
        &maria.url,
        MSG_16P,
        rt,
        produce_concurrent_single
    );
}

#[cfg(feature = "mysql")]
fn maria_16p16c_batch(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(16)
        .enable_all()
        .build()
        .unwrap();
    let maria = mariadb_container::get_mariadb(&rt);
    bench_fn!(
        c,
        "mariadb",
        "16p16c_batch",
        &maria.url,
        MSG_16P,
        rt,
        produce_concurrent_batch
    );
}

// --- MariaDB long-haul (1M) ---

#[cfg(feature = "mysql")]
fn maria_1m_single(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(16)
        .enable_all()
        .build()
        .unwrap();
    let maria = mariadb_container::get_mariadb(&rt);
    bench_fn!(
        c,
        "mariadb_longhaul",
        "1m_16p_single",
        &maria.url,
        MSG_1M,
        rt,
        produce_concurrent_single,
        TIMEOUT_1M,
        10,
        120,
        1
    );
}

#[cfg(feature = "mysql")]
fn maria_1m_batch(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(16)
        .enable_all()
        .build()
        .unwrap();
    let maria = mariadb_container::get_mariadb(&rt);
    bench_fn!(
        c,
        "mariadb_longhaul",
        "1m_16p_batch",
        &maria.url,
        MSG_1M,
        rt,
        produce_concurrent_batch,
        TIMEOUT_1M,
        10,
        120,
        1
    );
}

#[cfg(feature = "mysql")]
criterion_group!(
    mariadb_bench,
    maria_1p1c_single,
    maria_16p16c_single,
    maria_16p16c_batch
);

#[cfg(feature = "mysql")]
criterion_group!(mariadb_longhaul, maria_1m_single, maria_1m_batch);

// ---------------------------------------------------------------------------
// SQLite benchmarks (single-writer — only 1P/1C is meaningful)
// ---------------------------------------------------------------------------

#[cfg(feature = "sqlite")]
fn sqlite_1p1c_single(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let sq = sqlite_setup::get_sqlite(&rt);
    bench_fn!(
        c,
        "sqlite",
        "1p1c_single",
        &sq.url,
        MSG_1P,
        rt,
        produce_single
    );
}

#[cfg(feature = "sqlite")]
fn sqlite_1p1c_batch(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let sq = sqlite_setup::get_sqlite(&rt);
    bench_fn!(
        c,
        "sqlite",
        "1p1c_batch",
        &sq.url,
        MSG_1P,
        rt,
        produce_batch
    );
}

// SQLite long-haul: 100K single-thread (single-writer bottleneck caps volume)
#[cfg(feature = "sqlite")]
fn sqlite_longhaul_single(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let sq = sqlite_setup::get_sqlite(&rt);
    bench_fn!(
        c,
        "sqlite_longhaul",
        "100k_1p_single",
        &sq.url,
        MSG_SQLITE_LONGHAUL,
        rt,
        produce_single,
        TIMEOUT_1M,
        10,
        120,
        1
    );
}

#[cfg(feature = "sqlite")]
fn sqlite_longhaul_batch(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let sq = sqlite_setup::get_sqlite(&rt);
    bench_fn!(
        c,
        "sqlite_longhaul",
        "100k_1p_batch",
        &sq.url,
        MSG_SQLITE_LONGHAUL,
        rt,
        produce_batch,
        TIMEOUT_1M,
        10,
        120,
        1
    );
}

#[cfg(feature = "sqlite")]
criterion_group!(sqlite_bench, sqlite_1p1c_single, sqlite_1p1c_batch);

#[cfg(feature = "sqlite")]
criterion_group!(
    sqlite_longhaul_group,
    sqlite_longhaul_single,
    sqlite_longhaul_batch
);

// ---------------------------------------------------------------------------
// Tracing setup — logs to /tmp/outbox_bench.log
// ---------------------------------------------------------------------------

fn setup_tracing() -> tracing_appender::non_blocking::WorkerGuard {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};

    let log_file = std::fs::File::create("/tmp/outbox_bench.log")
        .expect("failed to create /tmp/outbox_bench.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(log_file);

    tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false)
                .with_target(true)
                .with_level(true),
        )
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    eprintln!("Benchmark logs → /tmp/outbox_bench.log");
    guard
}

// ---------------------------------------------------------------------------
// Custom main — tracing + criterion + docker cleanup
// ---------------------------------------------------------------------------

#[cfg(not(any(feature = "pg", feature = "mysql", feature = "sqlite")))]
compile_error!(
    "outbox_throughput benchmark requires at least one database feature: pg, mysql, or sqlite"
);

fn main() {
    let guard = setup_tracing();
    let longhaul = std::env::var("BENCH_LONGHAUL").is_ok();

    // Validation profiles — quick regression check (<90s per engine).
    #[cfg(feature = "pg")]
    postgres_bench();
    #[cfg(feature = "mysql")]
    {
        mysql_bench();
        mariadb_bench();
    }
    #[cfg(feature = "sqlite")]
    sqlite_bench();

    // Long-haul profiles — stable baselines (opt-in via BENCH_LONGHAUL=1).
    if longhaul {
        #[cfg(feature = "pg")]
        postgres_longhaul();
        #[cfg(feature = "mysql")]
        {
            mysql_longhaul();
            mariadb_longhaul();
        }
        #[cfg(feature = "sqlite")]
        sqlite_longhaul_group();
    }

    criterion::Criterion::default()
        .configure_from_args()
        .final_summary();

    // Flush tracing
    drop(guard);
}
