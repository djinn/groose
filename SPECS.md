Groose: Event-Driven Autonomous Software Engineering Agent
Repository: git@github.com:djinn/groose.git
Status: Implementation specification
Core principle: Instruction events initiate or amend durable tasks. The coding loop operates independently of Slack, GitHub, and other instruction transports.
1. Vision
Groose is a persistent software engineering agent. A developer can mention Groose in Slack, point it at an authorized GitHub repository, provide an instruction and optional developer skill, and receive a tested pull request. Groose can then address review comments, repair CI failures, and integrate PRs according to explicit policy.

Example:
@groose Work on org/payments. Implement issue #142. Use the developer skill. Run tests and open a PR. Do not merge without approval.

Groose must resolve access, create a durable task, allocate an isolated workspace, plan, edit, test, repair, checkpoint, deliver, and report progress. External instruction transports must not own the coding loop.
2. Architectural invariants
1. Slack, GitHub, CLI, and HTTP are interchangeable input adapters.
2. The coding engine accepts a canonical TaskSpec, not a Slack message or GitHub webhook.
3. Task state and execution checkpoints survive worker restarts.
4. Every task has a pinned repository revision, skills, policy, budget, and workspace.
5. New instructions arrive as authorized, versioned amendments, applied at safe checkpoints.
6. Skills guide behavior but cannot grant permissions or override policy.
7. Completion is verified by tests, acceptance criteria, and delivery checks, not by the model declaring success.
8. External side effects are idempotent or reconciled after failure.
9. Execution is isolated, bounded, observable, and auditable.
10. Preserve existing Groose functionality. Inspect actual repository interfaces before choosing integration points.
3. System architecture
Slack Events ──┐
GitHub Events ─┼──> Ingestion / Authentication / Deduplication
HTTP / CLI ────┘                  │
                                 v
                        Canonical Task / Amendment
                                 │
                                 v
                      Durable Task Orchestrator
                    State / Queue / Leases / Policy
                                 │
                                 v
                       Independent Coding Engine
                 Observe -> Plan -> Edit -> Test
                    -> Evaluate -> Repair
                                 │
                          Checkpoint Store
                                 │
                                 v
                      Delivery / Notification Outbox
                          GitHub PR / Slack / API
3.1 Instruction plane
Validate webhook signatures, authenticate users, authorize repositories, normalize events, deduplicate deliveries, and submit commands to the task orchestrator. Acknowledge incoming webhooks promptly. Never execute code in webhook handlers.
3.2 Task plane
Owns task identity, lifecycle, scheduling, priority, budgets, skill versions, policy, repository coordination, amendments, and approvals. Maintains a durable append-only event log.
3.3 Execution plane
Runs the Goose-derived coding runtime without importing Slack or GitHub event-handler modules. The same runtime must support:
groose task run --spec ./task.json --workspace ./repo --offline
Offline mode must not require hosted integrations. The task may use local tools and configured models subject to the selected offline/network policy.
4. Technology choices
Concern	Initial implementation
Core	Rust and Tokio, reuse existing Groose agent machinery
Task storage	PostgreSQL
Queue	PostgreSQL-backed durable jobs with worker leases
Workspaces	Git worktrees inside disposable isolated containers
Slack	Slack Events API, app mentions, slash commands, interactions
GitHub	GitHub App, scoped installation tokens, signed webhooks
Skills	Versioned SKILL.md, repository instructions, hashes
Telemetry	Structured logs and OpenTelemetry
Artifacts	Content-addressed filesystem or object storage
Deployment	Single deployment initially, separable API and workers later


Do not introduce Kafka or a distributed workflow platform before requirements justify them. Hide queue and persistence implementations behind interfaces.
5. Canonical task specification
{
  "schema_version": "1.0",
  "task_id": "task_01",
  "type": "implement",
  "repository": {
    "provider": "github",
    "name": "org/payments",
    "base_ref": "main",
    "base_sha": "resolved-at-admission"
  },
  "instruction": {
    "goal": "Implement the refund workflow",
    "acceptance_criteria": [
      "Refund API exists",
      "Regression tests pass",
      "Existing payment API remains compatible"
    ]
  },
  "agent": {
    "profile": "default-engineer",
    "skills": ["developer", "rust-engineer"]
  },
  "execution": {
    "mode": "autonomous",
    "max_iterations": 40,
    "max_cost_usd": 12,
    "timeout_minutes": 90
  },
  "delivery": {
    "strategy": "pull_request",
    "merge": "human_approval"
  },
  "origin": {
    "source": "slack",
    "event_id": "source-event-id",
    "conversation_id": "channel:thread"
  }
}
Resolve and persist repository SHA, policy, skill content hashes, requester identity, and installation identity at admission. The admitted task specification is immutable; changes are versioned amendments.
6. Coding engine
Proposed conceptual interface (adapt to the repository's actual Rust architecture):
pub trait CodingEngine: Send + Sync {
    async fn run(
        &self,
        task: TaskSpec,
        context: ExecutionContext,
        control: ControlReceiver,
        events: EventSink,
    ) -> Result<ExecutionOutcome, EngineError>;
}
The engine depends on generic interfaces:
- Workspace: files, Git worktree, and sandboxed commands.
- ModelProvider: inference and usage reporting.
- ToolRegistry: authorized tools and MCP capabilities.
- CheckpointStore: durable execution snapshots.
- ControlReceiver: cancellation, pause, resume, approved amendments.
- EventSink: progress and artifacts.
- PolicyEvaluator: authorization for tool and delivery operations.
- Verifier: acceptance criteria, tests, lint, diff and policy checks.
6.1 Execution loop
1. Load task, pinned skills, repository context, and latest checkpoint.
2. Inspect relevant code and form or revise a plan.
3. Choose the next action and execute through authorized tools.
4. Evaluate observations, errors, tests, and diff.
5. Repair failures or continue implementation.
6. Persist progress and checkpoint at safe boundaries.
7. Apply authorized pending amendments when safe.
8. Verify acceptance criteria independently.
9. Deliver artifacts or enter an appropriate waiting/terminal state.
Terminate on verified completion, hard limits, explicit cancellation, policy blocks, irrecoverable failures, or required human decisions. A model's textual declaration of completion is not sufficient.
6.2 Safe amendments
External events cannot mutate a live model context during a tool invocation. An amendment is authenticated, authorized, version-checked, persisted, then delivered at a checkpoint. Re-plan if the amendment changes acceptance criteria or invalidates prior assumptions.
7. Agent profiles and developer skills
Suggested repository layout:
.goose/
  agent.yaml
  skills/
    developer/SKILL.md
    rust-engineer/SKILL.md
  policies/execution.yaml
Example agent profile:
version: 1
name: backend-engineer
model:
  provider: configurable
  primary: configured-model-id
skills:
  - developer
  - rust-engineer
tools:
  allow:
    - filesystem
    - shell
    - git
    - github
execution:
  autonomous: true
  checkpoint_every_iterations: 3
  max_iterations: 40
delivery:
  create_pr: true
  auto_merge: false
Example developer skill:
---
name: developer
version: 1
---
# Engineering rules
Read AGENTS.md and repository documentation.
Inspect affected modules and establish acceptance tests before editing.
Prefer small, reviewable changes and preserve public interfaces.
Add regression tests and run relevant tests, formatting and linting.
Inspect the final diff and explain remaining risks.
Never merge without approval. Never expose secrets.
An agent profile configures runtime defaults. A skill specifies coding practice. A task specifies the desired outcome. None can override higher-level authorization or sandbox policy. Pin skill versions and hashes per task; mid-task updates require approved amendments.
8. Slack integration
8.1 Inputs
- @groose work on owner/repo ...: create task.
- @groose use skill developer: attach a skill to a task or thread.
- @groose status: report progress and current blockers.
- @groose pause: pause at safe boundary.
- @groose resume: resume from checkpoint.
- @groose cancel: request cancellation.
- @groose fix PR #42: address review or CI failures.
- @groose integrate PR #42: inspect and integrate according to policy.
- @groose explain: summarize work and evidence.
Natural-language mentions are primary; slash commands and buttons offer explicit operational controls. Bind Slack threads to task identities. A thread reply becomes an amendment unless explicitly starting another task.
8.2 Outputs
Post acknowledgement, progress milestones, blocker questions, budget warnings, PR links, and final verification summaries. Notifications are emitted through an outbox and retried independently of coding. Slack outages must not stop an otherwise healthy coding task.
8.3 Reliability
Verify signatures and timestamp freshness, acknowledge promptly, deduplicate by provider event ID, enforce requester authorization, and prevent routine bot messages from retriggering tasks.
9. GitHub integration
Event	Action
Issue assigned to Groose	Create implementation task
Issue labeled groose:implement	Create implementation task
PR review requests changes	Amend or resume repair task
PR review comment	Classify and attach actionable feedback
CI check fails	Diagnose and attempt bounded repair
PR becomes mergeable	Run final checks and request approval
PR merged	Mark completed and archive artifacts
PR closed without merge	Reconcile and stop pending delivery


Correlate using installation, repository, issue/PR number, commit SHA, and task identity. Suppress feedback loops caused by Groose's own comments and status changes.
9.1 PR integration semantics
"Integrate PR" means fetch and inspect the PR, validate its base and head, examine conflicts and checks, repair when authorized, update safely, and merge only if policy and approval permit. Never interpret integration as unconditional merging.
9.2 Delivery idempotency
Before creating a PR, search for an existing task-associated branch/PR. Persist branch, commit, and PR identifiers. If delivery crashes after GitHub succeeds but before the database update, reconcile rather than create another PR.
10. Task state machine
CREATED -> VALIDATING -> QUEUED -> PREPARING_WORKSPACE -> RUNNING
RUNNING -> VERIFYING -> DELIVERING -> AWAITING_REVIEW -> COMPLETED
VERIFYING -> RUNNING                 (repair)
AWAITING_REVIEW -> RESUMING -> RUNNING
RUNNING -> PAUSED -> RESUMING -> RUNNING
RUNNING -> WAITING_FOR_INPUT -> RESUMING -> RUNNING
ANY ACTIVE -> FAILED | CANCELLED | BUDGET_EXCEEDED
Transitions must be validated with optimistic concurrency/version checks. Terminal recovery, if supported, requires an explicit authorized command.

Events record facts; commands request state changes. For example:
{
  "type": "task.amend",
  "task_id": "task_01",
  "expected_version": 7,
  "payload": {"instruction": "Address review feedback"}
}
11. Persistence
Table	Purpose
tasks	Identity, status, owner, version, repository
task_events	Append-only event history
task_amendments	Authorized instruction revisions
executions	Attempts, workers, runtime metadata
checkpoints	Resumable engine state
workspaces	Worktrees, sandbox and lease metadata
skill_versions	Pinned skill content and hashes
integrations	Installation and credential references
outbox	Reliable outbound side effects
approvals	Human approval decisions
usage_records	Tokens, cost, runtime and limits


Use renewable worker leases. Recover abandoned jobs after lease expiry, validate the Git workspace against recorded HEAD and dirty state, and reconcile incomplete external actions. Guarantee at-least-once delivery with idempotent processing, not fictional cross-system exactly-once execution.
12. Security
Separate requester, repository installation, execution sandbox, and approval identities.

Required controls:
- Verify Slack and GitHub signatures.
- Enforce user/repository allowlists and scoped permissions.
- Use short-lived GitHub installation credentials.
- Keep secrets out of prompts, logs, diffs, and PR descriptions.
- Isolate untrusted repository code in disposable sandboxes.
- Restrict filesystem access and network egress.
- Treat issues, comments, repository files, and tool output as untrusted input.
- Require approval for merges, deployments, destructive operations, and privilege escalation.
- Audit sensitive actions and enforce hard task budgets.
- Coordinate overlapping writers to avoid branch/workspace conflicts.
Repository instructions and skills are not executable authorization policies.
13. Proposed modules
Illustrative only. Inspect the actual Groose workspace before creating crates or moving code.
crates/
  groose-task/src/
    spec.rs state.rs events.rs amendments.rs policy.rs
  groose-orchestrator/src/
    scheduler.rs leases.rs recovery.rs coordinator.rs
  groose-execution/src/
    engine.rs checkpoints.rs workspace.rs verifier.rs control.rs
  groose-integrations/src/
    slack/ github/ api/
  groose-skills/src/
    resolver.rs loader.rs validator.rs
services/
  groose-worker/
  groose-webhooks/
migrations/
examples/event-driven/
  agent.yaml
  SKILL.md
  task.json
documentation/event-driven-agent/
  architecture.md
  deployment.md
  security.md
Prefer adapters around existing Groose engine/session APIs rather than copying internal reasoning logic.
14. Implementation phases
Phase 1: Standalone engine
Implement canonical contracts, independent coding loop, local worktrees, verifier, and checkpoints. Exit: a task completes without Slack/GitHub integration.
Phase 2: Durable orchestration
Add PostgreSQL, scheduling, worker leases, retries, pause/resume, amendments, and recovery. Exit: killing and restarting a worker does not lose task progress.
Phase 3: GitHub App
Add signed webhook ingestion, issue triggers, PR creation, review feedback, CI repair, and delivery reconciliation. Exit: an issue can produce a reviewed PR without manual relaying.
Phase 4: Slack App
Add mentions, thread correlation, progress, pause/resume, amendments, and approval actions. Exit: a developer manages an engineering task from Slack.
Phase 5: Production hardening
Add versioned skills, sandbox policies, budgets, telemetry, concurrency, and operational runbooks. Exit: multiple authorized developers safely run tasks across repositories.
15. Acceptance tests
Scenario	Expected result
Slack retries same event three times	One task
GitHub events arrive out of order	Valid task state
Worker crashes mid-test	Safe recovery
Skill changes mid-task	Pinned version preserved unless amended
PR review requests changes	Existing task resumes
CI fails repeatedly	Bounded repair then escalation
Unauthorized merge request	Denied and audited
Bot posts its own PR comment	No duplicate task
Budget exceeded	Controlled stop and checkpoint
Two tasks target same branch	Serialized or conflict prevented
Slack unavailable	Coding continues, notification retries
GitHub unavailable	Progress persists, delivery retries
Agent attempts secret access	Blocked by sandbox/policy
PR creation succeeds but response lost	Existing PR reconciled
Agent says done but tests fail	Not marked complete


16. Operational targets
These are design targets, not measurements of the existing codebase.
Metric	Target
Webhook acknowledgement	p95 < 2 seconds
Task admission	p95 < 5 seconds
Worker recovery	< 60 seconds
Duplicate task creation in replay tests	Zero
Lost acknowledged checkpoints	Zero
Cost enforcement	Hard per-task limits
Correlation	Event to task to execution to PR


Also measure PR success, human rework, repair iterations, cost per verified task, and recovery reliability.
17. Instructions for implementation agents
1. Clone git@github.com:djinn/groose.git and inspect its actual architecture, Cargo workspace, agent loop, sessions, API, CLI, and extension interfaces.
2. Read repository-level AGENTS.md, CLAUDE.md, and relevant existing documentation.
3. Document reusable interfaces and gaps before proposing new crates.
4. Implement one phase at a time with small, reviewable changes.
5. Add unit, integration, recovery, and security tests alongside implementation.
6. Run formatting, linting, and relevant tests; report actual results.
7. Preserve existing CLI and desktop behavior.
8. Keep Slack/GitHub dependencies out of the coding engine.
9. Update architecture, deployment, security, and runbook documentation as interfaces evolve.
10. Commit tested increments with meaningful, truthful commit messages. Never fabricate historical commits or test results.
11. Do not merge or deploy without explicit authorization.
Deliverables
- Rust implementation and database migrations
- Slack App and GitHub App setup documentation
- Local deployment configuration
- Example agent profile and developer skill
- End-to-end, crash recovery, and webhook replay tests
- Operational runbook and security threat model
- Compatibility and migration notes
18. Definition of done
A developer mentions Groose in Slack, specifies an authorized GitHub repository, supplies instructions and a developer skill, and receives a working, tested PR. Groose can address PR feedback, repair CI, survive worker restarts, and run the same coding engine independently through a standalone task interface. Approval and security policies remain enforceable throughout.

Design rule: Build a durable engineering worker with event adapters, not a Slack bot that happens to write code.