use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::{PublishError, UsageEvent};
use modkit_db::outbox::Outbox;
use tracing::{info, warn};

use crate::domain::error::DomainError;
use crate::domain::repos::{AttachmentCleanupEvent, OutboxEnqueuer};

/// Infrastructure implementation of [`OutboxEnqueuer`].
///
/// Serializes `UsageEvent` to JSON and inserts into the outbox table
/// within the caller's transaction via `modkit_db::outbox::Outbox::enqueue()`.
pub struct InfraOutboxEnqueuer {
    outbox: Arc<Outbox>,
    queue_name: String,
    cleanup_queue_name: String,
    num_partitions: u32,
}

impl InfraOutboxEnqueuer {
    pub(crate) fn new(
        outbox: Arc<Outbox>,
        queue_name: String,
        cleanup_queue_name: String,
        num_partitions: u32,
    ) -> Self {
        Self {
            outbox,
            queue_name,
            cleanup_queue_name,
            num_partitions,
        }
    }

    fn partition_for(&self, tenant_id: uuid::Uuid) -> u32 {
        Self::compute_partition(tenant_id, self.num_partitions)
    }

    fn compute_partition(tenant_id: uuid::Uuid, num_partitions: u32) -> u32 {
        let hash = tenant_id.as_u128();
        #[allow(clippy::cast_possible_truncation)]
        {
            (hash % u128::from(num_partitions)) as u32
        }
    }
}

#[async_trait]
impl OutboxEnqueuer for InfraOutboxEnqueuer {
    async fn enqueue_usage_event(
        &self,
        runner: &(dyn modkit_db::secure::DBRunner + Sync),
        event: UsageEvent,
    ) -> Result<(), DomainError> {
        let partition = self.partition_for(event.tenant_id);
        let payload = serde_json::to_vec(&event)
            .map_err(|e| DomainError::internal(format!("serialize UsageEvent: {e}")))?;

        self.outbox
            .enqueue(
                runner,
                &self.queue_name,
                partition,
                payload,
                "application/json",
            )
            .await
            .map_err(|e| DomainError::internal(format!("outbox enqueue: {e}")))?;

        info!(
            queue = %self.queue_name,
            partition,
            tenant_id = %event.tenant_id,
            turn_id = %event.turn_id,
            "usage event enqueued"
        );

        Ok(())
    }

    async fn enqueue_attachment_cleanup(
        &self,
        runner: &(dyn modkit_db::secure::DBRunner + Sync),
        event: AttachmentCleanupEvent,
    ) -> Result<(), DomainError> {
        let partition = self.partition_for(event.tenant_id);
        let payload = serde_json::to_vec(&event)
            .map_err(|e| DomainError::internal(format!("serialize AttachmentCleanupEvent: {e}")))?;

        self.outbox
            .enqueue(
                runner,
                &self.cleanup_queue_name,
                partition,
                payload,
                "application/json",
            )
            .await
            .map_err(|e| DomainError::internal(format!("outbox enqueue: {e}")))?;

        info!(
            queue = %self.cleanup_queue_name,
            partition,
            tenant_id = %event.tenant_id,
            attachment_id = %event.attachment_id,
            "attachment cleanup event enqueued"
        );

        Ok(())
    }

    fn flush(&self) {
        self.outbox.flush();
    }
}

/// Stub handler for attachment cleanup events.
///
/// Returns `Retry` for every message — events accumulate safely in the outbox
/// until the cleanup worker ships. This ensures the queue is registered and
/// partitioned from day one.
pub struct AttachmentCleanupHandler;

#[async_trait]
impl modkit_db::outbox::MessageHandler for AttachmentCleanupHandler {
    async fn handle(
        &self,
        msg: &modkit_db::outbox::OutboxMessage,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> modkit_db::outbox::HandlerResult {
        warn!(
            partition_id = msg.partition_id,
            seq = msg.seq,
            "attachment cleanup handler not yet implemented - retrying"
        );
        modkit_db::outbox::HandlerResult::Retry {
            reason: "cleanup handler not yet implemented".to_owned(),
        }
    }
}

/// Trait for lazily resolving the model-policy plugin.
///
/// Production code uses `ModelPolicyGateway` (lazy GTS resolution).
/// Tests provide a direct `Arc<dyn MiniChatModelPolicyPluginClientV1>`.
#[async_trait]
pub trait PolicyPluginProvider: Send + Sync {
    async fn get_plugin(
        &self,
    ) -> Result<
        Arc<dyn mini_chat_sdk::MiniChatModelPolicyPluginClientV1>,
        crate::domain::error::DomainError,
    >;
}

#[async_trait]
impl PolicyPluginProvider for crate::infra::model_policy::ModelPolicyGateway {
    async fn get_plugin(
        &self,
    ) -> Result<
        Arc<dyn mini_chat_sdk::MiniChatModelPolicyPluginClientV1>,
        crate::domain::error::DomainError,
    > {
        self.get_policy_plugin().await
    }
}

/// Delivers usage events to the model-policy plugin via `publish_usage()`.
///
/// Deserializes `OutboxMessage.payload` into `UsageEvent`, resolves the plugin
/// lazily via [`PolicyPluginProvider`], calls `publish_usage()`, and maps
/// `PublishError` variants to outbox `HandlerResult`:
/// - `Ok(())` → `Success` (ack + advance cursor)
/// - `PublishError::Transient` → `Retry` (exponential backoff, redelivery)
/// - `PublishError::Permanent` → `Reject` (dead-letter for manual inspection)
/// - Deserialization failure → `Reject` (corrupt payload, permanent)
/// - Plugin resolution failure → `Retry` (transient - plugin may not be ready yet)
pub struct UsageEventHandler {
    pub(crate) plugin_provider: Arc<dyn PolicyPluginProvider>,
}

#[async_trait]
impl modkit_db::outbox::MessageHandler for UsageEventHandler {
    async fn handle(
        &self,
        msg: &modkit_db::outbox::OutboxMessage,
        cancel: tokio_util::sync::CancellationToken,
    ) -> modkit_db::outbox::HandlerResult {
        let event = match serde_json::from_slice::<UsageEvent>(&msg.payload) {
            Ok(e) => e,
            Err(e) => {
                tracing::error!(
                    partition_id = msg.partition_id,
                    seq = msg.seq,
                    payload_len = msg.payload.len(),
                    "usage event deserialization failed: {e}"
                );
                return modkit_db::outbox::HandlerResult::Reject {
                    reason: format!("deserialization failed: {e}"),
                };
            }
        };

        info!(
            tenant_id = %event.tenant_id,
            user_id = %event.user_id,
            turn_id = %event.turn_id,
            request_id = %event.request_id,
            effective_model = %event.effective_model,
            billing_outcome = ?event.billing_outcome,
            settlement_method = ?event.settlement_method,
            actual_credits_micro = event.actual_credits_micro,
            partition_id = msg.partition_id,
            seq = msg.seq,
            "publishing usage event to plugin"
        );

        let plugin = match self.plugin_provider.get_plugin().await {
            Ok(p) => p,
            Err(e) => {
                warn!(
                    partition_id = msg.partition_id,
                    seq = msg.seq,
                    error = %e,
                    "failed to resolve policy plugin - will retry"
                );
                return modkit_db::outbox::HandlerResult::Retry {
                    reason: format!("plugin resolution failed: {e}"),
                };
            }
        };

        match plugin.publish_usage(event, cancel).await {
            Ok(()) => modkit_db::outbox::HandlerResult::Success,
            Err(PublishError::Transient(reason)) => {
                warn!(
                    partition_id = msg.partition_id,
                    seq = msg.seq,
                    %reason,
                    "publish_usage transient failure - will retry"
                );
                modkit_db::outbox::HandlerResult::Retry { reason }
            }
            Err(PublishError::Permanent(reason)) => {
                tracing::error!(
                    partition_id = msg.partition_id,
                    seq = msg.seq,
                    %reason,
                    "publish_usage permanent failure - dead-lettering"
                );
                modkit_db::outbox::HandlerResult::Reject { reason }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mini_chat_sdk::{
        MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginError, PolicySnapshot,
        PolicyVersionInfo, PublishError, UserLimits,
    };
    use modkit_db::outbox::{HandlerResult, MessageHandler, OutboxMessage};
    use std::sync::atomic::{AtomicU32, Ordering};
    use time::OffsetDateTime;
    use tokio_util::sync::CancellationToken;
    use uuid::Uuid;

    fn make_usage_event() -> UsageEvent {
        UsageEvent {
            tenant_id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            chat_id: Uuid::new_v4(),
            turn_id: Uuid::new_v4(),
            request_id: Uuid::new_v4(),
            effective_model: "gpt-4o".to_owned(),
            selected_model: "gpt-4o".to_owned(),
            terminal_state: "completed".to_owned(),
            billing_outcome: "charged".to_owned(),
            usage: None,
            actual_credits_micro: 500,
            settlement_method: "quota".to_owned(),
            policy_version_applied: 1,
            timestamp: OffsetDateTime::now_utc(),
        }
    }

    fn make_outbox_message(payload: Vec<u8>) -> OutboxMessage {
        OutboxMessage {
            partition_id: 1,
            seq: 42,
            payload,
            payload_type: "application/json".to_owned(),
            created_at: chrono::Utc::now(),
            attempts: 0,
        }
    }

    /// Mock plugin that records `publish_usage` calls and returns a configurable result.
    struct MockPlugin {
        result: std::sync::Mutex<Result<(), PublishError>>,
        call_count: AtomicU32,
        notifier: tokio::sync::Notify,
    }

    impl MockPlugin {
        fn ok() -> Arc<Self> {
            Arc::new(Self {
                result: std::sync::Mutex::new(Ok(())),
                call_count: AtomicU32::new(0),
                notifier: tokio::sync::Notify::new(),
            })
        }

        fn transient(reason: &str) -> Arc<Self> {
            Arc::new(Self {
                result: std::sync::Mutex::new(Err(PublishError::Transient(reason.to_owned()))),
                call_count: AtomicU32::new(0),
                notifier: tokio::sync::Notify::new(),
            })
        }

        fn permanent(reason: &str) -> Arc<Self> {
            Arc::new(Self {
                result: std::sync::Mutex::new(Err(PublishError::Permanent(reason.to_owned()))),
                call_count: AtomicU32::new(0),
                notifier: tokio::sync::Notify::new(),
            })
        }

        fn calls(&self) -> u32 {
            self.call_count.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl MiniChatModelPolicyPluginClientV1 for MockPlugin {
        async fn get_current_policy_version(
            &self,
            _user_id: Uuid,
            _cancel: CancellationToken,
        ) -> Result<PolicyVersionInfo, MiniChatModelPolicyPluginError> {
            unimplemented!("not needed in outbox tests")
        }

        async fn get_policy_snapshot(
            &self,
            _user_id: Uuid,
            _policy_version: u64,
            _cancel: CancellationToken,
        ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError> {
            unimplemented!("not needed in outbox tests")
        }

        async fn get_user_limits(
            &self,
            _user_id: Uuid,
            _policy_version: u64,
            _cancel: CancellationToken,
        ) -> Result<UserLimits, MiniChatModelPolicyPluginError> {
            unimplemented!("not needed in outbox tests")
        }

        async fn check_user_license(
            &self,
            _user_id: Uuid,
            _cancel: CancellationToken,
        ) -> Result<mini_chat_sdk::UserLicenseStatus, MiniChatModelPolicyPluginError> {
            unimplemented!("not needed in outbox tests")
        }

        async fn publish_usage(
            &self,
            _payload: UsageEvent,
            _cancel: CancellationToken,
        ) -> Result<(), PublishError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            let result = {
                let guard = self.result.lock().unwrap();
                match &*guard {
                    Ok(()) => Ok(()),
                    Err(PublishError::Transient(r)) => Err(PublishError::Transient(r.clone())),
                    Err(PublishError::Permanent(r)) => Err(PublishError::Permanent(r.clone())),
                }
            };
            self.notifier.notify_one();
            result
        }
    }

    /// Wraps a mock plugin as a [`PolicyPluginProvider`] for tests.
    struct MockProvider {
        plugin: Arc<dyn MiniChatModelPolicyPluginClientV1>,
    }

    #[async_trait]
    impl PolicyPluginProvider for MockProvider {
        async fn get_plugin(
            &self,
        ) -> Result<Arc<dyn MiniChatModelPolicyPluginClientV1>, crate::domain::error::DomainError>
        {
            Ok(self.plugin.clone())
        }
    }

    fn make_handler(plugin: &Arc<dyn MiniChatModelPolicyPluginClientV1>) -> UsageEventHandler {
        UsageEventHandler {
            plugin_provider: Arc::new(MockProvider {
                plugin: plugin.clone(),
            }),
        }
    }

    // ── 7.1: partition_for returns values in [0, num_partitions) ──

    #[test]
    fn partition_for_returns_in_range() {
        for num_partitions in [1, 2, 4, 8, 16, 32, 64] {
            for _ in 0..100 {
                let tenant_id = Uuid::new_v4();
                let p = InfraOutboxEnqueuer::compute_partition(tenant_id, num_partitions);
                assert!(
                    p < num_partitions,
                    "partition {p} >= num_partitions {num_partitions} for tenant {tenant_id}"
                );
            }
        }
    }

    #[test]
    fn partition_for_deterministic() {
        let tenant_id = Uuid::new_v4();
        let a = InfraOutboxEnqueuer::compute_partition(tenant_id, 4);
        let b = InfraOutboxEnqueuer::compute_partition(tenant_id, 4);
        assert_eq!(a, b);
    }

    // ── 7.2 / 7.7: UsageEventHandler returns Success when plugin returns Ok ──

    #[tokio::test]
    async fn handler_success_for_valid_event() {
        let plugin = MockPlugin::ok();
        let handler = make_handler(&(plugin.clone() as Arc<dyn MiniChatModelPolicyPluginClientV1>));
        let event = make_usage_event();
        let payload = serde_json::to_vec(&event).unwrap();
        let msg = make_outbox_message(payload);

        let result = handler.handle(&msg, CancellationToken::new()).await;
        assert!(matches!(result, HandlerResult::Success));
        assert_eq!(plugin.calls(), 1);
    }

    // ── 7.3: UsageEventHandler returns Reject for invalid payload ──

    #[tokio::test]
    async fn handler_reject_for_invalid_payload() {
        let plugin = MockPlugin::ok();
        let handler = make_handler(&(plugin.clone() as Arc<dyn MiniChatModelPolicyPluginClientV1>));
        let msg = make_outbox_message(b"not json".to_vec());

        let result = handler.handle(&msg, CancellationToken::new()).await;
        match result {
            HandlerResult::Reject { reason } => {
                assert!(
                    reason.contains("deserialization failed"),
                    "unexpected reason: {reason}"
                );
            }
            HandlerResult::Success => panic!("expected Reject, got Success"),
            HandlerResult::Retry { reason } => panic!("expected Reject, got Retry({reason})"),
        }
        // Plugin should not be called for invalid payload.
        assert_eq!(plugin.calls(), 0);
    }

    // ── 7.8: UsageEventHandler returns Retry on PublishError::Transient ──

    #[tokio::test]
    async fn handler_retry_on_transient_error() {
        let plugin = MockPlugin::transient("network timeout");
        let handler = make_handler(&(plugin.clone() as Arc<dyn MiniChatModelPolicyPluginClientV1>));
        let event = make_usage_event();
        let payload = serde_json::to_vec(&event).unwrap();
        let msg = make_outbox_message(payload);

        let result = handler.handle(&msg, CancellationToken::new()).await;
        match result {
            HandlerResult::Retry { reason } => {
                assert_eq!(reason, "network timeout");
            }
            HandlerResult::Success => panic!("expected Retry, got Success"),
            HandlerResult::Reject { reason } => {
                panic!("expected Retry, got Reject({reason})")
            }
        }
        assert_eq!(plugin.calls(), 1);
    }

    // ── 7.9: UsageEventHandler returns Reject on PublishError::Permanent ──

    #[tokio::test]
    async fn handler_reject_on_permanent_error() {
        let plugin = MockPlugin::permanent("schema mismatch");
        let handler = make_handler(&(plugin.clone() as Arc<dyn MiniChatModelPolicyPluginClientV1>));
        let event = make_usage_event();
        let payload = serde_json::to_vec(&event).unwrap();
        let msg = make_outbox_message(payload);

        let result = handler.handle(&msg, CancellationToken::new()).await;
        match result {
            HandlerResult::Reject { reason } => {
                assert_eq!(reason, "schema mismatch");
            }
            HandlerResult::Success => panic!("expected Reject, got Success"),
            HandlerResult::Retry { reason } => {
                panic!("expected Reject, got Retry({reason})")
            }
        }
        assert_eq!(plugin.calls(), 1);
    }

    // ── 7.5 / 7.10: Integration test — full pipeline with mock plugin ──

    #[tokio::test]
    async fn full_pipeline_enqueue_and_deliver() {
        use modkit_db::outbox::{Outbox, Partitions};
        use modkit_db::{ConnectOpts, connect_db, migration_runner::run_migrations_for_testing};
        use std::time::Duration;

        // Mock plugin that tracks calls.
        let plugin = MockPlugin::ok();

        // Set up in-memory DB with outbox migrations.
        let db = connect_db(
            "sqlite:file:outbox_integration?mode=memory&cache=shared",
            ConnectOpts {
                max_conns: Some(1),
                ..Default::default()
            },
        )
        .await
        .expect("connect");

        run_migrations_for_testing(&db, modkit_db::outbox::outbox_migrations())
            .await
            .expect("outbox migrations");

        // Start outbox pipeline with the real UsageEventHandler + mock plugin.
        let handle = Outbox::builder(db.clone())
            .poll_interval(Duration::from_millis(20))
            .queue("test.usage", Partitions::of(1))
            .decoupled(UsageEventHandler {
                plugin_provider: Arc::new(MockProvider {
                    plugin: plugin.clone(),
                }),
            })
            .start()
            .await
            .expect("outbox start");

        let outbox = Arc::clone(handle.outbox());

        // Enqueue a usage event using InfraOutboxEnqueuer.
        let enqueuer = InfraOutboxEnqueuer::new(
            outbox,
            "test.usage".to_owned(),
            "test.cleanup".to_owned(),
            1,
        );
        let event = make_usage_event();
        let conn = db.conn().expect("conn");
        enqueuer
            .enqueue_usage_event(&conn, event)
            .await
            .expect("enqueue");
        enqueuer.flush();

        // Wait for the handler to process (notification-based, no fixed sleep).
        tokio::time::timeout(Duration::from_secs(5), plugin.notifier.notified())
            .await
            .expect("plugin should have been called within 5s");

        assert_eq!(
            plugin.calls(),
            1,
            "publish_usage should have been called once"
        );

        handle.stop().await;
    }
}
