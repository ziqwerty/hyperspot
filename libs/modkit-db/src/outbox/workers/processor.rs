use sea_orm::ConnectionTrait;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use super::super::handler::HandlerResult;
use super::super::strategy::{ProcessContext, ProcessingStrategy};
use super::super::taskward::{Directive, WorkerAction};
use super::super::types::{OutboxError, QueueConfig};
use crate::Db;

/// Report emitted by a processor execution cycle.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ProcessorReport {
    /// The partition that was processed.
    pub partition_id: i64,
    /// Number of messages dispatched to the handler.
    pub messages_processed: u32,
    /// Outcome of the handler invocation.
    pub handler_result: HandlerResult,
}

/// Per-partition adaptive batch sizing state machine.
///
/// Degrades to single-message mode on failure, ramps back up on consecutive
/// successes. Analogous to TCP slow start.
#[derive(Debug, Clone)]
pub enum PartitionMode {
    /// Normal operation — use configured `msg_batch_size`.
    Normal,
    /// Degraded after failure — process fewer messages at a time.
    /// Ramps back up (doubling) on consecutive successes until reaching
    /// the configured `msg_batch_size`, then transitions back to `Normal`.
    Degraded {
        effective_size: u32,
        consecutive_successes: u32,
    },
}

impl PartitionMode {
    /// Returns the effective batch size for this mode.
    pub(crate) fn effective_batch_size(&self, configured: u32) -> u32 {
        match self {
            Self::Normal => configured,
            Self::Degraded { effective_size, .. } => *effective_size,
        }
    }

    /// Transition after a handler result.
    ///
    /// `processed_count`: how many messages were successfully processed before
    /// the batch ended. `Some` for `PerMessageAdapter` handlers, `None` for batch
    /// handlers. On Retry/Reject, degradation uses `max(pc, 1)` when known,
    /// or falls back to 1 when `None` (batch handler — we can't know where
    /// the failure occurred).
    pub(crate) fn transition(
        &mut self,
        result: &HandlerResult,
        configured_batch_size: u32,
        processed_count: Option<u32>,
    ) {
        match result {
            HandlerResult::Success => match self {
                Self::Normal => {}
                Self::Degraded {
                    effective_size,
                    consecutive_successes,
                } => {
                    *consecutive_successes += 1;
                    // Double the effective size on each consecutive success
                    let next = effective_size.saturating_mul(2).min(configured_batch_size);
                    if next >= configured_batch_size {
                        *self = Self::Normal;
                    } else {
                        *effective_size = next;
                    }
                }
            },
            HandlerResult::Retry { .. } | HandlerResult::Reject { .. } => {
                // Degrade: use max(processed_count, 1) as the new effective
                // size. If the handler processed some messages before failing,
                // we know the failure is at position pc+1, so we degrade to
                // max(pc, 1) to isolate the poison message. For batch handlers
                // (None), fall back to 1 (most conservative).
                let degrade_to = processed_count.map_or(1, |pc| pc.max(1));
                *self = Self::Degraded {
                    effective_size: degrade_to,
                    consecutive_successes: 0,
                };
            }
        }
    }
}

/// A per-partition processor parameterized by its processing strategy.
///
/// Each instance owns exactly one `partition_id` and runs as a long-lived
/// tokio task. The strategy (`TransactionalStrategy` or `DecoupledStrategy`)
/// is baked in at compile time via monomorphization.
pub struct PartitionProcessor<S: ProcessingStrategy> {
    strategy: S,
    partition_id: i64,
    config: QueueConfig,
    db: Db,
    partition_mode: PartitionMode,
}

impl<S: ProcessingStrategy> PartitionProcessor<S> {
    pub fn new(strategy: S, partition_id: i64, config: QueueConfig, db: Db) -> Self {
        Self {
            strategy,
            partition_id,
            config,
            db,
            partition_mode: PartitionMode::Normal,
        }
    }

    /// Returns the configured poll interval for this processor.
    pub fn poll_interval(&self) -> std::time::Duration {
        self.config.poll_interval
    }
}

impl<S: ProcessingStrategy> WorkerAction for PartitionProcessor<S> {
    type Payload = ProcessorReport;
    type Error = OutboxError;

    async fn execute(
        &mut self,
        cancel: &CancellationToken,
    ) -> Result<Directive<ProcessorReport>, OutboxError> {
        let (backend, dialect) = {
            let sea_conn = self.db.sea_internal();
            let b = sea_conn.get_database_backend();
            (b, super::super::dialect::Dialect::from(b))
        };

        let effective_size = self
            .partition_mode
            .effective_batch_size(self.config.msg_batch_size);

        let ctx = ProcessContext {
            db: &self.db,
            backend,
            dialect,
            partition_id: self.partition_id,
        };

        let mut effective_config = self.config.clone();
        effective_config.msg_batch_size = effective_size;

        let child_cancel = cancel.child_token();

        let result = self
            .strategy
            .process(&ctx, &effective_config, child_cancel)
            .await?;

        if let Some(pr) = result {
            let has_more = pr.count >= effective_size;
            let clamped_pc = pr.processed_count.map(|pc| pc.min(pr.count));
            self.partition_mode.transition(
                &pr.handler_result,
                self.config.msg_batch_size,
                clamped_pc,
            );
            if pr.count > 0 {
                debug!(
                    partition_id = self.partition_id,
                    count = pr.count,
                    mode = ?self.partition_mode,
                    "partition batch complete"
                );
            }
            let report = ProcessorReport {
                partition_id: self.partition_id,
                messages_processed: pr.count,
                handler_result: pr.handler_result,
            };
            if has_more {
                Ok(Directive::Proceed(report))
            } else {
                Ok(Directive::Idle(report))
            }
        } else {
            Ok(Directive::Idle(ProcessorReport {
                partition_id: self.partition_id,
                messages_processed: 0,
                handler_result: HandlerResult::Success,
            }))
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    // ---- PartitionMode state machine tests ----

    #[test]
    fn partition_mode_normal_uses_configured_size() {
        let mode = PartitionMode::Normal;
        assert_eq!(mode.effective_batch_size(50), 50);
    }

    #[test]
    fn partition_mode_degraded_uses_effective_size() {
        let mode = PartitionMode::Degraded {
            effective_size: 4,
            consecutive_successes: 2,
        };
        assert_eq!(mode.effective_batch_size(50), 4);
    }

    #[test]
    fn partition_mode_retry_degrades_to_one() {
        let mut mode = PartitionMode::Normal;
        mode.transition(
            &HandlerResult::Retry {
                reason: "fail".into(),
            },
            50,
            None, // batch handler
        );
        assert!(matches!(
            mode,
            PartitionMode::Degraded {
                effective_size: 1,
                consecutive_successes: 0,
            }
        ));
    }

    #[test]
    fn partition_mode_success_ramps_up() {
        let mut mode = PartitionMode::Degraded {
            effective_size: 1,
            consecutive_successes: 0,
        };
        // 1 → 2
        mode.transition(&HandlerResult::Success, 50, None);
        assert!(matches!(
            mode,
            PartitionMode::Degraded {
                effective_size: 2,
                consecutive_successes: 1,
            }
        ));
        // 2 → 4
        mode.transition(&HandlerResult::Success, 50, None);
        assert!(matches!(
            mode,
            PartitionMode::Degraded {
                effective_size: 4,
                ..
            }
        ));
        // 4 → 8
        mode.transition(&HandlerResult::Success, 50, None);
        assert!(matches!(
            mode,
            PartitionMode::Degraded {
                effective_size: 8,
                ..
            }
        ));
    }

    #[test]
    fn partition_mode_ramps_up_to_normal() {
        let mut mode = PartitionMode::Degraded {
            effective_size: 16,
            consecutive_successes: 4,
        };
        // 16 → 32
        mode.transition(&HandlerResult::Success, 32, None);
        // Should transition back to Normal since 32 >= configured(32)
        assert!(matches!(mode, PartitionMode::Normal));
    }

    #[test]
    fn partition_mode_reject_in_normal_degrades() {
        let mut mode = PartitionMode::Normal;
        mode.transition(
            &HandlerResult::Reject {
                reason: "bad".into(),
            },
            50,
            None, // batch handler — falls back to 1
        );
        assert!(matches!(
            mode,
            PartitionMode::Degraded {
                effective_size: 1,
                consecutive_successes: 0,
            }
        ));
    }

    #[test]
    fn partition_mode_reject_with_processed_count() {
        // PerMessageAdapter handler processed 3 msgs before poison at index 3
        let mut mode = PartitionMode::Normal;
        mode.transition(
            &HandlerResult::Reject {
                reason: "bad".into(),
            },
            50,
            Some(3), // PerMessageAdapter processed 3 successfully
        );
        assert!(matches!(
            mode,
            PartitionMode::Degraded {
                effective_size: 3,
                consecutive_successes: 0,
            }
        ));
    }

    #[test]
    fn partition_mode_retry_with_processed_count_zero() {
        // PerMessageAdapter failed at the very first message
        let mut mode = PartitionMode::Normal;
        mode.transition(
            &HandlerResult::Retry {
                reason: "fail".into(),
            },
            50,
            Some(0), // failed at first message
        );
        // max(0, 1) = 1
        assert!(matches!(
            mode,
            PartitionMode::Degraded {
                effective_size: 1,
                consecutive_successes: 0,
            }
        ));
    }

    #[test]
    fn partition_mode_success_in_normal_stays_normal() {
        let mut mode = PartitionMode::Normal;
        mode.transition(&HandlerResult::Success, 50, None);
        assert!(matches!(mode, PartitionMode::Normal));
    }

    #[test]
    fn partition_mode_full_recovery_cycle() {
        let mut mode = PartitionMode::Normal;

        // Retry → Degraded(1)
        mode.transition(&HandlerResult::Retry { reason: "x".into() }, 8, None);
        assert_eq!(mode.effective_batch_size(8), 1);

        // Success: 1→2→4→8→Normal
        mode.transition(&HandlerResult::Success, 8, None);
        assert_eq!(mode.effective_batch_size(8), 2);
        mode.transition(&HandlerResult::Success, 8, None);
        assert_eq!(mode.effective_batch_size(8), 4);
        mode.transition(&HandlerResult::Success, 8, None);
        assert!(matches!(mode, PartitionMode::Normal));
        assert_eq!(mode.effective_batch_size(8), 8);
    }
}
