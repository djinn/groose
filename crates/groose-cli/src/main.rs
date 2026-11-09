use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};

use groose_execution::engine::{CodingEngine, ShellModelProvider};
use groose_execution::workspace::LocalWorkspace;
use groose_execution::verifier::BasicVerifier;
use groose_execution::checkpoints::FileCheckpointStore;
use groose_execution::control::ChannelControlReceiver;
use groose_execution::{BroadcastEventSink, EngineEvent, ExecutionOutcome};

/// Groose CLI — run autonomous software engineering tasks.
#[derive(Parser)]
#[command(name = "groose", version = "0.1.0", about = "Event-driven autonomous software engineering agent")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a task from a specification file
    Run {
        /// Path to task JSON file.
        #[arg(short = 's', long = "spec", required = true)]
        spec_path: String,

        /// Path to the workspace/repository.
        #[arg(short = 'w', long = "workspace", required = true)]
        workspace_path: String,

        /// Test command to run (e.g., "cargo test").
        #[arg(short = 't', long = "test-command", default_value = "")]
        test_command: String,

        /// Lint command to run (e.g., "cargo fmt --check").
        #[arg(short = 'l', long = "lint-command", default_value = "")]
        lint_command: String,

        /// Maximum iterations for the coding loop.
        #[arg(short = 'i', long = "max-iterations", default_value_t = 40)]
        max_iterations: u32,

        /// Checkpoint frequency (iterations between checkpoints).
        #[arg(short = 'c', long = "checkpoint-every", default_value_t = 5)]
        checkpoint_every: u32,

        /// Timeout in minutes.
        #[arg(short = 'T', long = "timeout", default_value_t = 90)]
        timeout_minutes: u32,

        /// Checkpoint directory.
        #[arg(long = "checkpoint-dir", default_value = ".groose/checkpoints")]
        checkpoint_dir: String,
    },
}

async fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Run {
            spec_path,
            workspace_path,
            test_command,
            lint_command,
            max_iterations,
            checkpoint_every,
            timeout_minutes,
            checkpoint_dir,
        } => {
            run_task(
                &spec_path,
                &workspace_path,
                &test_command,
                &lint_command,
                max_iterations,
                checkpoint_every,
                timeout_minutes,
                &checkpoint_dir,
            )
            .await
        }
    }
}

fn main() -> Result<()> {
    let handle = std::thread::Builder::new()
        .name("groose-main".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("Failed to build Tokio runtime");
            runtime.block_on(run())
        })
        .map_err(|e| anyhow::anyhow!("Failed to spawn groose main thread: {}", e))?;

    handle
        .join()
        .map_err(|_| anyhow::anyhow!("groose main thread panicked"))?
}

async fn run_task(
    spec_path: &str,
    workspace_path: &str,
    test_command: &str,
    lint_command: &str,
    max_iterations: u32,
    checkpoint_every: u32,
    timeout_minutes: u32,
    checkpoint_dir: &str,
) -> Result<()> {
    // 1. Load the task specification
    let spec_file = Path::new(spec_path);
    if !spec_file.exists() {
        anyhow::bail!("task specification not found: {}", spec_path);
    }

    let task = if spec_path.ends_with(".yaml") || spec_path.ends_with(".yml") {
        groose_task::TaskSpec::from_yaml_file(spec_file)?
    } else {
        groose_task::TaskSpec::from_json_file(spec_file)?
    };

    eprintln!("✅ Loaded task: {} — {}", task.task_id, task.instruction.goal);

    // 2. Set up workspace
    let workspace_root = Path::new(workspace_path);
    if !workspace_root.exists() {
        anyhow::bail!("workspace not found: {}", workspace_path);
    }

    let workspace = LocalWorkspace::new(workspace_root.to_path_buf());

    // 3. Set up checkpoint store
    let cp_dir = Path::new(checkpoint_dir);
    if let Some(parent) = cp_dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let checkpoint_store = FileCheckpointStore::new(cp_dir.to_path_buf())?;

    // 4. Set up verifier
    let verifier = BasicVerifier::new(test_command, lint_command);

    // 5. Set up control and events
    let (control_receiver, _control_sender) = ChannelControlReceiver::new();
    let (event_sink, mut event_rx) = BroadcastEventSink::new(128);

    // 6. Set up model (Phase 1: mock/shell provider)
    let model = ShellModelProvider::mock("phase1-mock");

    // 7. Create execution context
    let timeout = Duration::from_secs(timeout_minutes as u64 * 60);
    let context = groose_execution::ExecutionContext {
        task: task.clone(),
        workspace_root: workspace_root.to_path_buf(),
        max_iterations,
        max_cost_usd: 12.0,
        timeout,
        checkpoint_every,
    };

    // 8. Spawn event consumer
    let event_handle = tokio::task::spawn(async move {
        while let Ok(event) = event_rx.recv().await {
            match event {
                EngineEvent::Started { .. } => eprintln!("🚀 Engine started"),
                EngineEvent::PlanFormed { plan, .. } => eprintln!("📋 Plan:\n{}", plan),
                EngineEvent::Action { action, .. } => eprintln!("⚡ {}", action),
                EngineEvent::Progress { iteration, .. } => {
                    eprintln!("📊 iteration {}", iteration);
                }
                EngineEvent::Checkpoint { version, .. } => eprintln!("💾 checkpoint v{}", version),
                EngineEvent::Verification { result, .. } => {
                    eprintln!("🔍 verification: {:?}", result);
                }
                EngineEvent::Completed { .. } => eprintln!("✅ Completed"),
                EngineEvent::Failed { error, .. } => {
                    eprintln!("❌ Failed: {}", error);
                }
                EngineEvent::BudgetExceeded { reason, .. } => {
                    eprintln!("💰 Budget exceeded: {}", reason);
                }
                EngineEvent::Cancelled { reason, .. } => {
                    eprintln!("⏹️  Cancelled: {}", reason);
                }
                _ => {}
            }
        }
    });

    // 9. Run the engine
    let engine = CodingEngine::new("groose");
    let outcome = engine
        .run(
            &context.task,
            &context,
            &workspace,
            &control_receiver,
            &event_sink,
            &checkpoint_store,
            &verifier,
            &model,
        )
        .await?;

    // 10. Report outcome
    match outcome {
        ExecutionOutcome::Completed => println!("✅ Task completed successfully!"),
        ExecutionOutcome::Failed { error } => println!("❌ Task failed: {}", error),
        ExecutionOutcome::BudgetExceeded { reason } => println!("💰 Budget exceeded: {}", reason),
        ExecutionOutcome::Cancelled { reason } => println!("⏹️  Task cancelled: {}", reason),
        ExecutionOutcome::PendingReview { pr_url } => {
            println!("👀 Task pending review: {}", pr_url.unwrap_or("N/A".to_string()));
        }
    }

    // Wait for event consumer
    event_handle.await.ok();

    Ok(())
}