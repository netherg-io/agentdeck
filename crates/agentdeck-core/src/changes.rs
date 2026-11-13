//! Dry-run plumbing reused by later units: every write is a `Change` that can
//! be printed (dry-run) or applied.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub enum Change {
    WriteFile {
        path: PathBuf,
        contents: Vec<u8>,
        old: Option<Vec<u8>>,
        /// Copy of the pre-write file, made before overwriting (`None` keeps
        /// the old no-backup behaviour, e.g. registry writes back up elsewhere).
        backup: Option<PathBuf>,
    },
    Mkdir {
        path: PathBuf,
    },
    Symlink {
        link: PathBuf,
        target: PathBuf,
    },
    BackupRemove {
        path: PathBuf,
        backup: PathBuf,
    },
    /// Remove a file agentdeck generated (verified by marker before planning).
    Remove {
        path: PathBuf,
    },
    /// Like `WriteFile` but the result is executable (wrapper scripts, 0755).
    WriteScript {
        path: PathBuf,
        contents: Vec<u8>,
        old: Option<Vec<u8>>,
    },
}

impl Change {
    pub fn describe(&self) -> String {
        match self {
            Change::WriteFile {
                path,
                contents,
                old,
                backup,
            } => {
                let base = match old {
                    None => format!(
                        "write {} (new file, {} bytes)",
                        path.display(),
                        contents.len()
                    ),
                    Some(prev) if prev == contents => {
                        format!("write {} (unchanged)", path.display())
                    }
                    Some(prev) => describe_diff(path, prev, contents),
                };
                match backup {
                    Some(dest) => format!("{base} (backup {})", dest.display()),
                    None => base,
                }
            }
            Change::Mkdir { path } => format!("mkdir {}", path.display()),
            Change::Symlink { link, target } => {
                format!("symlink {} -> {}", link.display(), target.display())
            }
            Change::BackupRemove { path, backup } => {
                format!(
                    "backup {} to {}, then remove",
                    path.display(),
                    backup.display()
                )
            }
            Change::Remove { path } => format!("remove {}", path.display()),
            Change::WriteScript {
                path,
                contents,
                old,
            } => match old {
                None => format!(
                    "write {} (new executable, {} bytes)",
                    path.display(),
                    contents.len()
                ),
                Some(prev) if prev == contents => {
                    format!("write {} (unchanged)", path.display())
                }
                Some(prev) => describe_diff(path, prev, contents),
            },
        }
    }

    pub fn apply(&self) -> anyhow::Result<()> {
        match self {
            Change::WriteFile {
                path,
                contents,
                backup,
                ..
            } => {
                if let Some(parent) = path.parent() {
                    ensure_dir(parent)?;
                }
                // Backup the existing file before overwriting.
                if let Some(dest) = backup
                    && fs::symlink_metadata(path).is_ok()
                {
                    if let Some(parent) = dest.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::copy(path, dest)?;
                    fs::set_permissions(dest, fs::Permissions::from_mode(0o600))?;
                }
                // .claude.json (account state) is always 0600; other existing
                // real files keep their mode; new files and symlink
                // replacements get 0600.
                let keep_mode = path
                    .file_name()
                    .is_none_or(|n| n != ".claude.json")
                    .then(|| fs::symlink_metadata(path).ok())
                    .flatten()
                    .filter(|m| !m.file_type().is_symlink())
                    .map(|m| m.permissions().mode() & 0o777);
                // Same-dir tmp + rename so a crash never leaves a half file.
                let tmp = path.with_extension("tmp");
                fs::write(&tmp, contents)?;
                fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
                fs::rename(&tmp, path)?;
                if let Some(mode) = keep_mode {
                    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
                }
                Ok(())
            }
            Change::Mkdir { path } => {
                ensure_dir(path)?;
                Ok(())
            }
            Change::Symlink { link, target } => {
                if let Some(parent) = link.parent() {
                    ensure_dir(parent)?;
                }
                if fs::symlink_metadata(link).is_ok_and(|m| m.file_type().is_symlink()) {
                    fs::remove_file(link)?;
                }
                std::os::unix::fs::symlink(target, link)?;
                Ok(())
            }
            Change::BackupRemove { path, backup } => {
                // Already gone (an earlier change pruned it): nothing to back up.
                if fs::symlink_metadata(path).is_err() {
                    return Ok(());
                }
                if let Some(parent) = backup.parent() {
                    fs::create_dir_all(parent)?;
                }
                if path.is_dir() && !path.is_symlink() {
                    fs_extra_copy_dir(path, backup)?;
                    fs::remove_dir_all(path)?;
                } else if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
                    // Back up the link itself, never traversing the target
                    // (it may be dangling or belong to another home).
                    let target = fs::read_link(path)?;
                    std::os::unix::fs::symlink(target, backup)?;
                    fs::remove_file(path)?;
                } else {
                    fs::copy(path, backup)?;
                    fs::remove_file(path)?;
                }
                Ok(())
            }
            Change::Remove { path } => {
                match fs::remove_file(path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
                prune_created_parents(path);
                Ok(())
            }
            Change::WriteScript { path, contents, .. } => {
                if let Some(parent) = path.parent() {
                    ensure_dir(parent)?;
                }
                // Same-dir tmp + rename so a crash never leaves a half file.
                let tmp = path.with_extension("tmp");
                fs::write(&tmp, contents)?;
                fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
                fs::rename(&tmp, path)?;
                Ok(())
            }
        }
    }
}

/// Dirs agentdeck created under `$HOME` (one path per line, in its state
/// dir). Removing a generated file later prunes exactly these once empty, so
/// a disabled plugin leaves no `~/.config/systemd/user` or plugin dir behind,
/// while dirs that existed before (`~/.local/bin`) always stay.
const CREATED_DIRS: &str = "created-dirs";

fn created_log() -> Option<(PathBuf, crate::paths::Paths)> {
    let paths = crate::paths::Paths::from_env().ok()?;
    Some((paths.state_dir.join(CREATED_DIRS), paths))
}

fn read_created(log: &Path) -> Vec<PathBuf> {
    fs::read_to_string(log)
        .map(|t| {
            t.lines()
                .filter(|l| !l.is_empty())
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

/// `create_dir_all` that records the dirs it actually created.
fn ensure_dir(dir: &Path) -> std::io::Result<()> {
    let mut missing = Vec::new();
    let mut d = Some(dir);
    while let Some(x) = d {
        if fs::symlink_metadata(x).is_ok() {
            break;
        }
        missing.push(x.to_path_buf());
        d = x.parent();
    }
    fs::create_dir_all(dir)?;
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Ok(());
    };
    let Some((log, paths)) = created_log() else {
        return Ok(());
    };
    // agentdeck's own config/state dirs are data, never pruned.
    let own = |m: &Path| {
        [&paths.state_dir, &paths.config_dir]
            .iter()
            .any(|o| m.starts_with(o) || o.starts_with(m))
    };
    let new: Vec<PathBuf> = missing
        .into_iter()
        .filter(|m| m.starts_with(&home) && *m != home && !own(m))
        .collect();
    if new.is_empty() {
        return Ok(());
    }
    let mut all = read_created(&log);
    all.extend(new);
    all.sort();
    all.dedup();
    if let Some(parent) = log.parent() {
        fs::create_dir_all(parent)?;
    }
    let body: String = all.iter().map(|p| format!("{}\n", p.display())).collect();
    fs::write(&log, body)
}

/// After removing a generated file, drop the empty dirs agentdeck created
/// for it, innermost first.
fn prune_created_parents(path: &Path) {
    let Some((log, _)) = created_log() else {
        return;
    };
    let mut all = read_created(&log);
    let before = all.len();
    let mut dir = path.parent();
    while let Some(d) = dir {
        let Some(i) = all.iter().position(|p| p == d) else {
            break;
        };
        // remove_dir only succeeds on an empty dir.
        if fs::remove_dir(d).is_err() {
            break;
        }
        all.remove(i);
        dir = d.parent();
    }
    if all.len() != before {
        let _ = if all.is_empty() {
            fs::remove_file(&log)
        } else {
            fs::write(
                &log,
                all.iter()
                    .map(|p| format!("{}\n", p.display()))
                    .collect::<String>(),
            )
        };
    }
}

fn fs_extra_copy_dir(src: &Path, dst: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        let ft = entry.file_type()?;
        if ft.is_dir() {
            fs_extra_copy_dir(&entry.path(), &to)?;
        } else if ft.is_symlink() {
            let target = fs::read_link(entry.path())?;
            std::os::unix::fs::symlink(target, to)?;
        } else {
            fs::copy(entry.path(), to)?;
        }
    }
    Ok(())
}

pub fn apply_changes(changes: &[Change]) -> anyhow::Result<()> {
    for change in changes {
        change.apply()?;
    }
    Ok(())
}

/// Backup destination for a path that must be moved aside before linking:
/// `<state_dir>/backups/<file-name>.<unix-secs>.<nanos>`.
pub fn backup_dest(state_dir: &Path, file_name: &str) -> PathBuf {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    state_dir.join("backups").join(format!(
        "{file_name}.{}.{:09}",
        now.as_secs(),
        now.subsec_nanos()
    ))
}

/// Build a `WriteFile` change, or `None` when old and new bytes are identical.
pub fn plan_write_file(path: PathBuf, old: Option<&[u8]>, new: &[u8]) -> Option<Change> {
    if old == Some(new) {
        return None;
    }
    Some(Change::WriteFile {
        path,
        contents: new.to_vec(),
        old: old.map(|b| b.to_vec()),
        backup: None,
    })
}

/// Build a `WriteScript` change, or `None` when old and new bytes are identical.
pub fn plan_write_script(path: PathBuf, old: Option<&[u8]>, new: &[u8]) -> Option<Change> {
    if old == Some(new) {
        return None;
    }
    Some(Change::WriteScript {
        path,
        contents: new.to_vec(),
        old: old.map(|b| b.to_vec()),
    })
}

/// Line-set diff used by redacted printers too (masked texts in, diff out).
pub fn describe_diff(path: &Path, old: &[u8], new: &[u8]) -> String {
    const MAX_LINES: usize = 30;
    let header = format!("write {}", path.display());
    let (Ok(old_text), Ok(new_text)) = (std::str::from_utf8(old), std::str::from_utf8(new)) else {
        return format!("{header} ({} bytes, binary)", new.len());
    };
    if old_text.contains('\0') || new_text.contains('\0') {
        return format!("{header} ({} bytes, binary)", new.len());
    }
    let old_lines: Vec<&str> = old_text.lines().collect();
    let new_lines: Vec<&str> = new_text.lines().collect();
    // shortcut: line-set diff, good enough for small generated TOML; upgrade to a
    // real unified diff if files this describes ever grow large.
    let mut out = vec![
        format!("{header}:"),
        "--- a".to_string(),
        "+++ b".to_string(),
    ];
    let mut shown = 0;
    for line in &old_lines {
        if !new_lines.contains(line) {
            out.push(format!("-{line}"));
            shown += 1;
        }
    }
    for line in &new_lines {
        if !old_lines.contains(line) {
            out.push(format!("+{line}"));
            shown += 1;
        }
    }
    if shown > MAX_LINES {
        out.truncate(3 + MAX_LINES);
        out.push("... (truncated)".to_string());
    }
    if shown == 0 {
        return format!("{header} ({} bytes)", new.len());
    }
    out.join("\n")
}
