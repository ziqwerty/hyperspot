use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;

use super::core::Outbox;
use super::handler::{
    Handler, MessageHandler, PerMessageAdapter, TransactionalHandler, TransactionalMessageHandler,
};
use super::manager::{OutboxBuilder, QueueDeclaration};
use super::stats::{StatsListener, StatsRegistry};
use super::strategy::{DecoupledStrategy, TransactionalStrategy, generate_worker_id};
use super::taskward::{
    BackoffConfig, Bulkhead, BulkheadConfig, ConcurrencyLimit, PanicPolicy, TracingListener,
    WorkerBuilder,
};
use super::types::{Partitions, QueueConfig};
use super::workers::processor::{PartitionProcessor, ProcessorReport};
use crate::Db;

/// All runtime context needed to spawn a processor worker.
/// Constructed once per partition in [`OutboxBuilder::start()`].
pub struct SpawnContext {
    pub pid: i64,
    pub db: Db,
    pub cancel: CancellationToken,
    pub partition_notify: Arc<Notify>,
    pub processor_sem: Arc<Semaphore>,
    pub start_notify: Arc<Notify>,
    #[allow(dead_code)]
    pub outbox: Arc<Outbox>,
    /// Shared stats registry for processor workers. `None` when stats disabled.
    pub stats_registry: Option<Arc<std::sync::Mutex<StatsRegistry>>>,
}

/// Trait for creating processor workers. One impl per processing mode.
pub trait ProcessorFactory: Send {
    fn spawn(&self, ctx: SpawnContext) -> (String, Pin<Box<dyn Future<Output = ()> + Send>>);
}

/// Shared worker assembly logic for all processor factories.
fn build_processor_worker<S: super::strategy::ProcessingStrategy + 'static>(
    ctx: &SpawnContext,
    strategy: S,
    config: QueueConfig,
) -> (String, Pin<Box<dyn Future<Output = ()> + Send>>) {
    let processor = PartitionProcessor::new(strategy, ctx.pid, config, ctx.db.clone());
    let name = format!("processor-{}", ctx.pid);
    let mut builder = WorkerBuilder::<ProcessorReport>::new(&name, ctx.cancel.clone())
        .notifier(Arc::clone(&ctx.partition_notify))
        .notifier(Arc::clone(&ctx.start_notify))
        .with_poker(processor.poll_interval())
        .bulkhead(Bulkhead::new(
            &name,
            BulkheadConfig {
                semaphore: ConcurrencyLimit::Fixed(Arc::clone(&ctx.processor_sem)),
                backoff: BackoffConfig::default(),
                steady_pace: Duration::ZERO,
            },
        ))
        .listener(TracingListener)
        .on_panic(PanicPolicy::CatchAndRetry);

    if let Some(ref registry) = ctx.stats_registry {
        let stats = StatsListener::new(Box::new(|any| {
            any.downcast_ref::<ProcessorReport>()
                .map_or(0, |r| u64::from(r.messages_processed))
        }));
        if let Ok(mut reg) = registry.lock() {
            reg.register("processor".to_owned(), stats.clone());
        }
        builder = builder.listener(stats);
    }

    let worker = builder.build(processor);
    (name, Box::pin(worker.run()))
}

/// Builder for registering a queue with per-queue configuration.
///
/// Obtained via [`OutboxBuilder::queue`]. Terminal methods (`transactional`,
/// `decoupled`, `batch_transactional`, `batch_decoupled`) register the queue
/// and return the parent [`OutboxBuilder`] for chaining.
#[must_use = "a queue builder does nothing until a handler is registered via .transactional() or .decoupled()"]
pub struct QueueBuilder {
    builder: OutboxBuilder,
    name: String,
    partitions: Partitions,
    config: QueueConfig,
}

impl QueueBuilder {
    pub(crate) fn new(builder: OutboxBuilder, name: String, partitions: Partitions) -> Self {
        Self {
            builder,
            name,
            partitions,
            config: QueueConfig::default(),
        }
    }

    /// Messages per handler call per partition.
    pub fn msg_batch_size(mut self, n: u32) -> Self {
        self.config.msg_batch_size = n;
        self
    }

    /// Base delay for exponential backoff on retry.
    pub fn backoff_base(mut self, d: Duration) -> Self {
        self.config.backoff_base = d;
        self
    }

    /// Maximum delay for exponential backoff on retry.
    pub fn backoff_max(mut self, d: Duration) -> Self {
        self.config.backoff_max = d;
        self
    }

    /// Register a single-message transactional handler (common case).
    ///
    /// Forces `msg_batch_size = 1` — the `PerMessageAdapter` adapter processes
    /// one message at a time, so fetching larger batches would be wasteful.
    #[must_use]
    pub fn transactional(
        mut self,
        handler: impl TransactionalMessageHandler + 'static,
    ) -> OutboxBuilder {
        self.config.msg_batch_size = 1;
        self.batch_transactional(PerMessageAdapter::new(handler))
    }

    /// Register a single-message decoupled handler (common case).
    ///
    /// Forces `msg_batch_size = 1` — the `PerMessageAdapter` adapter processes
    /// one message at a time, so fetching larger batches would be wasteful.
    #[must_use]
    pub fn decoupled(mut self, handler: impl MessageHandler + 'static) -> OutboxBuilder {
        self.config.msg_batch_size = 1;
        self.batch_decoupled(PerMessageAdapter::new(handler))
    }

    /// Register a single-message decoupled handler with explicit configuration.
    ///
    /// Forces `msg_batch_size = 1`. Use this to customize `lease_duration`
    /// and other decoupled-specific settings.
    #[must_use]
    pub fn decoupled_with(
        mut self,
        handler: impl MessageHandler + 'static,
        config: super::types::DecoupledConfig,
    ) -> OutboxBuilder {
        self.config.msg_batch_size = 1;
        let super::types::DecoupledConfig { lease_duration } = config;
        self.config.lease_duration = lease_duration;
        self.batch_decoupled(PerMessageAdapter::new(handler))
    }

    /// Register a batch transactional handler (advanced).
    #[must_use]
    pub fn batch_transactional(
        self,
        handler: impl TransactionalHandler + 'static,
    ) -> OutboxBuilder {
        let factory = TransactionalProcessorFactory {
            handler: Arc::new(handler),
            config: self.config.clone(),
        };

        let mut builder = self.builder;
        builder.queue_declarations.push(QueueDeclaration {
            name: self.name,
            partitions: self.partitions,
            config: self.config,
            factory: Box::new(factory),
        });
        builder
    }

    /// Register a batch decoupled handler (advanced).
    #[must_use]
    pub fn batch_decoupled(self, handler: impl Handler + 'static) -> OutboxBuilder {
        let factory = DecoupledProcessorFactory {
            handler: Arc::new(handler),
            config: self.config.clone(),
            queue_name: self.name.clone(),
        };

        let mut builder = self.builder;
        builder.queue_declarations.push(QueueDeclaration {
            name: self.name,
            partitions: self.partitions,
            config: self.config,
            factory: Box::new(factory),
        });
        builder
    }

    /// Register a batch decoupled handler with explicit configuration (advanced).
    #[must_use]
    pub fn batch_decoupled_with(
        mut self,
        handler: impl Handler + 'static,
        config: super::types::DecoupledConfig,
    ) -> OutboxBuilder {
        let super::types::DecoupledConfig { lease_duration } = config;
        self.config.lease_duration = lease_duration;
        self.batch_decoupled(handler)
    }
}

// --- Factory implementations ---

struct TransactionalProcessorFactory<H: TransactionalHandler> {
    handler: Arc<H>,
    config: QueueConfig,
}

impl<H: TransactionalHandler + 'static> ProcessorFactory for TransactionalProcessorFactory<H> {
    fn spawn(&self, ctx: SpawnContext) -> (String, Pin<Box<dyn Future<Output = ()> + Send>>) {
        let strategy = TransactionalStrategy::new(Box::new(ArcTransactionalHandler(Arc::clone(
            &self.handler,
        ))));
        build_processor_worker(&ctx, strategy, self.config.clone())
    }
}

struct DecoupledProcessorFactory<H: Handler> {
    handler: Arc<H>,
    config: QueueConfig,
    queue_name: String,
}

impl<H: Handler + 'static> ProcessorFactory for DecoupledProcessorFactory<H> {
    fn spawn(&self, ctx: SpawnContext) -> (String, Pin<Box<dyn Future<Output = ()> + Send>>) {
        let worker_id = generate_worker_id(&self.queue_name);
        let strategy =
            DecoupledStrategy::new(Box::new(ArcHandler(Arc::clone(&self.handler))), worker_id);
        build_processor_worker(&ctx, strategy, self.config.clone())
    }
}

// Arc wrappers to share handler across partitions

struct ArcTransactionalHandler<H: TransactionalHandler>(Arc<H>);

#[async_trait::async_trait]
impl<H: TransactionalHandler> TransactionalHandler for ArcTransactionalHandler<H> {
    async fn handle(
        &self,
        txn: &dyn sea_orm::ConnectionTrait,
        msgs: &[super::handler::OutboxMessage],
        cancel: CancellationToken,
    ) -> super::handler::HandlerResult {
        self.0.handle(txn, msgs, cancel).await
    }

    fn processed_count(&self) -> Option<usize> {
        self.0.processed_count()
    }
}

struct ArcHandler<H: Handler>(Arc<H>);

#[async_trait::async_trait]
impl<H: Handler> Handler for ArcHandler<H> {
    async fn handle(
        &self,
        msgs: &[super::handler::OutboxMessage],
        cancel: CancellationToken,
    ) -> super::handler::HandlerResult {
        self.0.handle(msgs, cancel).await
    }

    fn processed_count(&self) -> Option<usize> {
        self.0.processed_count()
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::outbox::types::DEFAULT_LEASE_DURATION;

    #[test]
    fn queue_config_defaults() {
        let config = QueueConfig::default();
        assert_eq!(config.lease_duration, DEFAULT_LEASE_DURATION);
        assert_eq!(config.msg_batch_size, 1);
    }

    #[test]
    fn partitions_count() {
        assert_eq!(Partitions::of(1).count(), 1);
        assert_eq!(Partitions::of(2).count(), 2);
        assert_eq!(Partitions::of(4).count(), 4);
        assert_eq!(Partitions::of(8).count(), 8);
        assert_eq!(Partitions::of(16).count(), 16);
        assert_eq!(Partitions::of(32).count(), 32);
        assert_eq!(Partitions::of(64).count(), 64);
    }
}
