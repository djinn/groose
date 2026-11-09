use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── Canonical Task Specification (SPECS.md §5) ──────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct RepositorySpec {
    pub provider: String,
    pub name: String,
    pub base_ref: String,
    /// Resolved and pinned at admission time.
    pub base_sha: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct InstructionSpec {
    pub goal: String,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct AgentConfig {
    pub profile: String,
    #[serde(default)]
    pub skills: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct ExecutionConfig {
    #[serde(default = "autonomous_default")]
    pub mode: String,
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    #[serde(default = "default_max_cost")]
    pub max_cost_usd: f64,
    #[serde(default = "default_timeout")]
    pub timeout_minutes: u32,
}

fn autonomous_default() -> String {
    "autonomous".to_string()
}

fn default_max_iterations() -> u32 {
    40
}

fn default_max_cost() -> f64 {
    12.0
}

fn default_timeout() -> u32 {
    90
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct DeliveryConfig {
    #[serde(default)]
    pub strategy: String,
    #[serde(default)]
    pub merge: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct OriginInfo {
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub event_id: String,
    #[serde(default)]
    pub conversation_id: String,
}

/// The canonical task specification — the universal input to the coding engine.
/// Once admitted, this is immutable; changes arrive as versioned amendments.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct TaskSpec {
    pub schema_version: String,
    pub task_id: String,
    #[serde(default = "implement_type_default")]
    pub r#type: String,
    pub repository: RepositorySpec,
    pub instruction: InstructionSpec,
    pub agent: AgentConfig,
    pub execution: ExecutionConfig,
    pub delivery: DeliveryConfig,
    pub origin: OriginInfo,
    /// Resolved and pinned at admission.
    #[serde(default)]
    pub admitted_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub requester_identity: Option<String>,
    #[serde(default)]
    pub installation_identity: Option<String>,
    #[serde(default = "default_version")]
    pub version: u64,
}

fn implement_type_default() -> String {
    "implement".to_string()
}

fn default_version() -> u64 {
    0
}

impl TaskSpec {
    /// Load from a JSON file.
    pub fn from_json_file(path: &std::path::Path) -> Result<Self> {
        let bytes = std::fs::read(path)?;
        Ok(serde_json::from_slice::<Self>(&bytes)?)
    }

    /// Load from a JSON string.
    pub fn from_json(json: &str) -> Result<Self> {
        Ok(serde_json::from_str::<Self>(json)?)
    }

    /// Load from a YAML file.
    pub fn from_yaml_file(path: &std::path::Path) -> Result<Self> {
        let bytes = std::fs::read(path)?;
        Ok(serde_yaml::from_slice::<Self>(&bytes)?)
    }

    /// Serialize to pretty-printed JSON.
    pub fn to_json_pretty(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Versioned amendment: return a new spec with incremented version and changed fields.
    pub fn apply_amendment(&self, amendment: TaskAmendment) -> Result<Self> {
        let mut updated = self.clone();
        updated.version = self.version + 1;

        if let Some(goal) = amendment.instruction_goal {
            updated.instruction.goal = goal;
        }
        if let Some(criteria) = amendment.acceptance_criteria {
            updated.instruction.acceptance_criteria = criteria;
        }
        if let Some(skills) = amendment.skills {
            updated.agent.skills = skills;
        }
        if let Some(max_iterations) = amendment.max_iterations {
            updated.execution.max_iterations = max_iterations;
        }
        if let Some(max_cost_usd) = amendment.max_cost_usd {
            updated.execution.max_cost_usd = max_cost_usd;
        }
        if let Some(timeout_minutes) = amendment.timeout_minutes {
            updated.execution.timeout_minutes = timeout_minutes;
        }

        Ok(updated)
    }
}

// ── Amendments (SPECS.md §5) ────────────────────────────────────────────

/// An authorized, versioned amendment to a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TaskAmendment {
    pub task_id: String,
    pub expected_version: u64,
    pub timestamp: DateTime<Utc>,
    pub authorized_by: String,
    pub instruction_goal: Option<String>,
    pub acceptance_criteria: Option<Vec<String>>,
    pub skills: Option<Vec<String>>,
    pub max_iterations: Option<u32>,
    pub max_cost_usd: Option<f64>,
    pub timeout_minutes: Option<u32>,
}

// ── Task State Machine (SPECS.md §10) ───────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum TaskState {
    Created,
    Validating,
    Queued,
    PreparingWorkspace,
    Running,
    Verifying,
    Delivering,
    AwaitingReview,
    Resuming,
    Paused,
    WaitingForInput,
    Completed,
    Failed,
    Cancelled,
    BudgetExceeded,
}

/// Valid transitions per SPECS.md §10.
impl TaskState {
    pub fn can_transition_to(&self, target: TaskState) -> bool {
        match (*self, target) {
            // Forward flow
            (TaskState::Created, TaskState::Validating)
            | (TaskState::Validating, TaskState::Queued)
            | (TaskState::Queued, TaskState::PreparingWorkspace)
            | (TaskState::PreparingWorkspace, TaskState::Running)
            | (TaskState::Running, TaskState::Verifying)
            | (TaskState::Verifying, TaskState::Delivering)
            | (TaskState::Delivering, TaskState::AwaitingReview)
            | (TaskState::AwaitingReview, TaskState::Completed)
            // Repair loop
            | (TaskState::Verifying, TaskState::Running)
            // Review -> resume
            | (TaskState::AwaitingReview, TaskState::Resuming)
            | (TaskState::Paused, TaskState::Resuming)
            // Resume -> Running
            | (TaskState::Resuming, TaskState::Running)
            // Pause
            | (TaskState::Running, TaskState::Paused)
            // Waiting for input
            | (TaskState::Running, TaskState::WaitingForInput)
            | (TaskState::WaitingForInput, TaskState::Resuming)
            // Terminal states from Running
            | (TaskState::Running, TaskState::Failed)
            | (TaskState::Running, TaskState::Cancelled)
            | (TaskState::Running, TaskState::BudgetExceeded)
            // From any active state to terminal
            // (SPECS: "ANY ACTIVE -> FAILED | CANCELLED | BUDGET_EXCEEDED")
            | (TaskState::Created, TaskState::Cancelled)
            | (TaskState::Validating, TaskState::Failed)
            | (TaskState::Validating, TaskState::Cancelled)
            | (TaskState::Queued, TaskState::Cancelled)
            | (TaskState::PreparingWorkspace, TaskState::Failed)
            | (TaskState::PreparingWorkspace, TaskState::Cancelled)
            | (TaskState::Verifying, TaskState::Failed)
            | (TaskState::Verifying, TaskState::BudgetExceeded)
            | (TaskState::Delivering, TaskState::Failed)
            | (TaskState::Delivering, TaskState::Cancelled)
            | (TaskState::Paused, TaskState::Cancelled)
            | (TaskState::WaitingForInput, TaskState::Cancelled)
            | (TaskState::WaitingForInput, TaskState::Failed)
            | (TaskState::Resuming, TaskState::Failed)
            | (TaskState::Resuming, TaskState::Cancelled)
            | (TaskState::AwaitingReview, TaskState::Cancelled) => true,
            _ => false,
        }
    }

    /// Whether this state is a terminal (final) state.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskState::Completed
                | TaskState::Failed
                | TaskState::Cancelled
                | TaskState::BudgetExceeded
        )
    }

    /// Whether this state is active (execution is live).
    pub fn is_active(&self) -> bool {
        !self.is_terminal()
            && !matches!(
                self,
                TaskState::Created
                    | TaskState::Validating
                    | TaskState::Queued
                    | TaskState::Paused
                    | TaskState::WaitingForInput
                    | TaskState::AwaitingReview
                    | TaskState::Completed
            )
    }
}

// ── Events (SPECS.md §10: "Events record facts; commands request state changes") ──

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TaskEvent {
    pub event_id: String,
    pub task_id: String,
    pub timestamp: DateTime<Utc>,
    pub event_type: String,
    pub payload: Value,
    pub expected_version: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TaskCommand {
    pub command_id: String,
    pub task_id: String,
    pub command_type: String,
    pub expected_version: u64,
    pub payload: Value,
    pub authorized_by: Option<String>,
}

// ── Task Record (what gets persisted) ────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TaskRecord {
    pub task_id: String,
    pub spec: TaskSpec,
    pub state: TaskState,
    pub version: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub error_message: Option<String>,
}

impl TaskRecord {
    pub fn new(spec: TaskSpec) -> Self {
        let now = chrono::Utc::now();
        Self {
            task_id: spec.task_id.clone(),
            spec,
            state: TaskState::Created,
            version: 1,
            created_at: now,
            updated_at: now,
            error_message: None,
        }
    }

    /// Attempt a state transition. Returns an error if the transition is invalid.
    pub fn transition_to(&mut self, target: TaskState) -> Result<()> {
        if !self.state.can_transition_to(target) {
            return Err(anyhow!("invalid transition: {} -> {}", self.state, target,));
        }
        self.state = target;
        self.updated_at = chrono::Utc::now();
        self.version += 1;
        Ok(())
    }

    pub fn set_error(&mut self, message: &str) {
        self.error_message = Some(message.to_string());
        self.updated_at = chrono::Utc::now();
    }
}

// ── Policy (SPECS.md §6: PolicyEvaluator) ───────────────────────────────

/// A policy rule allowing or denying a specific operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyRule {
    Allow { resource: String, operation: String },
    Deny { resource: String, operation: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Policy {
    pub rules: Vec<PolicyRule>,
    pub default_deny: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDecision {
    Allowed,
    Denied { reason: String },
}

impl Policy {
    pub fn evaluate(&self, resource: &str, operation: &str) -> PolicyDecision {
        // Explicit rules checked in order (last match wins)
        let mut decision: PolicyDecision = if self.default_deny {
            PolicyDecision::Denied {
                reason: "default deny".to_string(),
            }
        } else {
            PolicyDecision::Allowed
        };

        for rule in &self.rules {
            match rule {
                PolicyRule::Allow {
                    resource: r,
                    operation: o,
                } if matches_pattern(r, resource) && matches_pattern(o, operation) => {
                    decision = PolicyDecision::Allowed;
                }
                PolicyRule::Deny {
                    resource: r,
                    operation: o,
                } if matches_pattern(r, resource) && matches_pattern(o, operation) => {
                    decision = PolicyDecision::Denied {
                        reason: format!("policy rule denies {}/{}", resource, operation),
                    };
                }
                _ => {}
            }
        }

        decision
    }
}

/// Simple glob-style pattern matching (* matches anything).
fn matches_pattern(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if pattern.ends_with("*") {
        let prefix = &pattern[..pattern.len() - 1];
        return value.starts_with(prefix);
    }
    pattern == value
}

/// Load policy from YAML.
pub fn load_policy(path: &std::path::Path) -> Result<Policy> {
    let bytes = std::fs::read(path)?;
    Ok(serde_yaml::from_slice::<Policy>(&bytes)?)
}

// ── Agent Profile ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AgentProfile {
    pub version: u32,
    pub name: String,
    pub model: AgentModelConfig,
    pub skills: Vec<String>,
    pub tools: ToolAllowConfig,
    pub execution: AgentExecutionConfig,
    pub delivery: AgentDeliveryConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AgentModelConfig {
    pub provider: String,
    pub primary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ToolAllowConfig {
    pub allow: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AgentExecutionConfig {
    pub autonomous: bool,
    pub checkpoint_every_iterations: u32,
    pub max_iterations: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AgentDeliveryConfig {
    pub create_pr: bool,
    pub auto_merge: bool,
}

pub fn load_agent_profile(path: &std::path::Path) -> Result<AgentProfile> {
    let bytes = std::fs::read(path)?;
    Ok(serde_yaml::from_slice::<AgentProfile>(&bytes)?)
}
