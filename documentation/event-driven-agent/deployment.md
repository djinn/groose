# Groose Deployment Guide

## Local (Phase 1)

### Prerequisites

- Rust 1.94+ with cargo
- Git (for workspace operations)
- A repository to work on

### Building

```bash
source bin/activate-hermit
cargo build -p groose-cli --release
```

### Running

```bash
# Run a task against a local repository
cargo run --release -p groose-cli -- run \
  --spec examples/event-driven/task.json \
  --workspace /path/to/repo

# With test and lint commands
cargo run --release -p groose-cli -- run \
  --spec examples/event-driven/task.json \
  --workspace /path/to/repo \
  --test-command "cargo test" \
  --lint-command "cargo fmt --check"
```

## Phase 2: Durable Orchestration

Requires PostgreSQL and additional dependencies:

```bash
# Database setup
cargo install sqlx-cli
sqlx migrate run --database-url postgres://localhost/groose

# Run worker
groose-worker --database-url postgres://localhost/groose

# Run webhook receiver
groose-webhooks --database-url postgres://localhost/groose
```

## Phase 3-5: Production

See `deployment.md` for Kubernetes manifests, monitoring setup,
and operational runbooks.

## Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `GROOSE_CHECKPOINT_DIR` | Checkpoint storage path | `.groose/checkpoints` |
| `GROOSE_MAX_ITERATIONS` | Max coding loop iterations | 40 |
| `GROOSE_TIMEOUT_MINUTES` | Task timeout | 90 |
| `GROOSE_MAX_COST_USD` | Max cost per task | 12.0 |