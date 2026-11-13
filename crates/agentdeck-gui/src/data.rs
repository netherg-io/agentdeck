//! Read-only snapshot behind the Instances page: registry instances, T3 health
//! and quota bars from the usage state file. A missing state file is "no
//! data", never an error; nothing here writes.

use std::collections::HashMap;
use std::path::PathBuf;

use agentdeck_core::{Paths, Registry, plugins, processes, t3};
use agentdeck_usage::model::{self, AppState};

/// Canonical usage state file, then the old ai-usage path the daemon still
/// compat-writes (a missing file means "no data", not an error).
fn state_file_candidates(paths: &Paths) -> Vec<PathBuf> {
    vec![
        paths.state_dir.join("usage").join("state.json"),
        agentdeck_usage::util::legacy_persistent_state_dir().join("state.json"),
    ]
}

fn read_state(path: &std::path::Path) -> Option<AppState> {
    let raw = std::fs::read(path).ok()?;
    let state: AppState = serde_json::from_slice(&raw).ok()?;
    (state.schema_version == model::SCHEMA_VERSION).then_some(state)
}

#[derive(Debug, Clone, Default)]
pub struct T3Info {
    pub status: Option<String>,
    pub threads: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct QuotaBar {
    pub label: String,
    pub used_percent: f64,
    pub scope: Option<String>,
    /// Whole minutes until the window resets, when the provider reports it.
    pub resets_in_mins: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct UsageQuotas {
    pub usage_id: String,
    pub display: String,
    pub status: String,
    pub error: Option<String>,
    pub bars: Vec<QuotaBar>,
}

#[derive(Debug, Clone, Default)]
pub struct InstanceRow {
    pub id: String,
    pub display_name: String,
    pub harness: String,
    pub enabled: bool,
    pub max_parallel: Option<u32>,
    pub t3: Option<T3Info>,
    pub quotas: Vec<UsageQuotas>,
}

#[derive(Debug, Clone, Default)]
pub struct PluginRow {
    pub name: String,
    pub enabled: bool,
    pub available: bool,
    pub detail: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Data {
    /// False until the first load finished, so the UI never claims "empty" while reading.
    pub loaded: bool,
    pub instances: Vec<InstanceRow>,
    pub standalone: Vec<UsageQuotas>,
    pub plugins: Vec<PluginRow>,
    /// Registry-level failure (unreadable file); per-source gaps stay silent.
    pub error: Option<String>,
    pub processes: Vec<processes::SessionGroup>,
    /// `[sync] auto_apply`: background sync applies additive drift itself.
    pub auto_apply: bool,
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Whole minutes until `resets_at`, or `None` when already past / unknown.
fn resets_in_mins(now: i64, resets_at: Option<i64>) -> Option<i64> {
    resets_at.and_then(|t| {
        let mins = (t - now).div_euclid(60);
        (mins > 0).then_some(mins)
    })
}

fn quotas_for(
    usage_id: &str,
    display: &str,
    kind: &str,
    accounts: &[model::AccountState],
) -> UsageQuotas {
    let account = accounts
        .iter()
        .find(|a| a.id == usage_id && a.provider == kind);
    let now = now_secs();
    UsageQuotas {
        usage_id: usage_id.to_string(),
        display: display.to_string(),
        status: account.map(|a| a.status.clone()).unwrap_or_default(),
        error: account.and_then(|a| a.error.clone()),
        bars: account
            .map(|a| {
                a.windows
                    .iter()
                    .map(|w| QuotaBar {
                        label: w.label.clone(),
                        used_percent: w.used_percent,
                        scope: w.scope.clone(),
                        resets_in_mins: resets_in_mins(now, w.resets_at),
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// Load everything the Instances page shows. Never fails: gaps become dashes.
pub fn load(paths: &Paths) -> Data {
    let registry = match Registry::load(paths) {
        Ok(registry) => registry,
        Err(error) => {
            return Data {
                loaded: true,
                error: Some(format!("registry: {error:#}")),
                ..Data::default()
            };
        }
    };
    let t3_rows: HashMap<String, (Option<String>, Option<u64>)> = t3::t3_root()
        .ok()
        .and_then(|root| t3::t3_status(paths, &root).ok())
        .map(|rows| {
            rows.into_iter()
                .map(|r| (r.id, (r.cache.map(|c| c.status), r.threads)))
                .collect()
        })
        .unwrap_or_default();
    let accounts: Vec<model::AccountState> = state_file_candidates(paths)
        .iter()
        .find_map(|path| read_state(path))
        .map(|s| s.accounts)
        .unwrap_or_default();

    let mut data = Data {
        loaded: true,
        ..Data::default()
    };
    for inst in &registry.instances {
        let quotas = registry
            .usage
            .iter()
            .filter(|u| u.instance.as_deref() == Some(inst.id.as_str()))
            .map(|u| quotas_for(&u.id, &u.display, u.kind.as_str(), &accounts))
            .collect();
        data.instances.push(InstanceRow {
            id: inst.id.clone(),
            display_name: inst.display_name.clone(),
            harness: inst.harness.as_str().to_string(),
            enabled: inst.enabled,
            max_parallel: inst.max_parallel,
            t3: t3_rows.get(&inst.id).map(|(status, threads)| T3Info {
                status: status.clone(),
                threads: *threads,
            }),
            quotas,
        });
    }
    data.standalone = registry
        .usage
        .iter()
        .filter(|u| u.instance.is_none())
        .map(|u| quotas_for(&u.id, &u.display, u.kind.as_str(), &accounts))
        .collect();
    // Plugin status is read-only (link/unit/block presence); a missing HOME
    // just reports everything absent.
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let exe = agentdeck_core::current_exe().unwrap_or_default();
    data.plugins = plugins::status(paths, &home, &exe)
        .into_iter()
        .map(|s| PluginRow {
            name: s.name.to_string(),
            enabled: s.enabled,
            available: s.available,
            detail: s.detail,
        })
        .collect();
    // Read-only /proc snapshot; gaps become dashes, never an error.
    data.processes = processes::snapshot().groups;
    data.auto_apply = agentdeck_core::sync::auto_apply_enabled(paths);
    data
}

#[cfg(test)]
mod tests {
    use super::resets_in_mins;

    #[test]
    fn reset_countdown_rounds_down_and_ignores_the_past() {
        assert_eq!(
            resets_in_mins(1_000, Some(1_000 + 2 * 3_600 + 15 * 60)),
            Some(135)
        );
        assert_eq!(resets_in_mins(1_000, Some(1_000 + 40 * 60)), Some(40));
        assert_eq!(resets_in_mins(1_000, Some(1_000)), None);
        assert_eq!(resets_in_mins(1_000, Some(999)), None);
        assert_eq!(resets_in_mins(1_000, None), None);
    }
}
