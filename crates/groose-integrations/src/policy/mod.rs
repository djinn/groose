//! Policy enforcement bridge between trust decisions and task creation.
//!
//! This module provides the `PolicyEvaluator` that SPECS.md §6 references,
//! combining the authorization layer with Groose's own policy engine.

use crate::trust::{AuthDecision, Authorizer, Requester, Target};

/// Evaluates whether an action is permitted based on authorization,
/// resource constraints, and operational policies.
pub struct PolicyEvaluator {
    authorizer: Box<dyn Authorizer>,
    task_budget_default: f64,
    max_concurrent_tasks: usize,
}

impl PolicyEvaluator {
    pub fn new(
        authorizer: Box<dyn Authorizer>,
        task_budget_default: f64,
        max_concurrent_tasks: usize,
    ) -> Self {
        Self {
            authorizer,
            task_budget_default,
            max_concurrent_tasks,
        }
    }

    /// Check whether a requester is authorized to perform an action on a target.
    pub fn authorize(&self, requester: &Requester, target: &Target) -> AuthDecision {
        self.authorizer.authorize(requester, target)
    }

    /// The default budget for a new task from this integration.
    pub fn default_task_budget(&self) -> f64 {
        self.task_budget_default
    }

    /// The maximum number of concurrent tasks allowed.
    pub fn max_concurrent_tasks(&self) -> usize {
        self.max_concurrent_tasks
    }

    /// Check whether creating a new task would exceed operational limits.
    pub fn check_operational_limits(&self, active_tasks: usize) -> Result<(), String> {
        if active_tasks >= self.max_concurrent_tasks {
            return Err(format!(
                "max concurrent tasks ({}) reached",
                self.max_concurrent_tasks,
            ));
        }
        Ok(())
    }
}

/// Map an integration event to a task specification.
///
/// This is the boundary between the trust/integration layer and
/// the task/execution layer.  The coding engine never sees raw
/// webhook payloads — only validated, canonical TaskSpecs.
pub fn event_to_task_spec(
    requester: &Requester,
    target: &Target,
    goal: &str,
    acceptance_criteria: Vec<String>,
) -> groose_task::TaskSpec {
    groose_task::TaskSpec {
        schema_version: "1.0".to_string(),
        task_id: format!("task_{}", uuid::Uuid::new_v4().simple()),
        r#type: "implement".to_string(),
        repository: groose_task::RepositorySpec {
            provider: requester.provider.clone(),
            name: target.repository.clone(),
            base_ref: "main".to_string(),
            base_sha: None,
        },
        instruction: groose_task::InstructionSpec {
            goal: goal.to_string(),
            acceptance_criteria,
        },
        agent: groose_task::AgentConfig {
            profile: "default-engineer".to_string(),
            skills: vec!["developer".to_string()],
        },
        execution: groose_task::ExecutionConfig {
            mode: "autonomous".to_string(),
            max_iterations: 40,
            max_cost_usd: 12.0,
            timeout_minutes: 90,
        },
        delivery: groose_task::DeliveryConfig {
            strategy: "pull_request".to_string(),
            merge: "human_approval".to_string(),
        },
        origin: groose_task::OriginInfo {
            source: requester.provider.clone(),
            event_id: String::new(),
            conversation_id: String::new(),
        },
        admitted_at: None,
        requester_identity: Some(requester.user_id.clone()),
        installation_identity: requester.installation_id.clone(),
        version: 0,
    }
}