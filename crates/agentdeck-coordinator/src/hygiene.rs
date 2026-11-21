//! Pre-dispatch resource hygiene (Q13): refuse to dispatch when the disk is
//! nearly full, and surface stale git worktrees/clones for cleanup. All
//! discovery is read-only; the daemon deletes nothing by itself.

use std::collections::HashSet;
use std::ffi::CString;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct DiskCheck {
    pub free_gb: u64,
    pub min_gb: u64,
    pub ok: bool,
}

/// Free bytes on the filesystem holding `path`, via `statvfs`.
pub fn free_disk_gb(path: &Path) -> anyhow::Result<u64> {
    let c = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| anyhow::anyhow!("unrepresentable path for statvfs"))?;
    let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: statvfs writes a complete struct on success; the CString
    // outlives the call and the buffer is stack-owned.
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut buf) };
    if rc != 0 {
        anyhow::bail!("statvfs failed for {}", path.display());
    }
    Ok(buf.f_bavail as u64 * buf.f_frsize as u64 / (1024 * 1024 * 1024))
}

pub fn check_disk_ok(path: &Path, min_gb: u64) -> DiskCheck {
    match free_disk_gb(path) {
        Ok(free_gb) => DiskCheck {
            free_gb,
            min_gb,
            ok: free_gb >= min_gb,
        },
        // Unreadable disk stats fail closed: dispatching blind risks the
        // 64 KB-free stall from the Blockfield run.
        Err(_) => DiskCheck {
            free_gb: 0,
            min_gb,
            ok: false,
        },
    }
}

fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Read-only discovery of stale checkouts under `work_dir`: clean git
/// worktrees/clones (not in `active_names`) whose HEAD is an ancestor of
/// `origin/main`, or whose current branch is fully merged into it. Dirty
/// dirs, non-git dirs and anything git cannot confirm are never flagged —
/// the daemon must not touch unreviewed work.
pub fn find_stale_worktrees(work_dir: &Path, active_names: &HashSet<String>) -> Vec<PathBuf> {
    let entries = std::fs::read_dir(work_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .collect::<Vec<_>>();
    let mut stale = Vec::new();
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if active_names.contains(&name) {
            continue;
        }
        // Not a git checkout at all.
        if git_out(&path, &["rev-parse", "--git-dir"]).is_none() {
            continue;
        }
        // Dirty (including untracked): hands off.
        if git_out(&path, &["status", "--porcelain"]).is_none_or(|s| !s.is_empty()) {
            continue;
        }
        let ancestor = git_out(&path, &["merge-base", "--is-ancestor", "HEAD", "origin/main"]).is_some();
        let branch = git_out(&path, &["branch", "--show-current"]).unwrap_or_default();
        let merged = !branch.is_empty()
            && git_out(&path, &["branch", "--merged", "origin/main"])
                .is_some_and(|list| list.lines().any(|l| l.trim().trim_start_matches("* ").trim() == branch));
        if ancestor || merged {
            stale.push(path);
        }
    }
    stale.sort();
    stale
}
