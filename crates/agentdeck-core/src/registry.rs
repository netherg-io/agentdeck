//! `instances.toml` registry: schema, flocked load/update, atomic writes, backups.
//!
//! ```toml
//! version = 1
//! [[instance]]
//! id = "claude-main"
//! harness = "claude"
//! display_name = "Claude main"
//! home = "~/.claude"
//! enabled = true
//! t3 = true
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::changes::{Change, plan_write_file};
use crate::paths::Paths;
use crate::pool::PoolEntry;
use crate::usage::{UsageEntry, validate_all};

const CURRENT_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Harness {
    Claude,
    Codex,
    Opencode,
    Agy,
}

impl Harness {
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        match s {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "opencode" => Ok(Self::Opencode),
            "agy" => Ok(Self::Agy),
            other => {
                anyhow::bail!("bad harness {other:?}: expected claude, codex, opencode or agy")
            }
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
            Self::Agy => "agy",
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Instance {
    pub id: String,
    pub harness: Harness,
    #[serde(default)]
    pub display_name: String,
    pub home: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub t3: bool,
    /// Maximum live T3 runs across all models and coordinators; absent means unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_parallel: Option<u32>,
    /// Unknown keys are preserved verbatim so a rewrite never loses them.
    #[serde(flatten, default)]
    pub extra: toml::map::Map<String, toml::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Registry {
    pub version: u32,
    #[serde(rename = "instance", default)]
    pub instances: Vec<Instance>,
    #[serde(rename = "pool", default)]
    pub pool: Vec<PoolEntry>,
    #[serde(default)]
    pub usage: Vec<UsageEntry>,
    /// Built-in plugin flags (`agent-process = true`). Absent table or key
    /// means disabled, so files written before plugins stay valid.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub plugins: BTreeMap<String, bool>,
    /// Unknown top-level tables/keys survive rewrites.
    #[serde(flatten)]
    pub extra: toml::map::Map<String, toml::Value>,
}

impl Registry {
    pub fn empty() -> Self {
        Self {
            version: CURRENT_VERSION,
            instances: Vec::new(),
            pool: Vec::new(),
            usage: Vec::new(),
            plugins: Default::default(),
            extra: Default::default(),
        }
    }

    pub fn get(&self, id: &str) -> Option<&Instance> {
        self.instances.iter().find(|i| i.id == id)
    }

    pub fn from_toml(text: &str) -> anyhow::Result<Self> {
        let mut reg: Self = toml::from_str(text)
            .map_err(|e| anyhow::anyhow!("cannot parse instances.toml: {e}"))?;
        reg.validate()?;
        Ok(reg)
    }

    pub(crate) fn validate(&mut self) -> anyhow::Result<()> {
        if self.version != CURRENT_VERSION {
            anyhow::bail!(
                "unsupported instances.toml version {}: expected {CURRENT_VERSION}",
                self.version
            );
        }
        let mut seen = BTreeSet::new();
        for inst in &mut self.instances {
            check_id(&inst.id)?;
            if !seen.insert(inst.id.clone()) {
                anyhow::bail!("duplicate instance id {:?}", inst.id);
            }
            if inst.max_parallel == Some(0) {
                anyhow::bail!("instance {:?}: max_parallel must be positive", inst.id);
            }
            if inst.home.is_empty() {
                anyhow::bail!("instance {:?} has an empty home", inst.id);
            }
            if inst.display_name.is_empty() {
                inst.display_name.clone_from(&inst.id);
            }
        }
        for e in &self.pool {
            if self.get(&e.instance).is_none() {
                anyhow::bail!(
                    "pool entry {:?} refers to unknown instance {:?}",
                    e.model,
                    e.instance
                );
            }
            if e.model.is_empty() {
                anyhow::bail!(
                    "pool entry for instance {:?} has an empty model",
                    e.instance
                );
            }
        }
        for (i, a) in self.pool.iter().enumerate() {
            if self.pool[i + 1..].iter().any(|b| a.key_eq(b)) {
                anyhow::bail!("duplicate pool entry {:?}", a.describe());
            }
        }
        validate_all(&mut self.usage, &self.instances)?;
        Ok(())
    }

    pub fn render(&self) -> anyhow::Result<String> {
        Ok(toml::to_string_pretty(self)?)
    }

    /// Shared-locked read. A missing file is an empty registry.
    pub fn load(paths: &Paths) -> anyhow::Result<Self> {
        let _lock = shared_lock(paths)?;
        match fs::read_to_string(paths.registry_file()) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::empty()),
            Err(e) => Err(e.into()),
            Ok(text) if text.trim().is_empty() => Ok(Self::empty()),
            Ok(text) => Self::from_toml(&text),
        }
    }

    /// Exclusive-locked read-modify-write: re-reads inside the lock, validates,
    /// backs up the old file, and writes atomically (tmp + fsync + rename, 0600).
    pub fn update(
        paths: &Paths,
        f: impl FnOnce(&mut Self) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let _lock = exclusive_lock(paths)?;
        let file = paths.registry_file();
        let old = match fs::read(&file) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
            Ok(bytes) => Some(bytes),
        };
        let mut reg = match &old {
            None => Self::empty(),
            Some(bytes) => {
                let text = std::str::from_utf8(bytes)
                    .map_err(|_| anyhow::anyhow!("instances.toml is not valid UTF-8"))?;
                if text.trim().is_empty() {
                    Self::empty()
                } else {
                    Self::from_toml(text)?
                }
            }
        };
        f(&mut reg)?;
        reg.validate()?;
        let new_text = reg.render()?;
        if old.as_deref() == Some(new_text.as_bytes()) {
            return Ok(());
        }
        if old.is_some() {
            backup(paths)?;
        }
        atomic_write(&file, new_text.as_bytes())?;
        Ok(())
    }

    pub fn add(&mut self, spec: AddSpec) -> anyhow::Result<()> {
        check_id(&spec.id)?;
        if self.get(&spec.id).is_some() {
            anyhow::bail!("duplicate instance id {:?}", spec.id);
        }
        if spec.home.is_empty() {
            anyhow::bail!("home must not be empty");
        }
        self.instances.push(Instance {
            display_name: spec.display_name.unwrap_or_else(|| spec.id.clone()),
            id: spec.id,
            harness: spec.harness,
            home: spec.home,
            binary: spec.binary,
            enabled: true,
            t3: !spec.no_t3,
            max_parallel: None,
            extra: Default::default(),
        });
        self.validate()
    }

    /// Removes the instance and its pool entries (they cannot route without
    /// it); usage entries refuse, since they hold account configuration.
    pub fn remove(&mut self, id: &str) -> anyhow::Result<Instance> {
        let used_by: Vec<String> = self
            .usage
            .iter()
            .filter(|u| u.instance.as_deref() == Some(id))
            .map(|u| u.id.clone())
            .collect();
        if !used_by.is_empty() {
            anyhow::bail!(
                "cannot remove instance {id:?}: still used by usage entries:\n  {}",
                used_by.join("\n  ")
            );
        }
        let pos = self
            .instances
            .iter()
            .position(|i| i.id == id)
            .ok_or_else(|| anyhow::anyhow!("unknown instance id {id:?}"))?;
        self.pool.retain(|e| e.instance != id);
        Ok(self.instances.remove(pos))
    }

    pub fn rename(&mut self, id: &str, name: &str) -> anyhow::Result<()> {
        let name = name.trim();
        if name.is_empty() || name.chars().any(char::is_control) {
            anyhow::bail!("display name must be non-empty and on one line");
        }
        let inst = self
            .instances
            .iter_mut()
            .find(|i| i.id == id)
            .ok_or_else(|| anyhow::anyhow!("unknown instance id {id:?}"))?;
        let old = std::mem::replace(&mut inst.display_name, name.to_string());
        for usage in &mut self.usage {
            if usage.instance.as_deref() == Some(id)
                && (usage.display == old || usage.display == id)
            {
                usage.display = name.to_string();
            }
        }
        Ok(())
    }

    pub fn set_max_parallel(&mut self, id: &str, limit: Option<u32>) -> anyhow::Result<()> {
        if limit == Some(0) {
            anyhow::bail!("max_parallel must be positive");
        }
        self.instances
            .iter_mut()
            .find(|i| i.id == id)
            .ok_or_else(|| anyhow::anyhow!("unknown instance id {id:?}"))?
            .max_parallel = limit;
        Ok(())
    }

    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> anyhow::Result<()> {
        let inst = self
            .instances
            .iter_mut()
            .find(|i| i.id == id)
            .ok_or_else(|| anyhow::anyhow!("unknown instance id {id:?}"))?;
        inst.enabled = enabled;
        Ok(())
    }
}

pub(crate) fn check_id(id: &str) -> anyhow::Result<()> {
    // T3 slug rule: instance ids double as T3 providerInstance ids
    // (e.g. `claudeAgent`, `codex_second`), so uppercase is allowed.
    if id.is_empty() || id.len() > 64 {
        anyhow::bail!("bad instance id {id:?}: must be 1-64 chars of [A-Za-z0-9_-]");
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        anyhow::bail!("bad instance id {id:?}: must match [A-Za-z0-9_-]+");
    }
    Ok(())
}

fn lock_file(paths: &Paths) -> anyhow::Result<File> {
    fs::create_dir_all(&paths.config_dir)?;
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(paths.lock_file())?)
}

fn shared_lock(paths: &Paths) -> anyhow::Result<File> {
    let file = lock_file(paths)?;
    file.lock_shared()?;
    Ok(file)
}

fn exclusive_lock(paths: &Paths) -> anyhow::Result<File> {
    let file = lock_file(paths)?;
    file.lock()?;
    Ok(file)
}

fn utc_stamp() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}.{:09}", now.as_secs(), now.subsec_nanos())
}

fn backup(paths: &Paths) -> anyhow::Result<PathBuf> {
    let dir = paths.backup_dir();
    fs::create_dir_all(&dir)?;
    // shortcut: no rotation, one file per overwrite; upgrade when backups pile up.
    let dst = dir.join(format!("instances.toml.{}", utc_stamp()));
    fs::copy(paths.registry_file(), &dst)?;
    Ok(dst)
}

fn atomic_write(path: &PathBuf, contents: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, contents)?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    File::open(&tmp)?.sync_all()?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Render the registry after applying `mutate` and return the resulting
/// `WriteFile` change (or `[]` when nothing would change), without touching disk.
fn preview(
    paths: &Paths,
    mutate: impl FnOnce(&mut Registry) -> anyhow::Result<()>,
) -> anyhow::Result<(Vec<Change>, Registry)> {
    let mut reg = Registry::load(paths)?;
    mutate(&mut reg)?;
    reg.validate()?;
    let new_text = reg.render()?;
    let old = fs::read(paths.registry_file()).ok();
    let changes = plan_write_file(paths.registry_file(), old.as_deref(), new_text.as_bytes())
        .map(|c| vec![c])
        .unwrap_or_default();
    Ok((changes, reg))
}

#[derive(Debug, Clone)]
pub struct AddSpec {
    pub id: String,
    pub harness: Harness,
    pub display_name: Option<String>,
    pub home: String,
    pub binary: Option<String>,
    pub no_t3: bool,
}

pub fn list_instances(paths: &Paths) -> anyhow::Result<Vec<Instance>> {
    Ok(Registry::load(paths)?.instances)
}

pub fn instances_json(paths: &Paths) -> anyhow::Result<String> {
    Ok(serde_json::to_string_pretty(&list_instances(paths)?)?)
}

pub fn add_instance(paths: &Paths, spec: AddSpec, dry_run: bool) -> anyhow::Result<Vec<Change>> {
    let (changes, _) = preview(paths, |reg| {
        reg.add(spec.clone())?;
        Ok(())
    })?;
    if changes.is_empty() {
        anyhow::bail!("instance {:?} already exists", spec.id);
    }
    if !dry_run {
        Registry::update(paths, |reg| {
            reg.add(spec)?;
            Ok(())
        })?;
    }
    Ok(changes)
}

pub fn remove_instance(
    paths: &Paths,
    id: &str,
    dry_run: bool,
) -> anyhow::Result<(Vec<Change>, String)> {
    let home = Registry::load(paths)?
        .get(id)
        .map(|i| i.home.clone())
        .ok_or_else(|| anyhow::anyhow!("unknown instance id {id:?}"))?;
    let (changes, _) = preview(paths, |reg| {
        reg.remove(id)?;
        Ok(())
    })?;
    if changes.is_empty() {
        anyhow::bail!("unknown instance id {id:?}");
    }
    if !dry_run {
        Registry::update(paths, |reg| {
            reg.remove(id)?;
            Ok(())
        })?;
    }
    Ok((changes, home))
}

pub fn rename_instance(
    paths: &Paths,
    id: &str,
    name: &str,
    dry_run: bool,
) -> anyhow::Result<Vec<Change>> {
    let (changes, _) = preview(paths, |reg| reg.rename(id, name))?;
    if !dry_run {
        Registry::update(paths, |reg| reg.rename(id, name))?;
    }
    Ok(changes)
}

pub fn set_max_parallel(
    paths: &Paths,
    id: &str,
    limit: Option<u32>,
    dry_run: bool,
) -> anyhow::Result<Vec<Change>> {
    let (changes, _) = preview(paths, |reg| reg.set_max_parallel(id, limit))?;
    if !dry_run {
        Registry::update(paths, |reg| reg.set_max_parallel(id, limit))?;
    }
    Ok(changes)
}

pub fn set_enabled(
    paths: &Paths,
    id: &str,
    enabled: bool,
    dry_run: bool,
) -> anyhow::Result<Vec<Change>> {
    let (changes, _) = preview(paths, |reg| {
        reg.set_enabled(id, enabled)?;
        Ok(())
    })?;
    if !dry_run {
        if changes.is_empty() {
            // Still validate the id exists so typos fail loudly.
            Registry::load(paths)?
                .get(id)
                .ok_or_else(|| anyhow::anyhow!("unknown instance id {id:?}"))?;
            return Ok(vec![]);
        }
        Registry::update(paths, |reg| {
            reg.set_enabled(id, enabled)?;
            Ok(())
        })?;
    } else if Registry::load(paths)?.get(id).is_none() {
        anyhow::bail!("unknown instance id {id:?}");
    }
    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);

    pub(crate) struct Guard(pub PathBuf);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    pub(crate) fn test_paths(tag: &str) -> (Paths, Guard) {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-{}-{}-{}",
            tag,
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&dir);
        let paths = Paths::new(dir.join("config"), dir.join("state"));
        (paths, Guard(dir))
    }

    fn spec(id: &str) -> AddSpec {
        AddSpec {
            id: id.to_string(),
            harness: Harness::Claude,
            display_name: None,
            home: format!("/fake/{id}"),
            binary: None,
            no_t3: false,
        }
    }

    #[test]
    fn parallel_limit_roundtrip_dry_run_clear_and_validation() {
        let (paths, _g) = test_paths("parallel");
        add_instance(&paths, spec("a-main"), false).unwrap();
        assert_eq!(
            Registry::load(&paths)
                .unwrap()
                .get("a-main")
                .unwrap()
                .max_parallel,
            None
        );
        set_max_parallel(&paths, "a-main", Some(2), true).unwrap();
        assert_eq!(
            Registry::load(&paths)
                .unwrap()
                .get("a-main")
                .unwrap()
                .max_parallel,
            None
        );
        set_max_parallel(&paths, "a-main", Some(2), false).unwrap();
        let reg = Registry::load(&paths).unwrap();
        assert_eq!(reg.get("a-main").unwrap().max_parallel, Some(2));
        let exported = crate::router::router_json_value(&reg).unwrap();
        assert_eq!(exported["instanceLimits"]["a-main"], 2);
        assert!(set_max_parallel(&paths, "a-main", Some(0), false).is_err());
        assert!(set_max_parallel(&paths, "missing", Some(1), false).is_err());
        assert!(
            Registry::from_toml(
                &reg.render()
                    .unwrap()
                    .replace("max_parallel = 2", "max_parallel = 0")
            )
            .is_err()
        );
        set_max_parallel(&paths, "a-main", None, false).unwrap();
        let cleared = Registry::load(&paths).unwrap();
        assert_eq!(cleared.get("a-main").unwrap().max_parallel, None);
        let cleared_json = crate::router::router_json_value(&cleared).unwrap();
        assert!(cleared_json.get("instanceLimits").is_none());
        assert_ne!(cleared_json["version"], exported["version"]);
    }

    #[test]
    fn round_trip_with_defaults() {
        let (paths, _g) = test_paths("roundtrip");
        Registry::update(&paths, |r| {
            r.add(spec("a-main"))?;
            r.add(AddSpec {
                binary: Some("/usr/bin/codex".into()),
                no_t3: true,
                harness: Harness::Codex,
                ..spec("b-second")
            })?;
            Ok(())
        })
        .unwrap();
        let reg = Registry::load(&paths).unwrap();
        assert_eq!(reg.instances.len(), 2);
        let a = reg.get("a-main").unwrap();
        assert_eq!(a.display_name, "a-main");
        assert!(a.enabled && a.t3 && a.binary.is_none());
        let b = reg.get("b-second").unwrap();
        assert!(!b.t3);
        assert_eq!(reg.version, 1);
    }

    #[test]
    fn validation_errors() {
        let (paths, _g) = test_paths("validation");
        Registry::update(&paths, |r| r.add(spec("dup"))).unwrap();
        assert!(Registry::update(&paths, |r| r.add(spec("dup"))).is_err());
        assert!(Registry::update(&paths, |r| r.add(spec("Bad ID!"))).is_err());
        // T3 slug rule: camelCase ids like T3's own `claudeAgent` are legal.
        Registry::update(&paths, |r| r.add(spec("claudeAgent"))).unwrap();
        Registry::update(&paths, |r| r.add(spec("codex_second"))).unwrap();
        assert!(
            Registry::from_toml(
                "version = 1\n[[instance]]\nid = \"x\"\nharness = \"bogus\"\nhome = \"h\"\n"
            )
            .is_err()
        );
        assert!(Registry::from_toml("version = 999\n").is_err());
    }

    #[test]
    fn unknown_keys_survive_rewrite() {
        let (paths, _g) = test_paths("unknown");
        fs::create_dir_all(&paths.config_dir).unwrap();
        fs::write(
            paths.registry_file(),
            "version = 1\nmystery_top = 42\n[pond]\nfoo = \"bar\"\n\
             [[instance]]\nid = \"k\"\nharness = \"agy\"\nhome = \"h\"\nnick = \"x\"\n",
        )
        .unwrap();
        Registry::update(&paths, |r| {
            r.set_enabled("k", false)?;
            Ok(())
        })
        .unwrap();
        let raw = fs::read_to_string(paths.registry_file()).unwrap();
        assert!(raw.contains("mystery_top"), "{raw}");
        assert!(raw.contains("[pond]"), "{raw}");
        assert!(raw.contains("nick"), "{raw}");
        assert!(raw.contains("enabled = false"), "{raw}");
    }

    #[test]
    fn backup_on_overwrite() {
        let (paths, _g) = test_paths("backup");
        Registry::update(&paths, |r| r.add(spec("one"))).unwrap();
        assert!(!paths.backup_dir().exists() || backup_count(&paths) == 0);
        Registry::update(&paths, |r| r.add(spec("two"))).unwrap();
        assert_eq!(backup_count(&paths), 1);
    }

    fn backup_count(paths: &Paths) -> usize {
        fs::read_dir(paths.backup_dir())
            .map(|d| d.count())
            .unwrap_or(0)
    }

    #[test]
    fn dry_run_changes_nothing() {
        let (paths, _g) = test_paths("dryrun");
        let changes = add_instance(&paths, spec("ghost"), true).unwrap();
        assert_eq!(changes.len(), 1);
        assert!(!paths.registry_file().exists());
        add_instance(&paths, spec("real"), false).unwrap();
        let (changes, _) = remove_instance(&paths, "real", true).unwrap();
        assert_eq!(changes.len(), 1);
        assert!(Registry::load(&paths).unwrap().get("real").is_some());
    }

    #[test]
    fn concurrent_updates_all_land() {
        let (paths, _g) = test_paths("concurrent");
        std::thread::scope(|s| {
            for t in 0..8 {
                let p = Paths::new(paths.config_dir.clone(), paths.state_dir.clone());
                s.spawn(move || {
                    Registry::update(&p, |r| {
                        r.add(spec(&format!("i-{t}")))?;
                        Ok(())
                    })
                    .unwrap();
                });
            }
        });
        let reg = Registry::load(&paths).unwrap();
        assert_eq!(reg.instances.len(), 8);
    }

    #[test]
    fn remove_and_set() {
        let (paths, _g) = test_paths("removeset");
        add_instance(&paths, spec("gone"), false).unwrap();
        set_enabled(&paths, "gone", false, false).unwrap();
        assert!(!Registry::load(&paths).unwrap().get("gone").unwrap().enabled);
        let (changes, home) = remove_instance(&paths, "gone", false).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(home, "/fake/gone");
        assert!(remove_instance(&paths, "gone", false).is_err());
        assert!(set_enabled(&paths, "gone", true, false).is_err());
    }
}
