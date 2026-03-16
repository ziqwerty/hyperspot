use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;

use super::builder::QueueBuilder;
use super::core::Outbox;
use super::prioritizer::SharedPrioritizer;
use super::stats::{StatsListener, StatsRegistry, StatsReporter};
use super::taskward::{
    BackoffConfig, Bulkhead, BulkheadConfig, ConcurrencyLimit, PanicPolicy, TaskSet,
    TracingListener, WorkerBuilder,
};
use super::types::{OutboxConfig, OutboxError, Partitions, QueueConfig, SequencerConfig};
use super::workers::sequencer::{Sequencer, SequencerReport};
use super::workers::vacuum::{VacuumReport, VacuumTask};
use crate::Db;

/// Deferred queue declaration — config + factory, resolved at `start()`.
pub struct QueueDeclaration {
    pub(crate) name: String,
    pub(crate) partitions: Partitions,
    pub(crate) config: QueueConfig,
    pub(crate) factory: Box<dyn super::builder::ProcessorFactory>,
}

/// Fluent builder for the outbox pipeline.
///
/// Entry point: [`Outbox::builder(db)`](Outbox::builder). Configure global
/// settings and register queues with handlers, then call
/// [`start()`](Self::start) to spawn background tasks.
///
/// ```ignore
/// let handle = Outbox::builder(db)
///     .poll_interval(Duration::from_millis(100))
///     .queue("orders", Partitions::of(4))
///         .decoupled(my_handler)
///     .start().await?;
/// // enqueue via handle.outbox()
/// handle.stop().await;
/// ```
pub struct OutboxBuilder {
    db: Db,
    sequencer_batch_size: u32,
    poll_interval: Duration,
    poker_interval: Duration,
    partition_batch_limit: u32,
    max_inner_iterations: u32,
    processors: Option<usize>,
    maintenance_guaranteed: Option<usize>,
    maintenance_shared: Option<usize>,
    vacuum_cooldown: Duration,
    stats_interval: Option<Duration>,
    steady_pace: Duration,
    pub(crate) queue_declarations: Vec<QueueDeclaration>,
}

impl OutboxBuilder {
    pub(crate) fn new(db: Db) -> Self {
        Self {
            db,
            sequencer_batch_size: super::types::DEFAULT_SEQUENCER_BATCH_SIZE,
            poll_interval: super::types::DEFAULT_POLL_INTERVAL,
            poker_interval: super::types::DEFAULT_POKER_INTERVAL,
            partition_batch_limit: super::types::DEFAULT_PARTITION_BATCH_LIMIT,
            max_inner_iterations: super::types::DEFAULT_MAX_INNER_ITERATIONS,
            processors: None,
            maintenance_guaranteed: None,
            maintenance_shared: None,
            vacuum_cooldown: super::types::DEFAULT_VACUUM_COOLDOWN,
            stats_interval: Some(Duration::from_secs(10)),
            steady_pace: Duration::from_millis(10),
            queue_declarations: Vec::new(),
        }
    }

    /// Sequencer batch size (rows per cycle). Default: 100.
    #[must_use]
    pub fn sequencer_batch_size(mut self, n: u32) -> Self {
        self.sequencer_batch_size = n;
        self
    }

    /// Safety net fallback poll interval for both the sequencer and
    /// per-partition processors. Default: 1s.
    #[must_use]
    pub fn poll_interval(mut self, d: Duration) -> Self {
        self.poll_interval = d;
        self
    }

    /// Maximum number of concurrent partition processors across all queues.
    ///
    /// Controls the global processor semaphore. Each partition processor
    /// acquires one permit before executing. Default: unlimited.
    ///
    /// # Panics
    ///
    /// Panics if `n` is 0.
    ///
    /// # Connection budget
    ///
    /// `max_connections >= processors + guaranteed + shared + producer_headroom`
    #[must_use]
    pub fn processors(mut self, n: usize) -> Self {
        assert!(n > 0, "processors must be > 0");
        self.processors = Some(n);
        self
    }

    /// Two-tier maintenance connection budget.
    ///
    /// - `guaranteed`: permits reserved exclusively for sequencer workers.
    /// - `shared`: permits available to both sequencer and maintenance tasks
    ///   (vacuum). Sequencers steal shared permits when maintenance is idle.
    ///
    /// Total sequencer workers = `guaranteed + shared`.
    /// Vacuum workers = `shared`.
    ///
    /// # Panics
    ///
    /// Panics if either `guaranteed` or `shared` is 0.
    ///
    /// # Connection budget
    ///
    /// `max_connections >= processors + guaranteed + shared + producer_headroom`
    ///
    /// Default: `guaranteed = 2, shared = 1`.
    #[must_use]
    pub fn maintenance(mut self, guaranteed: usize, shared: usize) -> Self {
        assert!(guaranteed > 0, "maintenance guaranteed must be > 0");
        assert!(shared > 0, "maintenance shared must be > 0");
        self.maintenance_guaranteed = Some(guaranteed);
        self.maintenance_shared = Some(shared);
        self
    }

    /// Cold reconciler (poker) interval. Default: 60s.
    /// The poker is a safety net that discovers partitions with pending
    /// incoming rows by querying the DB. Normally, the hot path (enqueue)
    /// populates the dirty set directly.
    ///
    /// # Panics
    ///
    /// Panics if `d` is zero.
    #[must_use]
    pub fn poker_interval(mut self, d: Duration) -> Self {
        assert!(!d.is_zero(), "poker_interval must be > 0");
        self.poker_interval = d;
        self
    }

    /// Max partitions the sequencer processes per cycle. Default: 128.
    ///
    /// # Panics
    ///
    /// Panics if `n` is 0.
    #[must_use]
    pub fn partition_batch_limit(mut self, n: u32) -> Self {
        assert!(n > 0, "partition_batch_limit must be > 0");
        self.partition_batch_limit = n;
        self
    }

    /// Max inner drain iterations per partition before yielding. Default: 8.
    ///
    /// # Panics
    ///
    /// Panics if `n` is 0.
    #[must_use]
    pub fn max_inner_iterations(mut self, n: u32) -> Self {
        assert!(n > 0, "max_inner_iterations must be > 0");
        self.max_inner_iterations = n;
        self
    }

    /// Minimum interval between full vacuum sweeps. Default: 1h.
    #[must_use]
    pub fn vacuum_cooldown(mut self, d: Duration) -> Self {
        self.vacuum_cooldown = d;
        self
    }

    /// Periodic stats reporting interval. Default: 10s.
    /// `Duration::ZERO` disables stats collection entirely.
    #[must_use]
    pub fn stats_interval(mut self, d: Duration) -> Self {
        self.stats_interval = if d.is_zero() { None } else { Some(d) };
        self
    }

    /// Steady-state pace between worker executions. Default: 100ms.
    /// Enforces a minimum interval between consecutive executions even
    /// when healthy, preventing tight-loop spinning on the database.
    /// `Duration::ZERO` disables pacing (workers spin as fast as possible).
    #[must_use]
    pub fn steady_pace(mut self, d: Duration) -> Self {
        self.steady_pace = d;
        self
    }

    /// Begin building a queue registration.
    pub fn queue(self, name: &str, partitions: Partitions) -> QueueBuilder {
        QueueBuilder::new(self, name.to_owned(), partitions)
    }

    /// Spawn background tasks and return a handle to the running pipeline.
    ///
    /// Registers all queues in the database, creates the sequencer and
    /// per-partition processors, then starts them as background tasks.
    ///
    /// # Errors
    ///
    /// Returns an error if queue registration fails (DB operation).
    ///
    /// # Panics
    ///
    /// Panics if the internal stats registry mutex is poisoned.
    pub async fn start(mut self) -> Result<OutboxHandle, OutboxError> {
        // Build shared prioritizer first — Outbox and sequencer workers
        // both subscribe to its internal Notify for wakeups.
        let shared_prioritizer = Arc::new(SharedPrioritizer::new());

        let config = OutboxConfig {
            sequencer: SequencerConfig {
                batch_size: self.sequencer_batch_size,
                poll_interval: self.poll_interval,
                partition_batch_limit: self.partition_batch_limit,
                max_inner_iterations: self.max_inner_iterations,
            },
        };
        let outbox = Outbox::new(config);

        let outbox = Arc::new(outbox);
        let cancel = CancellationToken::new();
        let mut task_set = TaskSet::new(cancel.clone());
        let start_notify = Arc::new(Notify::new());
        let partition_notify: DashMap<i64, Arc<Notify>> = DashMap::new();

        // Global processor semaphore — caps total concurrent partition processors
        let processor_sem = Arc::new(Semaphore::new(
            self.processors
                .unwrap_or(Semaphore::MAX_PERMITS)
                .min(Semaphore::MAX_PERMITS),
        ));

        // Shared stats registry (wrapped in Mutex for processor factory access)
        let stats_registry_shared = if self.stats_interval.is_some() {
            Some(Arc::new(std::sync::Mutex::new(StatsRegistry::new())))
        } else {
            None
        };

        // Register queues and spawn processor workers via factories
        for decl in &mut self.queue_declarations {
            // Apply global poll_interval to each queue
            decl.config.poll_interval = self.poll_interval;

            outbox
                .register_queue(&self.db, &decl.name, decl.partitions.count())
                .await?;

            let partition_ids = outbox.partition_ids_for_queue(&decl.name);

            for &pid in &partition_ids {
                let notify = Arc::new(Notify::new());
                partition_notify.insert(pid, Arc::clone(&notify));
                let ctx = super::builder::SpawnContext {
                    pid,
                    db: self.db.clone(),
                    cancel: cancel.clone(),
                    partition_notify: notify,
                    processor_sem: Arc::clone(&processor_sem),
                    start_notify: Arc::clone(&start_notify),
                    outbox: Arc::clone(&outbox),
                    stats_registry: stats_registry_shared.clone(),
                };
                let (name, future) = decl.factory.spawn(ctx);
                task_set.spawn(name, future);
            }
        }

        // Two-tier maintenance semaphores
        let guaranteed = self.maintenance_guaranteed.unwrap_or(2);
        let shared = self.maintenance_shared.unwrap_or(1);
        let guaranteed_sem = Arc::new(Semaphore::new(guaranteed.min(Semaphore::MAX_PERMITS)));
        let shared_sem = Arc::new(Semaphore::new(shared.min(Semaphore::MAX_PERMITS)));

        // Collect per-partition notify map for the sequencer
        let mut notify_map: HashMap<i64, Arc<Notify>> = HashMap::new();
        for entry in &partition_notify {
            notify_map.insert(*entry.key(), Arc::clone(entry.value()));
        }
        let notify_map = Arc::new(notify_map);
        outbox.set_partition_notify(notify_map).await;

        outbox
            .set_prioritizer(Arc::clone(&shared_prioritizer))
            .await;

        // Eager reconciliation at startup: discover pending partitions
        // from the incoming table before spawning the sequencer.
        super::workers::reconciler::reconcile_dirty(&outbox, &self.db, &shared_prioritizer).await;

        // Spawn parallel sequencer workers (guaranteed + shared)
        let sequencer_count = guaranteed + shared;
        for i in 0..sequencer_count {
            #[allow(unused_mut)]
            let mut sequencer = Sequencer::new(
                outbox.config().sequencer.clone(),
                Arc::clone(&outbox),
                self.db.clone(),
                Arc::clone(&shared_prioritizer),
            );
            let name = format!("sequencer-{i}");
            let mut builder = WorkerBuilder::<SequencerReport>::new(&name, cancel.clone())
                .notifier(shared_prioritizer.notifier())
                .notifier(Arc::clone(&start_notify))
                .bulkhead(Bulkhead::new(
                    &name,
                    BulkheadConfig {
                        semaphore: ConcurrencyLimit::Tiered {
                            guaranteed: Arc::clone(&guaranteed_sem),
                            shared: Arc::clone(&shared_sem),
                        },
                        backoff: BackoffConfig::default(),
                        steady_pace: self.steady_pace,
                    },
                ))
                .listener(TracingListener)
                .on_panic(PanicPolicy::CatchAndRetry);

            if let Some(ref registry) = stats_registry_shared {
                let stats = StatsListener::new(Box::new(|any| {
                    any.downcast_ref::<SequencerReport>()
                        .map_or(0, |r| u64::from(r.rows_claimed))
                }));
                if let Ok(mut reg) = registry.lock() {
                    reg.register("sequencer".to_owned(), stats.clone());
                }
                builder = builder.listener(stats);
            }

            let worker = builder.build(sequencer);
            task_set.spawn(&name, worker.run());
        }

        // Spawn cold reconciler as a WorkerAction (ungated, poker-driven)
        {
            let reconciler = super::workers::reconciler::ColdReconciler {
                outbox: Arc::clone(&outbox),
                db: self.db.clone(),
                prioritizer: Arc::clone(&shared_prioritizer),
            };
            let name = "cold-reconciler";
            let worker = WorkerBuilder::new(name, cancel.clone())
                .with_poker(self.poker_interval)
                .notifier(Arc::clone(&start_notify))
                .listener(TracingListener)
                .on_panic(PanicPolicy::CatchAndRetry)
                .build(reconciler);
            task_set.spawn(name, worker.run());
        }

        // Spawn parallel vacuum workers (one per shared permit)
        for i in 0..shared {
            #[allow(unused_mut)]
            let vacuum = VacuumTask::new(self.db.clone(), self.vacuum_cooldown);
            let name = format!("vacuum-{i}");
            let mut builder = WorkerBuilder::<VacuumReport>::new(&name, cancel.clone())
                .with_poker(self.vacuum_cooldown)
                .notifier(Arc::clone(&start_notify))
                .bulkhead(Bulkhead::new(
                    &name,
                    BulkheadConfig {
                        semaphore: ConcurrencyLimit::Fixed(Arc::clone(&shared_sem)),
                        backoff: BackoffConfig {
                            initial: Duration::from_millis(500),
                            max: Duration::from_secs(60),
                            ..Default::default()
                        },
                        steady_pace: Duration::ZERO,
                    },
                ))
                .listener(TracingListener)
                .on_panic(PanicPolicy::CatchAndRetry);

            if let Some(ref registry) = stats_registry_shared {
                let stats = StatsListener::new(Box::new(|any| {
                    any.downcast_ref::<VacuumReport>()
                        .map_or(0, |r| r.rows_deleted)
                }));
                if let Ok(mut reg) = registry.lock() {
                    reg.register("vacuum".to_owned(), stats.clone());
                }
                builder = builder.listener(stats);
            }

            let worker = builder.build(vacuum);
            task_set.spawn(&name, worker.run());
        }

        // Spawn stats reporter (if enabled)
        if let Some(interval) = self.stats_interval {
            // Extract the registry — all workers have registered by now.
            #[allow(clippy::expect_used)]
            let registry = stats_registry_shared
                .expect("stats_registry_shared is Some when stats_interval is Some")
                .lock()
                .ok()
                .map(|mut guard| std::mem::replace(&mut *guard, StatsRegistry::new()))
                .expect("stats registry mutex not poisoned");
            let reporter = StatsReporter::new(Arc::new(registry));
            let name = "stats-reporter";
            let worker = WorkerBuilder::new(name, cancel.clone())
                .with_poker(interval)
                .on_panic(PanicPolicy::CatchAndRetry)
                .build(reporter);
            task_set.spawn(name, worker.run());
        }

        start_notify.notify_waiters();

        Ok(OutboxHandle {
            outbox,
            tasks: task_set,
        })
    }
}

/// A running outbox pipeline. Obtained by calling [`OutboxBuilder::start()`].
///
/// Provides access to the [`Outbox`] for enqueue operations and a
/// [`stop()`](Self::stop) method for graceful shutdown.
///
/// Drop safety: if `stop()` is never called, `TaskSet::Drop` cancels the
/// cancellation token, signaling all workers to exit.
pub struct OutboxHandle {
    outbox: Arc<Outbox>,
    tasks: TaskSet,
}

impl OutboxHandle {
    /// Returns the outbox for enqueue operations.
    #[must_use]
    pub fn outbox(&self) -> &Arc<Outbox> {
        &self.outbox
    }

    /// Cancel background tasks and join all handles. Consumes self.
    pub async fn stop(self) {
        self.tasks.shutdown().await;
    }
}
