//! Registry and harness adapters shared by the agentdeck CLI, GUI and daemon.

pub mod agy;
pub mod changes;
pub mod claude;
pub mod codex;
pub mod harness;
pub mod imagegen;
pub mod launch;
pub mod login;
pub mod manage;
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

/// `std::env::current_exe`, minus the " (deleted)" suffix Linux reports after
/// `cargo install` replaced the running binary; the installed path is what
/// generated hooks, units and instruction blocks must name.
pub fn current_exe() -> std::io::Result<std::path::PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(exe
        .to_str()
        .and_then(|s| s.strip_suffix(" (deleted)"))
        .map(std::path::PathBuf::from)
        .unwrap_or(exe))
}
