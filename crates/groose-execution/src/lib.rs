pub mod checkpoints;
pub mod control;
pub mod engine;
pub mod verifier;
pub mod workspace;

pub use checkpoints::{CheckpointStore, ExecutionCheckpoint};
pub use control::{ControlReceiver, ControlSignal, NoopControlReceiver};
pub use engine::{CodingEngine, EngineError, ExecutionContext, ExecutionOutcome};
pub use verifier::{VerificationResult, Verifier};
pub use workspace::{Workspace, WorkspaceMode};

use async_trait::async_trait;
use tokio::sync::broadcast;

/// A sink for progress events and artifacts produced by the engine.
#[async_trait]
pub trait EventSink: Send + Sync {
    async fn emit(&self, event: EngineEvent);
}

/// Events produced during coding engine execution.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineEvent {
    /// Engine started execution for a task.
    Started { task_id: String },
    /// Engine formed or revised a plan.
    PlanFormed { task_id: String, plan: String },
    /// An action was taken (tool call).
    Action { task_id: String, action: String },
    /// An observation or result from an action.
    Observation {
        task_id: String,
        observation: String,
    },
    /// A checkpoint was saved.
    Checkpoint { task_id: String, version: u64 },
    /// A test or verification was run.
    Verification {
        task_id: String,
        result: VerificationResult,
    },
    /// A repair was attempted.
    Repair { task_id: String, iteration: u32 },
    /// Execution completed successfully.
    Completed {
        task_id: String,
        outcome: ExecutionOutcome,
    },
    /// Execution failed with an error.
    Failed { task_id: String, error: String },
    /// Budget or limit was exceeded.
    BudgetExceeded { task_id: String, reason: String },
    /// Execution was cancelled.
    Cancelled { task_id: String, reason: String },
    /// Progress update.
    Progress {
        task_id: String,
        iteration: u32,
        message: String,
    },
}

/// A broadcast channel-based event sink.
#[derive(Clone)]
pub struct BroadcastEventSink {
    sender: broadcast::Sender<EngineEvent>,
}

impl BroadcastEventSink {
    pub fn new(capacity: usize) -> (Self, broadcast::Receiver<EngineEvent>) {
        let (sender, rx) = broadcast::channel(capacity);
        (Self { sender }, rx)
    }
}

#[async_trait]
impl EventSink for BroadcastEventSink {
    async fn emit(&self, event: EngineEvent) {
        let _ = self.sender.send(event);
    }
}

/// Simple event sink that logs events via tracing.
#[derive(Clone, Default)]
pub struct LoggingEventSink;

#[async_trait]
impl EventSink for LoggingEventSink {
    async fn emit(&self, event: EngineEvent) {
        match &event {
            EngineEvent::Started { task_id } => tracing::info!(task_id, "engine started"),
            EngineEvent::Completed { task_id, .. } => tracing::info!(task_id, "engine completed"),
            EngineEvent::Failed { task_id, error } => {
                tracing::error!(task_id, error, "engine failed")
            }
            EngineEvent::BudgetExceeded { task_id, reason } => {
                tracing::warn!(task_id, reason, "budget exceeded")
            }
            EngineEvent::Progress {
                task_id,
                iteration,
                message,
            } => {
                tracing::debug!(task_id, iteration, message, "progress")
            }
            EngineEvent::Checkpoint { task_id, version } => {
                tracing::debug!(task_id, version, "checkpoint saved")
            }
            EngineEvent::Verification { task_id, result } => {
                tracing::debug!(task_id, ?result, "verification")
            }
            _ => tracing::trace!(?event, "engine event"),
        }
    }
}
