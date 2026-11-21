//! Append-only 0600 JSONL audit log for approval decisions.
//!
//! Records the true decision source (`PolicyJudge` vs `Telegram`) for operator
//! review and calibration while wire responses remain opaque. Never stores raw
//! secret values.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuditSource {
    PolicyJudge,
    Telegram,
    Denied,
    Timeout,
    ClientCancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditRecord {
    pub timestamp: u64,
    pub request_id: String,
    pub host: String,
    pub kind: String,
    pub sha256_payload: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    pub perimeter: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge_allow: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge_confidence: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge_reason: Option<String>,
    pub outcome: String,
    pub source: AuditSource,
    pub elapsed_ms: u64,
}

pub fn unix_timestamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn audit_log_path(state_dir: &Path) -> PathBuf {
    state_dir.join("approver").join("audit.jsonl")
}

/// Append one JSONL record to `path`, enforcing 0600 permissions.
pub fn append_record(path: &Path, record: &AuditRecord) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create audit dir {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("failed to open audit log {}", path.display()))?;
    // Create-time mode is masked by umask, so pin 0600 explicitly.
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to set 0600 on {}", path.display()))?;
    let mut line = serde_json::to_string(record)?;
    line.push('\n');
    file.write_all(line.as_bytes())
        .with_context(|| format!("failed to append audit record to {}", path.display()))?;
    file.flush()?;
    Ok(())
}

/// Read up to `limit` most recent audit records in chronological order.
pub fn read_recent(path: &Path, limit: usize) -> Result<Vec<AuditRecord>> {
    if limit == 0 || !path.exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read audit log {}", path.display()))?;
    let mut records = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let rec: AuditRecord = serde_json::from_str(trimmed)
            .with_context(|| format!("corrupt audit JSONL line in {}", path.display()))?;
        records.push(rec);
    }
    if records.len() > limit {
        let start = records.len() - limit;
        records.drain(..start);
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_audit_path(tag: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-approver-audit-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&dir);
        let path = audit_log_path(&dir);
        (dir, path)
    }

    #[test]
    fn append_and_read_recent_respects_mode_and_limit() {
        let (dir, path) = temp_audit_path("recent");
        for i in 0..5 {
            let rec = AuditRecord {
                timestamp: 1_700_000_000 + i,
                request_id: format!("req-{i}"),
                host: "homeserver".to_string(),
                kind: "root".to_string(),
                sha256_payload: "deadbeef".to_string(),
                thread_id: Some("thread-1".to_string()),
                perimeter: "clear".to_string(),
                judge_allow: Some(true),
                judge_confidence: Some(0.95),
                judge_reason: Some("within mandate".to_string()),
                outcome: "approved".to_string(),
                source: AuditSource::PolicyJudge,
                elapsed_ms: 42,
            };
            append_record(&path, &rec).unwrap();
        }
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let recent = read_recent(&path, 2).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].request_id, "req-3");
        assert_eq!(recent[1].request_id, "req-4");
        let _ = fs::remove_dir_all(&dir);
    }
}
