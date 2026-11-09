use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use tokio::sync::mpsc;

/// A signal sent to the control receiver to affect execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlSignal {
    /// Cancel execution as soon as possible.
    Cancel { reason: String },
    /// Pause at the next safe checkpoint.
    Pause,
    /// Resume from a paused state.
    Resume,
    /// Apply an amendment to the task.
    Amend {
        instruction_goal: Option<String>,
        acceptance_criteria: Option<Vec<String>>,
    },
}

/// Receiver for control signals that can affect a running engine.
#[async_trait]
pub trait ControlReceiver: Send + Sync {
    /// Wait for the next control signal, blocking until one arrives.
    async fn receive(&self) -> Result<ControlSignal>;
    /// Try to fetch a control signal without blocking.
    async fn try_receive(&self) -> Result<Option<ControlSignal>>;
    /// Send a control signal.
    async fn send(&self, signal: ControlSignal) -> Result<()>;
    /// Check if cancellation has been requested.
    async fn is_cancelled(&self) -> bool;
}

/// A control receiver backed by an MPSC channel.
/// Uses a separate watch channel for cancellation to avoid holding locks across await.
pub struct ChannelControlReceiver {
    rx: Mutex<mpsc::UnboundedReceiver<ControlSignal>>,
    tx: mpsc::UnboundedSender<ControlSignal>,
    cancelled_flag: std::sync::atomic::AtomicBool,
}

impl ChannelControlReceiver {
    pub fn new() -> (Self, mpsc::UnboundedSender<ControlSignal>) {
        let (tx, rx) = mpsc::unbounded_channel::<ControlSignal>();
        let tx_clone = tx.clone();
        (
            Self {
                rx: Mutex::new(rx),
                tx,
                cancelled_flag: std::sync::atomic::AtomicBool::new(false),
            },
            tx_clone,
        )
    }
}

#[async_trait]
impl ControlReceiver for ChannelControlReceiver {
    async fn receive(&self) -> Result<ControlSignal> {
        loop {
            let sig = self.rx.lock().unwrap().try_recv();
            match sig {
                Ok(signal) => {
                    if matches!(signal, ControlSignal::Cancel { .. }) {
                        self.cancelled_flag
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    return Ok(signal);
                }
                Err(mpsc::error::TryRecvError::Empty) => {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    return Err(anyhow!("control channel disconnected"));
                }
            }
        }
    }

    async fn try_receive(&self) -> Result<Option<ControlSignal>> {
        match self.rx.lock().unwrap().try_recv() {
            Ok(signal) => {
                if matches!(signal, ControlSignal::Cancel { .. }) {
                    self.cancelled_flag
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                }
                Ok(Some(signal))
            }
            Err(mpsc::error::TryRecvError::Empty) => Ok(None),
            Err(mpsc::error::TryRecvError::Disconnected) => {
                Err(anyhow!("control channel disconnected"))
            }
        }
    }

    async fn send(&self, signal: ControlSignal) -> Result<()> {
        self.tx
            .send(signal)
            .map_err(|_| anyhow!("control channel closed"))?;
        Ok(())
    }

    async fn is_cancelled(&self) -> bool {
        // Drain any pending Cancel signals
        loop {
            match self.rx.lock().unwrap().try_recv() {
                Ok(ControlSignal::Cancel { .. }) => {
                    self.cancelled_flag
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                }
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        self.cancelled_flag
            .load(std::sync::atomic::Ordering::SeqCst)
    }
}
