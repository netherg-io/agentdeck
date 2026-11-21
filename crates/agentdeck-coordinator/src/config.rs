//! Coordinator daemon configuration: the dedicated Coordinator Pool plus
//! Watchdog / Checkpoint / hygiene thresholds (ADR-0002).

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

fn default_weekly_reserve() -> f64 {
    10.0
}
fn default_proactive_below_5h() -> f64 {
    10.0
}
fn default_sleep_buffer() -> u64 {
    120
}
fn default_wake_grace() -> u64 {
    180
}
fn default_soft_runs() -> u32 {
    80
}
fn default_soft_items() -> u32 {
    1500
}
fn default_confidence() -> f64 {
    0.80
}
fn default_min_disk() -> u64 {
    15
}
fn default_enabled() -> bool {
    true
}

/// One strong-model instance eligible to host the Coordinator. Cheap worker
/// models never belong here: a rotated-to Coordinator must be able to run
/// the whole Autonomous Run by itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoordinatorPoolEntry {
    pub instance_id: String,
    pub model: String,
    #[serde(default)]
    pub options: serde_json::Map<String, serde_json::Value>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

impl CoordinatorPoolEntry {
    pub fn new(instance_id: &str, model: &str) -> Self {
        Self {
            instance_id: instance_id.to_string(),
            model: model.to_string(),
            options: Default::default(),
            enabled: true,
        }
    }

    pub fn options_map(&self) -> HashMap<String, serde_json::Value> {
        self.options.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }
}

/// Daemon-side knobs. Defaults encode ADR-0002: weekly reserve 10%,
/// proactive rotation below 10% 5h, `resets_at + 2 min` sleep, 3 min idle
/// grace before a wake, Checkpoint soft limits (80 runs / 1500 items),
/// 0.80 judge confidence, 15 GB free disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoordinatorConfig {
    #[serde(default)]
    pub pool: Vec<CoordinatorPoolEntry>,
    #[serde(default = "default_weekly_reserve")]
    pub min_weekly_reserve_pct: f64,
    #[serde(default = "default_proactive_below_5h")]
    pub proactive_rotate_below_5h_pct: f64,
    #[serde(default = "default_sleep_buffer")]
    pub quota_sleep_buffer_secs: u64,
    #[serde(default = "default_wake_grace")]
    pub idle_wake_grace_secs: u64,
    #[serde(default = "default_soft_runs")]
    pub checkpoint_soft_runs: u32,
    #[serde(default = "default_soft_items")]
    pub checkpoint_soft_items: u32,
    #[serde(default = "default_confidence")]
    pub checkpoint_confidence_threshold: f64,
    #[serde(default = "default_min_disk")]
    pub min_free_disk_gb: u64,
    #[serde(default)]
    pub t3_endpoint: Option<String>,
    #[serde(default)]
    pub t3_token_file: Option<PathBuf>,
    #[serde(default)]
    pub judge_provider: Option<String>,
    #[serde(default)]
    pub judge_model: Option<String>,
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            pool: Vec::new(),
            min_weekly_reserve_pct: default_weekly_reserve(),
            proactive_rotate_below_5h_pct: default_proactive_below_5h(),
            quota_sleep_buffer_secs: default_sleep_buffer(),
            idle_wake_grace_secs: default_wake_grace(),
            checkpoint_soft_runs: default_soft_runs(),
            checkpoint_soft_items: default_soft_items(),
            checkpoint_confidence_threshold: default_confidence(),
            min_free_disk_gb: default_min_disk(),
            t3_endpoint: None,
            t3_token_file: None,
            judge_provider: None,
            judge_model: None,
        }
    }
}
