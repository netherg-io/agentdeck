//! Background sync: gather drift across all registry instances, classify each
//! item, and auto-apply only the additive ones (behind the `[sync]`
//! `auto_apply` flag, OFF by default).
//!
//! Drift is the divergence of settings, instructions, MCP and skills between
//! providers (Claude/Codex homes). T3 provider-instance envelopes
//! (enabled/config) are explicit T3 management (`t3 sync`), never Drift:
//! they are excluded here so they show no badge and send no notification.
//!
//! Classification (value-free summaries only, so logs/notifications leak
//! nothing):
//! - ADDITIVE: a missing symlink, a settings key missing on a secondary, a
//!   new main-home entry needing a shadow link. Safe to create.
//! - DECISION: changed values, wrong link targets, real entries where a link
//!   belongs, deletions/extras, private links. Needs the
//!   owner; never written by [`apply_additive`], even with auto-apply on.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::changes::Change;
use crate::{Paths, Registry, claude, codex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemKind {
    Additive,
    Decision,
}

#[derive(Debug, Clone, Serialize)]
pub struct SyncItem {
    /// Registry instance id.
    pub instance: String,
    pub harness: String,
    pub kind: ItemKind,
    /// Value-free summary (key/path names only, never secret values).
    pub detail: String,
}

impl SyncItem {
    fn new(instance: &str, harness: &str, kind: ItemKind, detail: String) -> Self {
        Self {
            instance: instance.to_string(),
            harness: harness.to_string(),
            kind,
            detail,
        }
    }
}

/// `[sync] auto_apply` from instances.toml. Missing table/key or a parse
/// failure means OFF (safe default).
pub fn auto_apply_enabled(paths: &Paths) -> bool {
    Registry::load(paths)
        .map(|r| sync_flag(&r))
        .unwrap_or(false)
}

fn sync_flag(reg: &Registry) -> bool {
    reg.extra
        .get("sync")
        .and_then(|v| v.as_table())
        .and_then(|t| t.get("auto_apply"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Flip `[sync] auto_apply` (other tables/keys survive the rewrite).
pub fn set_auto_apply(paths: &Paths, on: bool, dry_run: bool) -> anyhow::Result<Vec<Change>> {
    let before = Registry::load(paths).ok();
    let before_flag = before.as_ref().is_some_and(sync_flag);
    if before_flag == on {
        return Ok(vec![]);
    }
    if dry_run {
        // Describe without touching disk: the only change is the flag.
        return Ok(vec![Change::WriteFile {
            path: paths.registry_file(),
            contents: Vec::new(),
            old: None,
            backup: None,
        }]);
    }
    Registry::update(paths, |reg| {
        let mut table = toml::map::Map::new();
        table.insert("auto_apply".to_string(), toml::Value::Boolean(on));
        reg.extra
            .insert("sync".to_string(), toml::Value::Table(table));
        Ok(())
    })?;
    Ok(vec![Change::WriteFile {
        path: paths.registry_file(),
        contents: Vec::new(),
        old: None,
        backup: None,
    }])
}

/// Gather drift for every registry instance (Claude + Codex). `home` is the
/// real `$HOME` in production, a temp dir in tests; `t3_root` is accepted and
/// ignored: T3 provider-instance envelopes are explicit T3 management
/// (`t3 sync`), never Drift, so they contribute no badge or notification.
pub fn gather(paths: &Paths, home: &Path, t3_root: Option<&Path>) -> anyhow::Result<Vec<SyncItem>> {
    let _ = t3_root;
    let reg = Registry::load(paths)?;
    let mut out = Vec::new();
    let primary = home.join(".claude");
    let primary_json = home.join(".claude.json");
    for inst in &reg.instances {
        let dir = expand_tilde(&inst.home, home);
        match inst.harness {
            crate::Harness::Claude => {
                let drift = claude::check(&primary, &dir, &primary_json)?;
                out.extend(drift.iter().map(|d| classify_claude(&inst.id, d)));
            }
            crate::Harness::Codex => {
                let main_default = home.join(".codex");
                if dir == main_default {
                    continue; // primary home: nothing to link into itself
                }
                let drift = codex::check(&main_default, &dir)?;
                out.extend(drift.iter().map(|d| classify_codex(&inst.id, d)));
            }
            // Managed later (M7); background sync only watches Claude/Codex.
            crate::Harness::Opencode | crate::Harness::Agy => {}
        }
    }
    out.sort_by(|a, b| (&a.instance, &a.detail).cmp(&(&b.instance, &b.detail)));
    Ok(out)
}

fn short(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn classify_claude(id: &str, drift: &claude::Drift) -> SyncItem {
    match drift {
        claude::Drift::Symlink { link, actual, .. } => match actual {
            Some(_) => SyncItem::new(
                id,
                "claude",
                ItemKind::Decision,
                format!("symlink {} points elsewhere", short(link)),
            ),
            None if link.exists() => SyncItem::new(
                // A real entry where a link belongs: needs a decision.
                id,
                "claude",
                ItemKind::Decision,
                format!("real entry {} where a link belongs", short(link)),
            ),
            None => SyncItem::new(
                id,
                "claude",
                ItemKind::Additive,
                format!("missing symlink {}", short(link)),
            ),
        },
        claude::Drift::SettingsKey {
            key,
            primary: Some(_),
            secondary: None,
        } => SyncItem::new(
            id,
            "claude",
            ItemKind::Additive,
            format!("settings key {key:?} missing on secondary"),
        ),
        claude::Drift::SettingsKey { key, .. } => SyncItem::new(
            id,
            "claude",
            ItemKind::Decision,
            format!("settings key {key:?} differs"),
        ),
        claude::Drift::McpServers {
            primary: Some(_),
            secondary: None,
        } => SyncItem::new(
            id,
            "claude",
            ItemKind::Additive,
            "mcpServers missing on secondary".to_string(),
        ),
        claude::Drift::McpServers { .. } => SyncItem::new(
            id,
            "claude",
            ItemKind::Decision,
            "mcpServers differs".to_string(),
        ),
        claude::Drift::PrimaryConfigMissing { .. } => SyncItem::new(
            id,
            "claude",
            ItemKind::Decision,
            "primary .claude.json missing".to_string(),
        ),
    }
}

fn classify_codex(id: &str, drift: &codex::Drift) -> SyncItem {
    match drift {
        codex::Drift::MissingLink { link, .. } => SyncItem::new(
            id,
            "codex",
            ItemKind::Additive,
            format!("missing link {}", short(link)),
        ),
        codex::Drift::WrongTarget { link, .. } => SyncItem::new(
            id,
            "codex",
            ItemKind::Decision,
            format!("link {} points elsewhere", short(link)),
        ),
        codex::Drift::RealEntry { path, .. } => SyncItem::new(
            id,
            "codex",
            ItemKind::Decision,
            format!("real entry {} where a link belongs", short(path)),
        ),
        codex::Drift::ExtraEntry { path } => SyncItem::new(
            id,
            "codex",
            ItemKind::Decision,
            format!("shadow-only {}", short(path)),
        ),
        codex::Drift::PrivateLink { path, .. } => SyncItem::new(
            id,
            "codex",
            ItemKind::Decision,
            format!("private {} is a symlink", short(path)),
        ),
        codex::Drift::SameHome { .. } => {
            SyncItem::new(id, "codex", ItemKind::Decision, "same home".to_string())
        }
        codex::Drift::Deferred { path, .. } => SyncItem::new(
            id,
            "codex",
            ItemKind::Decision,
            format!("deferred {}", short(path)),
        ),
    }
}

/// Apply additive drift through the existing planners. Instances with any
/// DECISION item are skipped whole (their planners would touch decided
/// content). Each applied instance is re-checked right before planning, so a
/// concurrent harness edit cannot slip a decision through. Idempotent: a
/// clean tree applies nothing.
pub fn apply_additive(
    paths: &Paths,
    home: &Path,
    t3_root: Option<&Path>,
) -> anyhow::Result<Vec<Change>> {
    let _ = t3_root; // T3 envelopes are out of Drift scope (see `gather`).
    let reg = Registry::load(paths)?;
    let primary = home.join(".claude");
    let primary_json = home.join(".claude.json");
    let main_default = home.join(".codex");
    let mut applied = Vec::new();
    for inst in &reg.instances {
        let dir = expand_tilde(&inst.home, home);
        match inst.harness {
            crate::Harness::Claude => {
                // Re-check right before applying.
                if claude::check(&primary, &dir, &primary_json)?
                    .iter()
                    .any(|d| classify_claude(&inst.id, d).kind == ItemKind::Decision)
                {
                    continue;
                }
                let changes =
                    claude::plan_materialize(&primary, &dir, &paths.state_dir, &primary_json)?;
                crate::apply_changes(&changes)?;
                applied.extend(changes);
            }
            crate::Harness::Codex => {
                if dir == main_default {
                    continue;
                }
                if codex::check(&main_default, &dir)?
                    .iter()
                    .any(|d| classify_codex(&inst.id, d).kind == ItemKind::Decision)
                {
                    continue;
                }
                // Errors here (a real entry appeared after the re-check) fail
                // loudly instead of touching decided content.
                let changes = codex::plan_materialize(&main_default, &dir, &paths.state_dir)?;
                crate::apply_changes(&changes)?;
                applied.extend(changes);
            }
            crate::Harness::Opencode | crate::Harness::Agy => {}
        }
    }
    Ok(applied)
}

/// Instances holding DECISION items, sorted. Notification/log text source:
/// ids only, never values.
pub fn decision_instances(items: &[SyncItem]) -> Vec<String> {
    let mut ids: BTreeSet<&str> = BTreeSet::new();
    for i in items.iter().filter(|i| i.kind == ItemKind::Decision) {
        ids.insert(i.instance.as_str());
    }
    ids.into_iter().map(str::to_string).collect()
}

/// Additive items grouped per instance, for status output.
pub fn additive_by_instance(items: &[SyncItem]) -> BTreeMap<&str, Vec<&SyncItem>> {
    let mut map: BTreeMap<&str, Vec<&SyncItem>> = BTreeMap::new();
    for i in items.iter().filter(|i| i.kind == ItemKind::Additive) {
        map.entry(i.instance.as_str()).or_default().push(i);
    }
    map
}

pub(crate) fn expand_tilde(value: &str, home: &Path) -> PathBuf {
    if value == "~" {
        home.to_path_buf()
    } else if let Some(rest) = value.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(value)
    }
}

/// Hash of the current DECISION set; remembered in the state dir so the
/// daemon notifies once per distinct set, never repeating itself.
pub fn decision_hash(items: &[SyncItem]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    for i in items.iter().filter(|i| i.kind == ItemKind::Decision) {
        i.instance.hash(&mut h);
        i.harness.hash(&mut h);
        i.detail.hash(&mut h);
    }
    h.finish()
}

pub fn notified_file(state_dir: &Path) -> PathBuf {
    state_dir.join("sync").join("last-notified")
}

pub fn last_notified(state_dir: &Path) -> Option<u64> {
    fs::read_to_string(notified_file(state_dir))
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Remember the notified set. `None` clears it (drift resolved).
pub fn store_notified(state_dir: &Path, hash: Option<u64>) -> anyhow::Result<()> {
    let file = notified_file(state_dir);
    match hash {
        Some(h) => {
            if let Some(parent) = file.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(file, h.to_string())?;
        }
        None => {
            let _ = fs::remove_file(file);
        }
    }
    Ok(())
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
        dir: PathBuf,
        paths: Paths,
        home: PathBuf,
    }

    fn fixture() -> Fx {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-sync-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&dir);
        let home = dir.join("home");
        let dot_claude = home.join(".claude");
        for name in ["skills", "agents", "plugins"] {
            fs::create_dir_all(dot_claude.join(name)).unwrap();
        }
        fs::write(dot_claude.join("CLAUDE.md"), "me").unwrap();
        fs::write(dot_claude.join("settings.json"), "{\"theme\":\"dark\"}\n").unwrap();
        fs::write(
            home.join(".claude.json"),
            "{\"oauthAccount\":{\"accountUuid\":\"P\"},\"mcpServers\":{\"srv\":{\"command\":\"x\"}}}",
        )
        .unwrap();
        fs::create_dir_all(home.join(".codex")).unwrap();
        fs::write(home.join(".codex/config.toml"), "x = 1\n").unwrap();
        let paths = Paths::new(dir.join("config"), dir.join("state"));
        Registry::update(&paths, |r| {
            r.add(crate::registry::AddSpec {
                id: "cl-second".into(),
                harness: crate::Harness::Claude,
                display_name: None,
                home: home.join("claude-second").to_string_lossy().into_owned(),
                binary: None,
                no_t3: false,
            })?;
            r.add(crate::registry::AddSpec {
                id: "cx-second".into(),
                harness: crate::Harness::Codex,
                display_name: None,
                home: home.join("codex-second").to_string_lossy().into_owned(),
                binary: None,
                no_t3: false,
            })?;
            Ok(())
        })
        .unwrap();
        Fx {
            _g: Guard(dir.clone()),
            dir,
            paths,
            home,
        }
    }

    fn kinds(items: &[SyncItem], instance: &str) -> Vec<ItemKind> {
        items
            .iter()
            .filter(|i| i.instance == instance)
            .map(|i| i.kind)
            .collect()
    }

    #[test]
    fn auto_apply_defaults_off() {
        let fx = fixture();
        assert!(!auto_apply_enabled(&fx.paths));
        set_auto_apply(&fx.paths, true, false).unwrap();
        assert!(auto_apply_enabled(&fx.paths));
        set_auto_apply(&fx.paths, false, false).unwrap();
        assert!(!auto_apply_enabled(&fx.paths));
        // Dry-run flips nothing.
        set_auto_apply(&fx.paths, true, true).unwrap();
        assert!(!auto_apply_enabled(&fx.paths));
    }

    #[test]
    fn classification_table() {
        let fx = fixture();
        // Fresh secondaries: only additive drift (missing links/keys).
        let items = gather(&fx.paths, &fx.home, None).unwrap();
        assert!(!items.is_empty());
        assert!(
            items.iter().all(|i| i.kind == ItemKind::Additive),
            "{items:?}"
        );
        // Wrong link target -> decision.
        let cx = fx.home.join("codex-second");
        fs::create_dir_all(&cx).unwrap();
        std::os::unix::fs::symlink("/tmp/elsewhere", cx.join("config.toml")).unwrap();
        let items = gather(&fx.paths, &fx.home, None).unwrap();
        assert!(kinds(&items, "cx-second").contains(&ItemKind::Decision));
        // Real entry where a link belongs -> decision.
        let cl = fx.home.join("claude-second");
        fs::create_dir_all(cl.join("skills")).unwrap();
        let items = gather(&fx.paths, &fx.home, None).unwrap();
        assert!(kinds(&items, "cl-second").contains(&ItemKind::Decision));
        // Changed settings value -> decision.
        fs::remove_dir_all(cl.join("skills")).unwrap();
        apply_additive(&fx.paths, &fx.home, None).unwrap();
        fs::write(cl.join("settings.json"), "{\"theme\":\"light\"}\n").unwrap();
        let items = gather(&fx.paths, &fx.home, None).unwrap();
        assert!(
            items.iter().any(|i| i.instance == "cl-second"
                && i.kind == ItemKind::Decision
                && i.detail.contains("theme")),
            "{items:?}"
        );
        // Secondary-only key -> decision (a deletion/import question).
        fs::write(
            cl.join("settings.json"),
            "{\"theme\":\"dark\",\"mine\":1}\n",
        )
        .unwrap();
        let items = gather(&fx.paths, &fx.home, None).unwrap();
        assert!(
            items.iter().any(|i| i.instance == "cl-second"
                && i.kind == ItemKind::Decision
                && i.detail.contains("mine")),
            "{items:?}"
        );
        // Shadow-only extra + private symlink -> decision.
        fs::write(cx.join("notes.txt"), "scratch").unwrap();
        fs::write(cx.join("auth.json"), "login").unwrap();
        std::os::unix::fs::symlink(cx.join("auth.json"), cx.join("models_cache.json")).unwrap();
        let _ = fs::remove_file(cx.join("models_cache.json"));
        std::os::unix::fs::symlink("/tmp/elsewhere", cx.join("models_cache.json")).unwrap();
        let items = gather(&fx.paths, &fx.home, None).unwrap();
        let cx_kinds = kinds(&items, "cx-second");
        for want in [
            "shadow-only notes.txt",
            "private models_cache.json is a symlink",
        ] {
            assert!(
                items.iter().any(|i| i.instance == "cx-second"
                    && i.kind == ItemKind::Decision
                    && i.detail == want),
                "{items:?}"
            );
        }
        assert!(cx_kinds.contains(&ItemKind::Decision));
    }

    #[test]
    fn additive_apply_is_idempotent() {
        let fx = fixture();
        let first = apply_additive(&fx.paths, &fx.home, None).unwrap();
        assert!(!first.is_empty());
        let items = gather(&fx.paths, &fx.home, None).unwrap();
        // Only decision-kind drift may remain; here everything was additive.
        assert!(items.is_empty(), "{items:?}");
        let second = apply_additive(&fx.paths, &fx.home, None).unwrap();
        assert!(second.is_empty());
    }

    #[test]
    fn decision_items_never_written() {
        let fx = fixture();
        apply_additive(&fx.paths, &fx.home, None).unwrap();
        set_auto_apply(&fx.paths, true, false).unwrap();
        // Plant decision drift on both harnesses.
        let cl = fx.home.join("claude-second");
        fs::write(cl.join("settings.json"), "{\"theme\":\"light\"}\n").unwrap();
        let before_settings = fs::read(cl.join("settings.json")).unwrap();
        let cx = fx.home.join("codex-second");
        let link = cx.join("config.toml");
        fs::remove_file(&link).unwrap();
        fs::write(&link, "mine").unwrap();
        let before_shadow = fs::read(&link).unwrap();
        // Even with auto_apply on, apply_additive writes nothing decided.
        let applied = apply_additive(&fx.paths, &fx.home, None).unwrap();
        assert!(applied.is_empty(), "{applied:?}");
        assert_eq!(fs::read(cl.join("settings.json")).unwrap(), before_settings);
        assert_eq!(fs::read(&link).unwrap(), before_shadow);
        // Drift is still reported.
        let items = gather(&fx.paths, &fx.home, None).unwrap();
        assert!(items.iter().any(|i| i.kind == ItemKind::Decision));
    }

    #[test]
    fn t3_envelope_mismatch_is_not_drift() {
        // T3 provider-instance envelopes (enabled/config) are explicit T3
        // management (`t3 sync`), not Drift: neither missing instances, nor
        // an enabled-flag-only mismatch, may surface as sync items (and so
        // never as a Drift badge or background notification).
        let fx = fixture();
        let t3_root = fx.dir.join("t3");
        fs::create_dir_all(t3_root.join("userdata")).unwrap();
        let settings = t3_root.join("userdata/settings.json");
        // Managed instances absent: T3 sync would create them.
        fs::write(&settings, "{\"providerInstances\":{}}").unwrap();
        let entries = crate::t3::sync_t3(&fx.paths, &t3_root, true).unwrap();
        assert!(
            entries.iter().any(|e| e.action != "unchanged"),
            "{entries:?}"
        );
        let items = gather(&fx.paths, &fx.home, Some(&t3_root)).unwrap();
        assert!(items.iter().all(|i| i.harness != "t3"), "{items:?}");
        // Enabled-flag-only mismatch (the reported antigravity case).
        crate::t3::sync_t3(&fx.paths, &t3_root, false).unwrap();
        let raw = fs::read_to_string(&settings).unwrap();
        let mut v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        v["providerInstances"]["cl-second"]["enabled"] = serde_json::Value::Bool(false);
        fs::write(&settings, serde_json::to_string_pretty(&v).unwrap()).unwrap();
        let entries = crate::t3::sync_t3(&fx.paths, &t3_root, true).unwrap();
        assert!(entries.iter().any(|e| e.action == "updated"), "{entries:?}");
        let items = gather(&fx.paths, &fx.home, Some(&t3_root)).unwrap();
        assert!(items.iter().all(|i| i.harness != "t3"), "{items:?}");
        assert!(
            decision_instances(&items)
                .iter()
                .all(|id| !id.starts_with("t3:")),
            "{items:?}"
        );
        // Explicit T3 sync still sees (and writes) the mismatch.
        let before = fs::read(&settings).unwrap();
        apply_additive(&fx.paths, &fx.home, Some(&t3_root)).unwrap();
        assert_eq!(fs::read(&settings).unwrap(), before);
    }

    #[test]
    fn notified_hash_round_trip() {
        let fx = fixture();
        assert_eq!(last_notified(&fx.paths.state_dir), None);
        store_notified(&fx.paths.state_dir, Some(42)).unwrap();
        assert_eq!(last_notified(&fx.paths.state_dir), Some(42));
        store_notified(&fx.paths.state_dir, None).unwrap();
        assert_eq!(last_notified(&fx.paths.state_dir), None);
    }

    #[test]
    fn no_values_in_details() {
        let fx = fixture();
        apply_additive(&fx.paths, &fx.home, None).unwrap();
        // Leak a would-be secret into drift and check the summaries.
        let cl = fx.home.join("claude-second");
        fs::write(cl.join("settings.json"), "{\"theme\":\"s3cr3t-value\"}\n").unwrap();
        let items = gather(&fx.paths, &fx.home, None).unwrap();
        let text = items
            .iter()
            .map(|i| format!("{} {}", i.instance, i.detail))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!text.contains("s3cr3t-value"), "{text}");
        assert!(!text.contains("\"P\""), "{text}");
        let _ = apply_changes(&[]);
    }
}
