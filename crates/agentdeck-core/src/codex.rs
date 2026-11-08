//! Codex adapter: materialize and drift-check a Codex shadow home exactly like
//! T3 builds it (installed server version S; see M1 E2 report).
//!
//! The primary instance owns the main home (`~/.codex`); every other Codex
//! instance home is a shadow home holding absolute symlinks into the main
//! home for every main entry except private and shadow-local names. Private
//! files (`auth.json`, its locks, `models_cache.json`) are never linked or
//! created by us; the login flow creates them. `.hook-trust` records are
//! keyed per home, so a real shadow copy is merged (never overwritten) before
//! it is backed up and replaced with a link.

use std::collections::BTreeSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use serde::Serialize;

use crate::changes::{Change, backup_dest};

/// Always-created shared directories (current S constants).
const SHARED_DIRS: [&str; 11] = [
    "sessions",
    "archived_sessions",
    "sqlite",
    "shell_snapshots",
    "worktrees",
    "skills",
    "plugins",
    "cache",
    "logs",
    "mcp-oauth-locks",
    "thread-writer-locks",
];
/// Never linked from the shared home; login creates them.
/// T3 lists only `auth.json` and `models_cache.json`; the lock files Codex
/// creates beside the login are per-account too.
const PRIVATE: [&str; 4] = [
    "auth.json",
    "auth.lock",
    "auth.json.lock",
    "models_cache.json",
];
/// Kept per shadow home, never fanned out.
const SHADOW_LOCAL: [&str; 4] = ["log", "memories", "tmp", ".DS_Store"];
/// Real entries T3 removes recursively, then links (disposable lock state).
const REPLACEABLE: [&str; 2] = ["mcp-oauth-locks", ".sqlite-maintenance.lock"];
/// Shared only when no writer holds it; otherwise left real (deferred).
const WRITER_LOCKS: &str = "thread-writer-locks";
const ACTIVE_LOCK: &str = "active.lock";
/// Merged record-by-record, never overwritten on conflict.
const HOOK_TRUST: &str = ".hook-trust";

fn is_excluded(name: &str) -> bool {
    PRIVATE.contains(&name) || SHADOW_LOCAL.contains(&name)
}

#[derive(Debug, Clone, Serialize)]
pub enum Drift {
    MissingLink {
        link: PathBuf,
        expected: PathBuf,
    },
    WrongTarget {
        link: PathBuf,
        expected: PathBuf,
        actual: Option<PathBuf>,
    },
    RealEntry {
        path: PathBuf,
        kind: String,
        size: u64,
    },
    ExtraEntry {
        path: PathBuf,
    },
    /// Shadow private file (`auth.json`, its locks, `models_cache.json`)
    /// that is a symlink: it would share the account's credentials/cache with
    /// another home. Never repaired by copying; the link is removed and the
    /// user logs in again for that account.
    PrivateLink {
        path: PathBuf,
        actual: Option<PathBuf>,
    },
    SameHome {
        main: PathBuf,
        shadow: PathBuf,
    },
    /// Real entry we refuse to touch yet (active writer lock, merge conflict,
    /// unknown content without `--force-backup`).
    Deferred {
        path: PathBuf,
        reason: String,
    },
}

impl Drift {
    pub fn describe(&self) -> String {
        match self {
            Drift::MissingLink { link, expected } => {
                format!("missing link {} -> {}", link.display(), expected.display())
            }
            Drift::WrongTarget {
                link,
                expected,
                actual,
            } => match actual {
                Some(a) => format!(
                    "link {} points to {}, expected {}",
                    link.display(),
                    a.display(),
                    expected.display()
                ),
                None => format!(
                    "link {} is unreadable, expected {}",
                    link.display(),
                    expected.display()
                ),
            },
            Drift::RealEntry { path, kind, size } => {
                format!(
                    "real {kind} {} (size {size}) where a link is expected",
                    path.display()
                )
            }
            Drift::ExtraEntry { path } => {
                format!("shadow-only {} (left alone)", path.display())
            }
            Drift::PrivateLink { path, actual } => match actual {
                Some(a) => format!(
                    "private {} is a symlink to {}, sharing another home's credentials; remove it and log in again for this account",
                    path.display(),
                    a.display()
                ),
                None => format!(
                    "private {} is an unreadable symlink, sharing another home's credentials; remove it and log in again for this account",
                    path.display()
                ),
            },
            Drift::SameHome { main, shadow } => format!(
                "shadow home {} is the same as or nested in main home {}; refusing to link a home into itself",
                shadow.display(),
                main.display()
            ),
            Drift::Deferred { path, reason } => {
                format!("deferred {}: {reason}", path.display())
            }
        }
    }
}

pub struct FixPlan {
    pub changes: Vec<Change>,
    pub deferred: Vec<String>,
}

/// Names T3 links into the shadow: known dirs plus current main names,
/// minus private and shadow-local names. Sorted for deterministic output.
fn link_set(main_names: &BTreeSet<String>) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = SHARED_DIRS.iter().map(|s| s.to_string()).collect();
    out.extend(main_names.iter().cloned());
    out.retain(|n| !is_excluded(n));
    out
}

fn main_entries(main: &Path) -> anyhow::Result<BTreeSet<String>> {
    match fs::read_dir(main) {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(BTreeSet::new()),
        Err(e) => Err(e.into()),
        Ok(rd) => {
            let mut out = BTreeSet::new();
            for entry in rd {
                let entry = entry?;
                out.insert(entry.file_name().to_string_lossy().into_owned());
            }
            Ok(out)
        }
    }
}

/// Lexical `shadow_dir/readlink` resolution: normalizes `.`/`..` without
/// touching the fs, so relative links and links into symlinked mains compare
/// right and nothing foreign is ever traversed.
fn lexical_join(dir: &Path, target: &Path) -> PathBuf {
    let raw = if target.is_absolute() {
        target.to_path_buf()
    } else {
        dir.join(target)
    };
    let mut out = PathBuf::new();
    for comp in raw.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            _ => out.push(comp.as_os_str()),
        }
    }
    out
}

fn canonical_or_lexical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Same directory by any spelling, or one nested in the other: linking would
/// point the home at itself.
fn self_nested(main: &Path, shadow: &Path) -> bool {
    if main == shadow {
        return true;
    }
    let (m, s) = (canonical_or_lexical(main), canonical_or_lexical(shadow));
    m == s || m.starts_with(&s) || s.starts_with(&m)
}

fn link_ok(shadow_dir: &Path, link: &Path, expected: &Path) -> bool {
    fs::read_link(link)
        .map(|t| lexical_join(shadow_dir, &t) == *expected)
        .unwrap_or(false)
}

fn is_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
}

fn real_kind_size(path: &Path) -> (String, u64) {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => {
            let n = fs::read_dir(path).map(|d| d.count()).unwrap_or(0) as u64;
            (format!("dir ({n} entries)"), n)
        }
        Ok(m) if m.is_file() => ("file".to_string(), m.len()),
        Ok(_) => ("special".to_string(), 0),
        Err(_) => ("missing".to_string(), 0),
    }
}

/// A writer holds the shadow lock dir when a direct child is named exactly
/// `active.lock` (T3's own filename test; no liveness check exists there).
fn has_active_lock(dir: &Path) -> bool {
    fs::symlink_metadata(dir.join(ACTIVE_LOCK)).is_ok()
}

fn mkdir_unless_dir(changes: &mut Vec<Change>, path: PathBuf) -> anyhow::Result<()> {
    if is_dir(&path) {
        return Ok(());
    }
    if fs::symlink_metadata(&path).is_ok() {
        anyhow::bail!("{} exists and is not a directory", path.display());
    }
    changes.push(Change::Mkdir { path });
    Ok(())
}

fn plan_mkdirs(main: &Path, shadow: &Path) -> anyhow::Result<Vec<Change>> {
    let mut changes = Vec::new();
    mkdir_unless_dir(&mut changes, main.to_path_buf())?;
    mkdir_unless_dir(&mut changes, shadow.to_path_buf())?;
    for name in SHARED_DIRS {
        mkdir_unless_dir(&mut changes, main.join(name))?;
    }
    Ok(changes)
}

/// Plan the T3 layout: shared dirs plus an absolute link per eligible entry.
/// Idempotent. A real entry where a link belongs is a conflict error like
/// T3's (run `codex fix`); sync never moves real user data aside on its own.
pub fn plan_materialize(main: &Path, shadow: &Path, _state: &Path) -> anyhow::Result<Vec<Change>> {
    if self_nested(main, shadow) {
        anyhow::bail!(
            "shadow home {} is the same as or nested in main home {}",
            shadow.display(),
            main.display()
        );
    }
    let mut changes = plan_mkdirs(main, shadow)?;
    for name in link_set(&main_entries(main)?) {
        let link = shadow.join(&name);
        let target = main.join(&name);
        match fs::symlink_metadata(&link) {
            Err(e) if e.kind() == ErrorKind::NotFound => {
                changes.push(Change::Symlink { link, target });
            }
            Ok(m) if m.file_type().is_symlink() => {
                if !link_ok(shadow, &link, &target) {
                    changes.push(Change::Symlink { link, target });
                }
            }
            Ok(_) => anyhow::bail!(
                "{} exists and is not a symlink (run `agentdeck codex fix`)",
                link.display()
            ),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(changes)
}

/// Read-only drift scan. Private and shadow-local names are ignored;
/// shadow-only extras are reported, never deleted.
pub fn check(main: &Path, shadow: &Path) -> anyhow::Result<Vec<Drift>> {
    if self_nested(main, shadow) {
        return Ok(vec![Drift::SameHome {
            main: main.to_path_buf(),
            shadow: shadow.to_path_buf(),
        }]);
    }
    let mut out = Vec::new();
    let links = link_set(&main_entries(main)?);
    for name in &links {
        let link = shadow.join(name);
        let expected = main.join(name);
        match fs::symlink_metadata(&link) {
            Err(e) if e.kind() == ErrorKind::NotFound => {
                out.push(Drift::MissingLink { link, expected })
            }
            Ok(m) if m.file_type().is_symlink() => {
                if !link_ok(shadow, &link, &expected) {
                    out.push(Drift::WrongTarget {
                        link: link.clone(),
                        expected,
                        actual: fs::read_link(&link).ok(),
                    });
                }
            }
            Ok(_) if name == WRITER_LOCKS && has_active_lock(&link) => out.push(Drift::Deferred {
                path: link,
                reason: format!("holds {ACTIVE_LOCK}; left real, not a hard failure"),
            }),
            Ok(_) => {
                let (kind, size) = real_kind_size(&link);
                out.push(Drift::RealEntry {
                    path: link,
                    kind,
                    size,
                });
            }
            Err(e) => return Err(e.into()),
        }
    }
    match fs::read_dir(shadow) {
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
        Ok(rd) => {
            for entry in rd {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if links.contains(&name) || is_excluded(&name) {
                    continue;
                }
                // SQLite sidecars of a linked db appear next to the link while it is open.
                if !(name.ends_with("-shm") || name.ends_with("-wal")) {
                    out.push(Drift::ExtraEntry { path: entry.path() });
                }
            }
        }
    }
    // Private files are never linked: a symlink here would share the
    // account's credentials/cache with another home (T3 rejects this too).
    // Regular private files and absent entries are fine and stay silent.
    for name in PRIVATE {
        let path = shadow.join(name);
        if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            out.push(Drift::PrivateLink {
                actual: fs::read_link(&path).ok(),
                path,
            });
        }
    }
    Ok(out)
}

fn is_record_name(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Merge a real shadow `.hook-trust` into the main one: union of disjoint
/// valid records (no-clobber writes), identical records kept, anything else
/// (conflicting bytes, unknown files, symlinks) defers the whole entry.
fn plan_hook_trust_merge(
    main: &Path,
    shadow: &Path,
    state: &Path,
) -> anyhow::Result<Result<Vec<Change>, String>> {
    let main_ht = main.join(HOOK_TRUST);
    let shadow_ht = shadow.join(HOOK_TRUST);
    if !is_dir(&main_ht) {
        return Ok(Err(format!(
            "main {} is not a directory; needs decision",
            main_ht.display()
        )));
    }
    let mut records: Vec<String> = Vec::new();
    for entry in fs::read_dir(&shadow_ht)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !ft.is_file() || ft.is_symlink() || !is_record_name(&name) {
            return Ok(Err(format!(
                "{HOOK_TRUST} holds non-record {name:?}; needs decision"
            )));
        }
        records.push(name);
    }
    let mut main_names = BTreeSet::new();
    for entry in fs::read_dir(&main_ht)? {
        let entry = entry?;
        main_names.insert(entry.file_name().to_string_lossy().into_owned());
    }
    records.sort();
    let mut changes = Vec::new();
    for name in &records {
        let shadow_bytes = fs::read(shadow_ht.join(name))?;
        let main_path = main_ht.join(name);
        match fs::read(&main_path) {
            Err(e) if e.kind() == ErrorKind::NotFound => changes.push(Change::WriteFile {
                path: main_path,
                contents: shadow_bytes,
                old: None,
                backup: None,
            }),
            Err(e) => return Err(e.into()),
            Ok(main_bytes) if main_bytes == shadow_bytes => {}
            Ok(_) => {
                return Ok(Err(format!(
                    "{HOOK_TRUST} record {name} differs between homes; needs decision"
                )));
            }
        }
    }
    changes.push(Change::BackupRemove {
        path: shadow_ht.clone(),
        backup: backup_dest(state, HOOK_TRUST),
    });
    changes.push(Change::Symlink {
        link: shadow_ht,
        target: main_ht,
    });
    Ok(Ok(changes))
}

/// Fix every real entry where a link belongs: merge `.hook-trust`, back up
/// and replace disposable lock dirs, back up anything else only with
/// `force_backup`. Never deletes without a backup; extras are left alone.
pub fn fix(
    main: &Path,
    shadow: &Path,
    state: &Path,
    force_backup: bool,
) -> anyhow::Result<FixPlan> {
    if self_nested(main, shadow) {
        anyhow::bail!(
            "shadow home {} is the same as or nested in main home {}",
            shadow.display(),
            main.display()
        );
    }
    let mut plan = FixPlan {
        changes: plan_mkdirs(main, shadow)?,
        deferred: Vec::new(),
    };
    for name in link_set(&main_entries(main)?) {
        let link = shadow.join(&name);
        let target = main.join(&name);
        match fs::symlink_metadata(&link) {
            Err(e) if e.kind() == ErrorKind::NotFound => {
                plan.changes.push(Change::Symlink { link, target });
            }
            Ok(m) if m.file_type().is_symlink() => {
                if !link_ok(shadow, &link, &target) {
                    plan.changes.push(Change::Symlink { link, target });
                }
            }
            Ok(_) => {
                let replaceable = REPLACEABLE.contains(&name.as_str())
                    || (name == WRITER_LOCKS && is_dir(&link) && !has_active_lock(&link));
                if name == HOOK_TRUST && is_dir(&link) {
                    match plan_hook_trust_merge(main, shadow, state)? {
                        Ok(cs) => plan.changes.extend(cs),
                        Err(reason) => plan.deferred.push(format!("{}: {reason}", link.display())),
                    }
                } else if name == WRITER_LOCKS && has_active_lock(&link) {
                    plan.deferred.push(format!(
                        "{} holds {ACTIVE_LOCK}; left real, not a hard failure",
                        link.display()
                    ));
                } else if replaceable || force_backup {
                    plan.changes.push(Change::BackupRemove {
                        path: link.clone(),
                        backup: backup_dest(state, &name),
                    });
                    plan.changes.push(Change::Symlink { link, target });
                } else {
                    plan.deferred.push(format!(
                        "{} is a real entry with unknown content; re-run with --force-backup after a decision",
                        link.display()
                    ));
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
    // A symlinked private file would share credentials with another home:
    // back up the link itself and leave the entry absent (the user must log
    // in again for that account). Regular private files are never touched.
    for name in PRIVATE {
        let path = shadow.join(name);
        if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            plan.changes.push(Change::BackupRemove {
                path: path.clone(),
                backup: backup_dest(state, name),
            });
            plan.deferred.push(format!(
                "{} was a symlink sharing another home's credentials; removed, log in again for this account",
                path.display()
            ));
        }
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::apply_changes;
    use std::sync::atomic::{AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);

    struct Guard(PathBuf);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct Fx {
        _g: Guard,
        main: PathBuf,
        shadow: PathBuf,
        state: PathBuf,
    }

    fn fixture() -> Fx {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-codex-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&dir);
        let fx = Fx {
            main: dir.join("main"),
            shadow: dir.join("shadow"),
            state: dir.join("state"),
            _g: Guard(dir),
        };
        fs::create_dir_all(&fx.main).unwrap();
        fs::write(fx.main.join("config.toml"), "x = 1\n").unwrap();
        fx
    }

    fn sync(fx: &Fx) {
        let changes = plan_materialize(&fx.main, &fx.shadow, &fx.state).unwrap();
        apply_changes(&changes).unwrap();
    }

    fn rec(name_byte: u8) -> String {
        std::iter::repeat_n(name_byte as char, 64).collect()
    }

    #[test]
    fn fresh_materialize_then_idempotent() {
        let fx = fixture();
        let changes = plan_materialize(&fx.main, &fx.shadow, &fx.state).unwrap();
        assert!(!changes.is_empty());
        // Absolute symlink targets like T3, and all shared dirs exist in main.
        for c in &changes {
            if let Change::Symlink { target, .. } = c {
                assert!(target.is_absolute(), "{target:?}");
            }
        }
        apply_changes(&changes).unwrap();
        for name in SHARED_DIRS {
            assert!(fx.main.join(name).is_dir(), "{name}");
        }
        assert_eq!(
            fs::read_link(fx.shadow.join("config.toml")).unwrap(),
            fx.main.join("config.toml")
        );
        assert!(
            plan_materialize(&fx.main, &fx.shadow, &fx.state)
                .unwrap()
                .is_empty()
        );
        assert!(check(&fx.main, &fx.shadow).unwrap().is_empty());
    }

    #[test]
    fn correct_relative_link_is_kept() {
        let fx = fixture();
        sync(&fx);
        let link = fx.shadow.join("config.toml");
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(Path::new("../main/config.toml"), &link).unwrap();
        assert!(check(&fx.main, &fx.shadow).unwrap().is_empty());
        assert!(
            plan_materialize(&fx.main, &fx.shadow, &fx.state)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn wrong_target_is_replaced() {
        let fx = fixture();
        sync(&fx);
        let link = fx.shadow.join("config.toml");
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink("/tmp/elsewhere", &link).unwrap();
        let drift = check(&fx.main, &fx.shadow).unwrap();
        assert!(
            matches!(drift.as_slice(), [Drift::WrongTarget { .. }]),
            "{drift:?}"
        );
        sync(&fx);
        assert!(check(&fx.main, &fx.shadow).unwrap().is_empty());
    }

    #[test]
    fn real_file_needs_decision_then_forced() {
        let fx = fixture();
        sync(&fx);
        let link = fx.shadow.join("config.toml");
        fs::remove_file(&link).unwrap();
        fs::write(&link, "mine").unwrap();
        let drift = check(&fx.main, &fx.shadow).unwrap();
        assert!(
            matches!(
                drift.as_slice(),
                [Drift::RealEntry { kind, size, .. }] if kind == "file" && *size == 4
            ),
            "{drift:?}"
        );
        assert!(plan_materialize(&fx.main, &fx.shadow, &fx.state).is_err());
        // Without --force-backup: no changes, one deferred note.
        let plan = fix(&fx.main, &fx.shadow, &fx.state, false).unwrap();
        assert!(plan.changes.iter().all(|c| !matches!(
            c,
            Change::BackupRemove { path, .. } if path == &link
        )));
        assert_eq!(plan.deferred.len(), 1);
        assert_eq!(fs::read(&link).unwrap(), b"mine");
        // With --force-backup: backed up outside both homes, then linked.
        let plan = fix(&fx.main, &fx.shadow, &fx.state, true).unwrap();
        assert!(plan.deferred.is_empty());
        apply_changes(&plan.changes).unwrap();
        assert_eq!(fs::read_link(&link).unwrap(), fx.main.join("config.toml"));
        let backup = fs::read_dir(fx.state.join("backups"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("config.toml.")
            })
            .expect("backup outside both homes");
        assert_eq!(fs::read(backup).unwrap(), b"mine");
        assert!(check(&fx.main, &fx.shadow).unwrap().is_empty());
    }

    #[test]
    fn real_dir_extra_and_late_main_entry() {
        let fx = fixture();
        sync(&fx);
        // Real dir where a link belongs.
        let link = fx.shadow.join("cache");
        fs::remove_file(&link).unwrap();
        fs::create_dir_all(&link).unwrap();
        fs::write(link.join("keep.bin"), [1, 2, 3]).unwrap();
        let drift = check(&fx.main, &fx.shadow).unwrap();
        assert!(
            drift.iter().any(|d| matches!(
                d,
                Drift::RealEntry { kind, .. } if kind.starts_with("dir")
            )),
            "{drift:?}"
        );
        // Shadow-only extra: reported, and fix never touches it.
        fs::write(fx.shadow.join("notes.txt"), "scratch").unwrap();
        let drift = check(&fx.main, &fx.shadow).unwrap();
        assert!(
            drift.iter().any(|d| matches!(d, Drift::ExtraEntry { .. })),
            "{drift:?}"
        );
        let plan = fix(&fx.main, &fx.shadow, &fx.state, true).unwrap();
        assert!(
            plan.changes.iter().all(|c| !matches!(
                c,
                Change::BackupRemove { path, .. } if path.ends_with("notes.txt")
            )),
            "{:?}",
            plan.changes
        );
        apply_changes(&plan.changes).unwrap();
        assert!(fs::read(fx.shadow.join("notes.txt")).is_ok(), "extra kept");
        // Main entry added later is linked on the next run.
        fs::write(fx.main.join("new.sqlite"), "db").unwrap();
        let drift = check(&fx.main, &fx.shadow).unwrap();
        assert!(
            drift.iter().any(|d| matches!(d, Drift::MissingLink { .. })),
            "{drift:?}"
        );
        sync(&fx);
        let drift = check(&fx.main, &fx.shadow).unwrap();
        assert!(
            drift
                .iter()
                .all(|d| !matches!(d, Drift::MissingLink { .. })),
            "{drift:?}"
        );
    }

    #[test]
    fn private_files_untouched() {
        let fx = fixture();
        for name in PRIVATE {
            fs::write(fx.main.join(name), "main-secret").unwrap();
        }
        // Login-created private file in the shadow.
        fs::create_dir_all(&fx.shadow).unwrap();
        fs::write(fx.shadow.join("auth.json"), "shadow-login").unwrap();
        sync(&fx);
        for name in PRIVATE {
            assert!(
                fs::symlink_metadata(fx.shadow.join(name))
                    .map(|m| !m.file_type().is_symlink())
                    .unwrap_or(true),
                "{name} must never be linked"
            );
        }
        assert_eq!(
            fs::read(fx.shadow.join("auth.json")).unwrap(),
            b"shadow-login"
        );
        assert!(check(&fx.main, &fx.shadow).unwrap().is_empty());
        // Codex's own login lock exists only in the shadow: not drift.
        fs::remove_file(fx.main.join("auth.json.lock")).unwrap();
        fs::write(fx.shadow.join("auth.json.lock"), "").unwrap();
        assert!(check(&fx.main, &fx.shadow).unwrap().is_empty());
    }

    #[test]
    fn private_symlink_is_drift_and_fix_removes_it() {
        let fx = fixture();
        for name in PRIVATE {
            fs::write(fx.main.join(name), "main-secret").unwrap();
        }
        sync(&fx);
        // Symlinked private files in the shadow share credentials with
        // another home: drift, whatever the target (including dangling).
        std::os::unix::fs::symlink(fx.main.join("auth.json"), fx.shadow.join("auth.json")).unwrap();
        std::os::unix::fs::symlink(fx.main.join("auth.lock"), fx.shadow.join("auth.lock")).unwrap();
        std::os::unix::fs::symlink(
            Path::new("/nonexistent/models_cache.json"),
            fx.shadow.join("models_cache.json"),
        )
        .unwrap();
        let drift = check(&fx.main, &fx.shadow).unwrap();
        assert_eq!(
            drift
                .iter()
                .filter(|d| matches!(d, Drift::PrivateLink { .. }))
                .count(),
            3,
            "{drift:?}"
        );
        assert!(
            drift
                .iter()
                .all(|d| matches!(d, Drift::PrivateLink { .. } | Drift::MissingLink { .. })),
            "{drift:?}"
        );
        // fix backs up each link and leaves the entry absent (log in again);
        // regular private files would never reach this path.
        let plan = fix(&fx.main, &fx.shadow, &fx.state, false).unwrap();
        assert_eq!(
            plan.changes
                .iter()
                .filter(|c| matches!(c, Change::BackupRemove { .. }))
                .count(),
            3,
            "{:?}",
            plan.changes
        );
        assert_eq!(plan.deferred.len(), 3);
        assert!(
            plan.deferred.iter().all(|d| d.contains("log in again")),
            "{:?}",
            plan.deferred
        );
        apply_changes(&plan.changes).unwrap();
        for name in PRIVATE {
            assert!(
                fs::symlink_metadata(fx.shadow.join(name)).is_err(),
                "{name} must be absent after fix"
            );
        }
        // Link identities were backed up outside both homes.
        let backups: Vec<PathBuf> = fs::read_dir(fx.state.join("backups"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(backups.len(), 3);
        assert!(
            backups
                .iter()
                .all(|b| fs::symlink_metadata(b).is_ok_and(|m| m.file_type().is_symlink())),
            "{backups:?}"
        );
        assert!(check(&fx.main, &fx.shadow).unwrap().is_empty());
    }

    #[test]
    fn shadow_local_names_ignored() {
        let fx = fixture();
        fs::create_dir_all(fx.main.join("log")).unwrap();
        sync(&fx);
        assert!(fs::symlink_metadata(fx.shadow.join("log")).is_err());
        assert!(check(&fx.main, &fx.shadow).unwrap().is_empty());
    }

    fn hook_trust_fixture(disjoint: bool) -> (Fx, String, String) {
        let fx = fixture();
        sync(&fx);
        for (dir, names) in [(&fx.main, vec![rec(b'a')]), (&fx.shadow, vec![rec(b'b')])] {
            let link = dir.join(HOOK_TRUST);
            if fs::symlink_metadata(&link)
                .ok()
                .is_some_and(|m| m.file_type().is_symlink())
            {
                fs::remove_file(&link).unwrap();
            }
            fs::create_dir_all(&link).unwrap();
            for n in names {
                fs::write(link.join(&n), format!("sha256:{n}")).unwrap();
            }
        }
        // Identical record in both homes is kept without a write.
        let same = rec(b'c');
        fs::write(fx.main.join(HOOK_TRUST).join(&same), "sha256:same").unwrap();
        fs::write(fx.shadow.join(HOOK_TRUST).join(&same), "sha256:same").unwrap();
        if !disjoint {
            let conflict = rec(b'd');
            fs::write(fx.main.join(HOOK_TRUST).join(&conflict), "sha256:main").unwrap();
            fs::write(fx.shadow.join(HOOK_TRUST).join(&conflict), "sha256:shadow").unwrap();
            return (fx, same, conflict);
        }
        (fx, same, String::new())
    }

    #[test]
    fn hook_trust_merge_clean() {
        let (fx, same, _) = hook_trust_fixture(true);
        let plan = fix(&fx.main, &fx.shadow, &fx.state, false).unwrap();
        assert!(plan.deferred.is_empty(), "{:?}", plan.deferred);
        assert!(
            plan.changes.iter().any(|c| matches!(
                c,
                Change::WriteFile { path, .. } if path.ends_with(rec(b'b'))
            )),
            "{:?}",
            plan.changes
        );
        apply_changes(&plan.changes).unwrap();
        // Union merged into main; shadow replaced by a link; backup outside homes.
        assert!(fs::read(fx.main.join(HOOK_TRUST).join(rec(b'b'))).is_ok());
        assert!(fs::read(fx.main.join(HOOK_TRUST).join(&same)).is_ok());
        assert_eq!(
            fs::read_link(fx.shadow.join(HOOK_TRUST)).unwrap(),
            fx.main.join(HOOK_TRUST)
        );
        let backups: Vec<PathBuf> = fs::read_dir(fx.state.join("backups"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(backups.len(), 1);
        assert!(backups[0].join(&same).exists());
        // Idempotent.
        let plan = fix(&fx.main, &fx.shadow, &fx.state, false).unwrap();
        assert!(plan.changes.is_empty() && plan.deferred.is_empty());
        assert!(check(&fx.main, &fx.shadow).unwrap().is_empty());
    }

    #[test]
    fn hook_trust_conflict_defers() {
        let (fx, _, conflict) = hook_trust_fixture(false);
        let plan = fix(&fx.main, &fx.shadow, &fx.state, true).unwrap();
        assert!(plan.changes.is_empty(), "{:?}", plan.changes);
        assert_eq!(plan.deferred.len(), 1);
        assert!(plan.deferred[0].contains(&conflict));
        assert!(fs::read(fx.shadow.join(HOOK_TRUST).join(&conflict)).is_ok());
    }

    #[test]
    fn hook_trust_unknown_file_defers() {
        let fx = fixture();
        sync(&fx);
        for dir in [&fx.main, &fx.shadow] {
            let link = dir.join(HOOK_TRUST);
            if fs::symlink_metadata(&link)
                .ok()
                .is_some_and(|m| m.file_type().is_symlink())
            {
                fs::remove_file(&link).unwrap();
            }
            fs::create_dir_all(&link).unwrap();
        }
        fs::write(fx.shadow.join(HOOK_TRUST).join("notes.txt"), "x").unwrap();
        let plan = fix(&fx.main, &fx.shadow, &fx.state, true).unwrap();
        assert!(plan.changes.is_empty());
        assert_eq!(plan.deferred.len(), 1);
    }

    #[test]
    fn writer_locks_active_defers_idle_replaces() {
        let fx = fixture();
        sync(&fx);
        let link = fx.shadow.join(WRITER_LOCKS);
        fs::remove_file(&link).unwrap();
        fs::create_dir_all(&link).unwrap();
        fs::write(link.join(ACTIVE_LOCK), "held").unwrap();
        let drift = check(&fx.main, &fx.shadow).unwrap();
        assert!(
            drift.iter().any(|d| matches!(d, Drift::Deferred { .. })),
            "{drift:?}"
        );
        let plan = fix(&fx.main, &fx.shadow, &fx.state, true).unwrap();
        assert!(plan.changes.is_empty());
        assert_eq!(plan.deferred.len(), 1);
        assert!(link.is_dir() && !link.is_symlink());
        // Idle lock dir (only coordination state) is backup-replaced.
        fs::remove_file(link.join(ACTIVE_LOCK)).unwrap();
        fs::write(link.join(".coordination.lock"), "x").unwrap();
        let plan = fix(&fx.main, &fx.shadow, &fx.state, false).unwrap();
        assert!(plan.deferred.is_empty());
        apply_changes(&plan.changes).unwrap();
        assert_eq!(fs::read_link(&link).unwrap(), fx.main.join(WRITER_LOCKS));
    }

    #[test]
    fn replaceable_lock_dir_replaced() {
        let fx = fixture();
        sync(&fx);
        let link = fx.shadow.join("mcp-oauth-locks");
        fs::remove_file(&link).unwrap();
        fs::create_dir_all(&link).unwrap();
        fs::write(link.join("tok"), "t").unwrap();
        let plan = fix(&fx.main, &fx.shadow, &fx.state, false).unwrap();
        assert!(plan.deferred.is_empty());
        apply_changes(&plan.changes).unwrap();
        assert_eq!(
            fs::read_link(&link).unwrap(),
            fx.main.join("mcp-oauth-locks")
        );
    }

    #[test]
    fn same_home_rejected() {
        let fx = fixture();
        let drift = check(&fx.main, &fx.main).unwrap();
        assert!(
            matches!(drift.as_slice(), [Drift::SameHome { .. }]),
            "{drift:?}"
        );
        assert!(plan_materialize(&fx.main, &fx.main, &fx.state).is_err());
        assert!(fix(&fx.main, &fx.main, &fx.state, true).is_err());
    }
}
