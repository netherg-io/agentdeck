//! External Watchdog support for long-horizon Autonomous Runs (ADR-0002).
//!
//! The Coordinator itself must not be trusted with its own liveness: a quota
//! limit or a forgotten wake-up kills its context. This crate holds the
//! daemon-side decision logic — Coordinator Pool selection, the Jev-gated
//! Checkpoint judge, the loopback T3 thread controller plus a read-only
//! `statev2.sqlite` observer, the reactive Watchdog tick engine, and
//! pre-dispatch resource hygiene — as pure, testable functions over small
//! adapter structs (`RunSnapshot`, `WatchdogThreadInput`). Wave 2 maps these
//! directly from `agentdeck_core::thread_state::ThreadState`.

pub mod checkpoint;
pub mod config;
pub mod hygiene;
pub mod pool;
pub mod t3_control;
pub mod watchdog;

pub use checkpoint::{CheckpointInput, CheckpointJudge, CheckpointVerdict};
pub use config::{CoordinatorConfig, CoordinatorPoolEntry};
pub use pool::{PoolSelection, QuotaWindowView};
pub use t3_control::{HttpT3Controller, ObservedThread, T3ThreadController};
pub use watchdog::{ActionDebounce, RunPhase, RunSnapshot, WatchdogAction, WatchdogThreadInput};
