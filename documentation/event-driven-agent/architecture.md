# Groose Architecture

## Overview

Groose is an event-driven autonomous software engineering agent. It transforms
Goose's interactive agent into a durable, task-oriented coding engine that
operates independently of its instruction transports (CLI, Slack, GitHub).

## Core Principle

> Instruction events initiate or amend durable tasks. The coding loop operates
> independently of Slack, GitHub, and other instruction transports.

## System Architecture (SPECS.md §3)

```
Slack Events ──┐
GitHub Events ─┼──> Ingestion / Authentication / Deduplication
HTTP / CLI ────┘    │
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
```

Three planes:

1. **Instruction Plane** — webhook handlers, CLI, API adapters. Validates,
   authenticates, deduplicates, and submits canonical tasks.
2. **Task Plane** — the durable orchestrator that owns task identity, lifecycle,
   state machine, scheduling, and amendments.
3. **Execution Plane** — the Goose-derived coding runtime. Runs the `Observe ->
   Plan -> Edit -> Test -> Evaluate -> Repair` loop independently.

## Crate Architecture

### `groose-task` — Canonical task spec, state machine, policy

- `TaskSpec` — universal input to the coding engine (SPECS.md §5)
- `TaskState` — state machine with validated transitions (SPECS.md §10)
- `TaskRecord` — persisted task with versioned state transitions
- `TaskAmendment` — authorized, versioned instruction revisions
- `TaskEvent` / `TaskCommand` — event-sourced command model
- `Policy` / `AgentProfile` — authorization and profile types

### `groose-execution` — Coding engine, workspace, checkpoints

- `CodingEngine` — independent execution loop (SPECS.md §6)
- `Workspace` — trait for Git worktrees, local dirs, temp dirs
- `CheckpointStore` — durable execution snapshots
- `Verifier` — tests, linting, acceptance criteria, diff checks
- `ControlReceiver` — cancellation, pause, resume, amendments
- `EventSink` / `EngineEvent` — progress and artifact events

### `groose-cli` — Standalone binary for Phase 1

- `groose run --spec ./task.json --workspace ./repo` runs a task offline
- No Slack or GitHub dependencies in the execution path

## State Machine (SPECS.md §10)

```
CREATED -> VALIDATING -> QUEUED -> PREPARING_WORKSPACE -> RUNNING
RUNNING -> VERIFYING -> DELIVERING -> AWAITING_REVIEW -> COMPLETED
VERIFYING -> RUNNING (repair)
AWAITING_REVIEW -> RESUMING -> RUNNING
RUNNING -> PAUSED -> RESUMING -> RUNNING
RUNNING -> WAITING_FOR_INPUT -> RESUMING -> RUNNING
ANY ACTIVE -> FAILED | CANCELLED | BUDGET_EXCEEDED
```

## Coding Engine Loop (SPECS.md §6.1)

1. **Load** — task, pinned skills, repository context, latest checkpoint
2. **Plan** — inspect code, form or revise a plan
3. **Act** — choose next action, execute through authorized tools
4. **Evaluate** — observations, errors, tests, diff
5. **Repair** — fix failures or continue
6. **Checkpoint** — persist progress at safe boundaries
7. **Amend** — apply authorized pending amendments when safe
8. **Verify** — independently verify acceptance criteria
9. **Deliver** — artifacts or terminal state

## Implementation Phases

| Phase | Description | Exit Criteria |
|-------|-------------|---------------|
| 1 | Standalone engine | Task completes via CLI without integrations |
| 2 | Durable orchestration | PostgreSQL, worker leases, pause/resume |
| 3 | GitHub App | Issue triggers, PR creation, review feedback |
| 4 | Slack App | Mentions, thread correlation, progress |
| 5 | Production hardening | Skills, budgets, telemetry, concurrency |

## Key Design Rules

1. Slack/GitHub are interchangeable input adapters.
2. The coding engine accepts `TaskSpec`, not webhook payloads.
3. Task state survives worker restarts.
4. Skills guide behavior but don't grant permissions.
5. External side effects are idempotent.
6. Completion is verified by tests and acceptance criteria,
   not the model declaring success.
7. Preserve existing Goose CLI and desktop behavior.
8. Keep Slack/GitHub dependencies out of the coding engine.