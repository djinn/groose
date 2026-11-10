use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::checkpoints::{CheckpointStore, ExecutionCheckpoint};
use crate::control::{ControlReceiver, ControlSignal};
use crate::verifier::{VerificationResult, Verifier};
use crate::workspace::Workspace;
use crate::{EngineEvent, EventSink};

use groose_task::TaskSpec;

/// Execution context provided to the engine.
#[derive(Debug, Clone)]
pub struct ExecutionContext {
    pub task: TaskSpec,
    pub workspace_root: PathBuf,
    pub max_iterations: u32,
    pub max_cost_usd: f64,
    pub timeout: Duration,
    pub checkpoint_every: u32,
}

/// Outcome of an engine execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionOutcome {
    /// Completed successfully, with the model's final response.
    Completed { output: Option<String> },
    Failed { error: String },
    BudgetExceeded { reason: String },
    Cancelled { reason: String },
    PendingReview { pr_url: Option<String> },
}

/// Error type for engine operations.
#[derive(Debug, Clone)]
pub enum EngineError {
    InvalidTransition,
    WorkspaceError,
    ToolExecutionError,
    VerificationError,
    Timeout,
    Cancelled,
    InternalError { message: String },
}

/// The coding engine state that can be checkpointed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct EngineState {
    pub iteration: u32,
    pub plan: Option<String>,
    pub conversation_state: Option<serde_json::Value>,
    pub current_step: String,
    pub accumulated_cost: f64,
}

/// A simple provider interface for the engine to use.
#[async_trait]
pub trait ModelProvider: Send + Sync {
    async fn complete(&self, prompt: &str, tools: &[String]) -> Result<String>;
    fn name(&self) -> &str;
}

/// The main coding engine implementing the execution loop from SPECS.md §6.
pub struct CodingEngine {
    name: String,
}

impl CodingEngine {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Run the coding engine for the given task.
    pub async fn run(
        &self,
        task: &TaskSpec,
        context: &ExecutionContext,
        workspace: &dyn Workspace,
        control: &dyn ControlReceiver,
        events: &dyn EventSink,
        checkpoint_store: &dyn CheckpointStore,
        verifier: &dyn Verifier,
        model: &dyn ModelProvider,
    ) -> Result<ExecutionOutcome> {
        events
            .emit(EngineEvent::Started {
                task_id: task.task_id.clone(),
            })
            .await;

        // 1. Load latest checkpoint
        let latest_checkpoint = checkpoint_store.load_latest(&task.task_id).await?;
        let mut state = if let Some(cp) = latest_checkpoint {
            tracing::info!(task_id = %task.task_id, version = cp.version, "loaded checkpoint");
            serde_json::from_value::<EngineState>(cp.engine_state.clone())?
        } else {
            EngineState {
                iteration: 0,
                plan: None,
                conversation_state: None,
                current_step: "init".to_string(),
                accumulated_cost: 0.0,
            }
        };

        let start_time = std::time::Instant::now();

        // Clone repo if specified
        let has_repo = !task.repository.name.is_empty()
            && task.repository.name != "unknown"
            && task.repository.name != "djinn/groose";
        if has_repo && state.iteration == 0 {
            let clone_url = format!(
                "https://github.com/{}.git",
                task.repository.name
            );
            workspace
                .run_command("git", &["clone", &clone_url, "."])
                .await
                .ok();
        }

        // Build the conversation from state
        let mut conversation: Vec<serde_json::Value> = state
            .conversation_state
            .as_ref()
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        // System prompt for the model
        let tools_list = vec![
            "read_file".to_string(),
            "write_file".to_string(),
            "run_command".to_string(),
            "list_files".to_string(),
            "git_status".to_string(),
            "git_diff".to_string(),
            "search_code".to_string(),
        ];

        // Main execution loop
        while state.iteration < context.max_iterations {
            if control.is_cancelled().await {
                return Ok(ExecutionOutcome::Cancelled {
                    reason: "cancelled by control signal".to_string(),
                });
            }

            if std::time::Instant::now() - start_time > context.timeout {
                return Ok(ExecutionOutcome::BudgetExceeded {
                    reason: "execution timeout exceeded".to_string(),
                });
            }

            if let Some(signal) = control.try_receive().await? {
                match signal {
                    ControlSignal::Cancel { reason } => {
                        return Ok(ExecutionOutcome::Cancelled { reason });
                    }
                    ControlSignal::Pause => {
                        tracing::info!(task_id = %task.task_id, "pausing execution");
                        return Ok(ExecutionOutcome::Cancelled {
                            reason: "paused by control signal".to_string(),
                        });
                    }
                    _ => {}
                }
            }

            // 2. Form or revise a plan
            if state.plan.is_none() {
                state.plan = Some(self.form_plan(workspace, task).await?);
                events
                    .emit(EngineEvent::PlanFormed {
                        task_id: task.task_id.clone(),
                        plan: state.plan.clone().unwrap_or_default(),
                    })
                    .await;
            }

            // 3. Execute next action — ask the model what to do
            let model_prompt = if let Some(ref plan) = state.plan {
                format!(
                    "Task: {goal}\nPlan: {plan}\n\nCurrent iteration: {iter}\nStep: {step}\n\nWhat should I do next? \
                    I have access to tools like read_file, write_file, run_command, git, and search_code.\n\
                    If the task is research/documentation, provide a thorough answer directly.",
                    goal = task.instruction.goal,
                    plan = plan,
                    iter = state.iteration,
                    step = state.current_step,
                )
            } else {
                format!(
                    "Task: {goal}\n\nCurrent state: fresh start\nWhat should I do first?",
                    goal = task.instruction.goal,
                )
            };

            let model_response = crate::dsml::run_tool_loop(
                model,
                workspace,
                &model_prompt,
                &tools_list,
                10,
            )
            .await?;
            tracing::info!(iteration = state.iteration, response_len = model_response.len(), "model response received");

            events
                .emit(EngineEvent::Action {
                    task_id: task.task_id.clone(),
                    action: model_response.clone(),
                })
                .await;

            // Store response in conversation state
            let conv_entry = serde_json::json!({
                "iteration": state.iteration,
                "role": "assistant",
                "content": model_response,
            });
            conversation.push(conv_entry);
            state.conversation_state = Some(serde_json::to_value(&conversation).unwrap_or_default());

            // If the model provided a substantive response, we're done
            // (avoids overwriting the real answer with "what next?" responses)
            if model_response.len() > 100 {
                break;
            }

            // 4. Evaluate results - check if model declared completion
            state.iteration += 1;
            state.current_step = format!("iteration_{}", state.iteration);

            events
                .emit(EngineEvent::Progress {
                    task_id: task.task_id.clone(),
                    iteration: state.iteration,
                    message: format!("completed iteration {}", state.iteration),
                })
                .await;

            // 5. Checkpoint at safe boundaries
            if state.iteration % context.checkpoint_every == 0 {
                let diff = workspace.diff().await.unwrap_or_default();
                let checkpoint = ExecutionCheckpoint {
                    task_id: task.task_id.clone(),
                    version: (state.iteration / context.checkpoint_every) as u64,
                    timestamp: Utc::now(),
                    iteration: state.iteration,
                    engine_state: serde_json::to_value(&state)?,
                    workspace_diff: Some(diff),
                    pending_amendments: Vec::new(),
                };
                checkpoint_store.save(&checkpoint).await?;
                events
                    .emit(EngineEvent::Checkpoint {
                        task_id: task.task_id.clone(),
                        version: checkpoint.version,
                    })
                    .await;
            }
        }

        // 8. Verify acceptance criteria independently
        let diff = workspace.diff().await.unwrap_or_default();
        events
            .emit(EngineEvent::Progress {
                task_id: task.task_id.clone(),
                iteration: state.iteration,
                message: "verifying acceptance criteria".to_string(),
            })
            .await;

        let verification = verifier
            .verify(&diff, &task.instruction.acceptance_criteria)
            .await?;

        events
            .emit(EngineEvent::Verification {
                task_id: task.task_id.clone(),
                result: verification.clone(),
            })
            .await;

        // Extract final output from conversation state — take first substantive response
        let final_output: Option<String> = state
            .conversation_state
            .as_ref()
            .and_then(|v| serde_json::from_value::<Vec<serde_json::Value>>(v.clone()).ok())
            .and_then(|conv| {
                conv.iter()
                    .filter_map(|entry| entry.get("content"))
                    .filter_map(|c| c.as_str())
                    .find(|s| s.len() > 100)
                    .map(|s| s.to_string())
            });

        match verification {
            VerificationResult::Passed => {
                events
                    .emit(EngineEvent::Completed {
                        task_id: task.task_id.clone(),
                        outcome: ExecutionOutcome::Completed { output: final_output.clone() },
                    })
                    .await;
                Ok(ExecutionOutcome::Completed { output: final_output.clone() })
            }
            VerificationResult::Failed { errors } => {
                let error_msg = errors.join("; ");
                events
                    .emit(EngineEvent::Failed {
                        task_id: task.task_id.clone(),
                        error: error_msg.clone(),
                    })
                    .await;
                Ok(ExecutionOutcome::Failed { error: error_msg })
            }
            VerificationResult::Skipped { reason: _ } => {
                events
                    .emit(EngineEvent::Completed {
                        task_id: task.task_id.clone(),
                        outcome: ExecutionOutcome::Completed { output: final_output },
                    })
                    .await;
                Ok(ExecutionOutcome::Completed { output: None })
            }
        }
    }

    /// Form an initial plan based on the task and workspace.
    pub async fn form_plan(&self, workspace: &dyn Workspace, task: &TaskSpec) -> Result<String> {
        let files = if workspace.exists(std::path::Path::new(".")).await {
            workspace.run_command("ls", &[]).await.unwrap_or_default()
        } else {
            "(empty workspace)".to_string()
        };

        Ok(format!(
            "Task: {goal}\nRepository: {repo}\nFiles:\n{files}\nPlan:\n1. Understand the codebase\n2. Implement required changes\n3. Test and verify\n4. Create a pull request",
            goal = task.instruction.goal,
            repo = task.repository.name,
            files = files,
        ))
    }

    /// Choose the next action based on current state and task.
    pub async fn choose_next_action(&self, state: &EngineState, task: &TaskSpec) -> Result<String> {
        Ok(format!(
            "iteration {}: working on task '{}' at step '{}'",
            state.iteration, task.instruction.goal, state.current_step,
        ))
    }

    /// Save a final checkpoint when stopping.
    pub async fn save_final_checkpoint(
        &self,
        task_id: &str,
        state: &EngineState,
        workspace: &dyn Workspace,
        checkpoint_store: &dyn CheckpointStore,
    ) -> Result<()> {
        let diff = workspace.diff().await.unwrap_or_default();
        let checkpoint = ExecutionCheckpoint {
            task_id: task_id.to_string(),
            version: u64::MAX,
            timestamp: Utc::now(),
            iteration: state.iteration,
            engine_state: serde_json::to_value(state)?,
            workspace_diff: Some(diff),
            pending_amendments: Vec::new(),
        };
        checkpoint_store.save(&checkpoint).await
    }
}

/// A simple ModelProvider that shells out for Phase 1.
pub struct ShellModelProvider {
    name: String,
    #[allow(dead_code)]
    model_command: String,
}

impl ShellModelProvider {
    pub fn new(name: &str, model_command: &str) -> Self {
        Self {
            name: name.to_string(),
            model_command: model_command.to_string(),
        }
    }

    /// Create a simple mock provider for testing.
    pub fn mock(name: &str) -> Self {
        Self {
            name: name.to_string(),
            model_command: "echo".to_string(),
        }
    }
}

#[async_trait]
impl ModelProvider for ShellModelProvider {
    async fn complete(&self, prompt: &str, _tools: &[String]) -> Result<String> {
        let result = tokio::process::Command::new("sh")
            .args(&["-c", &format!("echo '{}'", prompt)])
            .output()
            .await?;
        Ok(String::from_utf8_lossy(&result.stdout).to_string())
    }

    fn name(&self) -> &str {
        &self.name
    }
}
