//! Registry and harness adapters shared by the agentdeck CLI, GUI and daemon.

pub mod agy;
pub mod changes;
pub mod claude;
pub mod codex;
pub mod harness;
pub mod imagegen;
pub mod launch;
pub mod login;
pub mod opencode;
pub mod paths;
pub mod plugins;
pub mod pool;
pub mod processes;
pub mod registry;
pub mod router;
pub mod service;
pub mod settings;
pub mod shared;
pub mod sync;
pub mod t3;
pub mod usage;
pub mod wrappers;

pub use changes::{Change, apply_changes, plan_write_file, plan_write_script};
pub use paths::Paths;
pub use pool::{PoolEntry, PoolValue};
pub use registry::{AddSpec, Harness, Instance, Registry};
pub use usage::{UsageEntry, UsageKind};
