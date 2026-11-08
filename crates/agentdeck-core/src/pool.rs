//! `[[pool]]` model pool: `(instance, model, options)` entries with an
//! offline warning when the model is absent from the T3 instance cache.

use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::changes::{Change, plan_write_file};
use crate::paths::Paths;
use crate::registry::Registry;

fn default_true() -> bool {
    true
}

/// Pool option value: only string|bool are accepted.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PoolValue {
    Bool(bool),
    String(String),
}

impl std::fmt::Display for PoolValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bool(b) => write!(f, "{b}"),
            Self::String(s) => write!(f, "{s}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolEntry {
    pub instance: String,
    pub model: String,
    #[serde(default)]
    pub options: BTreeMap<String, PoolValue>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl PoolEntry {
    pub fn key_eq(&self, other: &Self) -> bool {
        self.instance == other.instance
            && self.model == other.model
            && self.options == other.options
    }

    pub fn describe(&self) -> String {
        let mut s = format!("{} {}", self.instance, self.model);
        for (k, v) in &self.options {
            s.push_str(&format!(" {k}={v}"));
        }
        s.push_str(if self.enabled {
            " enabled=true"
        } else {
            " enabled=false"
        });
        s
    }
}

/// Parse one `--option k=v` arg; "true"/"false" become bool, else string.
pub fn parse_option(arg: &str) -> anyhow::Result<(String, PoolValue)> {
    let (k, v) = arg
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("bad option {arg:?}: expected k=v"))?;
    if k.is_empty() {
        anyhow::bail!("bad option {arg:?}: empty key");
    }
    let value = match v {
        "true" => PoolValue::Bool(true),
        "false" => PoolValue::Bool(false),
        _ => PoolValue::String(v.to_string()),
    };
    Ok((k.to_string(), value))
}

pub fn parse_options(args: &[String]) -> anyhow::Result<BTreeMap<String, PoolValue>> {
    let mut map = BTreeMap::new();
    for a in args {
        let (k, v) = parse_option(a)?;
        map.insert(k, v);
    }
    Ok(map)
}

/// T3 home for cache reads only: `$AGENTDECK_T3_DIR` or `~/.t3`.
pub fn t3_dir() -> Option<PathBuf> {
    if let Some(d) = env::var_os("AGENTDECK_T3_DIR") {
        return Some(PathBuf::from(d));
    }
    env::var_os("HOME").map(|h| PathBuf::from(h).join(".t3"))
}

/// Warn (not fail) when `<t3dir>/caches/<instance>.json` exists and lists no
/// such model slug. Missing/unreadable files mean "no data": no warning.
pub fn check_model(t3dir: Option<&Path>, instance: &str, model: &str) -> Option<String> {
    match t3dir {
        Some(d) => check_model_in(d, instance, model),
        None => check_model_in(&t3_owned()?, instance, model),
    }
}

fn t3_owned() -> Option<PathBuf> {
    t3_dir()
}

fn check_model_in(t3dir: &Path, instance: &str, model: &str) -> Option<String> {
    let raw = std::fs::read(t3dir.join("caches").join(format!("{instance}.json"))).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    let models = v.get("models")?.as_array()?;
    let slugs: Vec<&str> = models
        .iter()
        .filter_map(|m| m.get("slug")?.as_str())
        .collect();
    if slugs.contains(&model) {
        return None;
    }
    let checked = v
        .get("checkedAt")
        .and_then(|c| c.as_str())
        .unwrap_or("unknown time");
    Some(format!(
        "model {model:?} not in T3 cache for instance {instance:?} (checkedAt {checked})"
    ))
}

impl Registry {
    pub fn pool(&self) -> &[PoolEntry] {
        &self.pool
    }

    pub fn add_pool(&mut self, entry: PoolEntry) -> anyhow::Result<Option<String>> {
        if self.get(&entry.instance).is_none() {
            anyhow::bail!("unknown instance id {:?}", entry.instance);
        }
        if entry.model.is_empty() {
            anyhow::bail!("model must not be empty");
        }
        if self.pool.iter().any(|e| e.key_eq(&entry)) {
            anyhow::bail!("duplicate pool entry {:?}", entry.describe());
        }
        let warn = check_model(None, &entry.instance, &entry.model);
        self.pool.push(entry);
        self.validate()?;
        Ok(warn)
    }

    pub fn remove_pool(
        &mut self,
        instance: &str,
        model: &str,
        options: &BTreeMap<String, PoolValue>,
    ) -> anyhow::Result<PoolEntry> {
        let pos = self
            .pool
            .iter()
            .position(|e| e.instance == instance && e.model == model && &e.options == options)
            .ok_or_else(|| anyhow::anyhow!("unknown pool entry {instance:?} {model:?}"))?;
        Ok(self.pool.remove(pos))
    }

    pub fn set_pool_enabled(
        &mut self,
        instance: &str,
        model: &str,
        options: &BTreeMap<String, PoolValue>,
        enabled: bool,
    ) -> anyhow::Result<()> {
        let e = self
            .pool
            .iter_mut()
            .find(|e| e.instance == instance && e.model == model && &e.options == options)
            .ok_or_else(|| anyhow::anyhow!("unknown pool entry {instance:?} {model:?}"))?;
        e.enabled = enabled;
        Ok(())
    }
}

/// Available model ids from the instance catalog; missing caches leave saved entries usable.
pub fn cached_models(t3_root: Option<&Path>, instance: &str) -> anyhow::Result<Vec<String>> {
    Ok(catalog_models(t3_root, instance)?
        .into_iter()
        .map(|(slug, _)| slug)
        .collect())
}

/// Catalog models T3 marks `isLegacy` (superseded versions).
pub fn legacy_models(t3_root: Option<&Path>, instance: &str) -> anyhow::Result<Vec<String>> {
    Ok(catalog_models(t3_root, instance)?
        .into_iter()
        .filter(|(_, legacy)| *legacy)
        .map(|(slug, _)| slug)
        .collect())
}

/// (slug, isLegacy) from the instance's T3 model cache, sorted and deduplicated.
fn catalog_models(t3_root: Option<&Path>, instance: &str) -> anyhow::Result<Vec<(String, bool)>> {
    let Some(root) = t3_root else {
        return Ok(Vec::new());
    };
    let file = root.join("caches").join(format!("{instance}.json"));
    let raw = match std::fs::read(&file) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(anyhow::anyhow!("{}: {e}", file.display())),
    };
    let catalog: serde_json::Value =
        serde_json::from_slice(&raw).map_err(|e| anyhow::anyhow!("{}: {e}", file.display()))?;
    let models = catalog
        .get("models")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("{}: missing model catalog", file.display()))?;
    let mut out: Vec<(String, bool)> = models
        .iter()
        .filter_map(|m| {
            let slug = m
                .get("slug")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())?;
            let legacy = m.get("isLegacy").and_then(|v| v.as_bool()).unwrap_or(false);
            Some((slug.to_owned(), legacy))
        })
        .collect();
    out.sort();
    out.dedup_by(|a, b| a.0 == b.0);
    Ok(out)
}

/// The whole catalog is visible; only explicit pool selections can be routed.
pub fn catalog_pool(reg: &Registry, t3_root: Option<&Path>) -> anyhow::Result<Vec<PoolEntry>> {
    let mut entries = reg.pool.clone();
    for instance in &reg.instances {
        for model in cached_models(t3_root, &instance.id)? {
            if !entries
                .iter()
                .any(|e| e.instance == instance.id && e.model == model && e.options.is_empty())
            {
                entries.push(PoolEntry {
                    instance: instance.id.clone(),
                    model,
                    options: BTreeMap::new(),
                    enabled: false,
                });
            }
        }
    }
    entries.sort_by(|a, b| {
        (&a.instance, &a.model, &a.options).cmp(&(&b.instance, &b.model, &b.options))
    });
    Ok(entries)
}

pub fn list_pool(paths: &Paths) -> anyhow::Result<Vec<PoolEntry>> {
    catalog_pool(&Registry::load(paths)?, t3_dir().as_deref())
}

pub fn pool_json(paths: &Paths) -> anyhow::Result<String> {
    Ok(serde_json::to_string_pretty(&list_pool(paths)?)?)
}

fn preview_pool(
    paths: &Paths,
    mutate: impl FnOnce(&mut Registry) -> anyhow::Result<Option<String>>,
) -> anyhow::Result<(Vec<Change>, Option<String>)> {
    let mut reg = Registry::load(paths)?;
    let warn = mutate(&mut reg)?;
    reg.validate()?;
    let new_text = reg.render()?;
    let old = std::fs::read(paths.registry_file()).ok();
    let changes = plan_write_file(paths.registry_file(), old.as_deref(), new_text.as_bytes())
        .map(|c| vec![c])
        .unwrap_or_default();
    Ok((changes, warn))
}

pub fn add_pool_entry(
    paths: &Paths,
    entry: PoolEntry,
    dry_run: bool,
) -> anyhow::Result<(Vec<Change>, Option<String>)> {
    let (changes, warn) = preview_pool(paths, |reg| reg.add_pool(entry.clone()))?;
    if changes.is_empty() {
        anyhow::bail!("pool entry {:?} already exists", entry.describe());
    }
    if !dry_run {
        Registry::update(paths, |reg| {
            reg.add_pool(entry)?;
            Ok(())
        })?;
    }
    Ok((changes, warn))
}

pub fn remove_pool_entry(
    paths: &Paths,
    instance: &str,
    model: &str,
    options: &BTreeMap<String, PoolValue>,
    dry_run: bool,
) -> anyhow::Result<Vec<Change>> {
    Registry::load(paths)?
        .pool
        .iter()
        .find(|e| e.instance == instance && e.model == model && &e.options == options)
        .ok_or_else(|| anyhow::anyhow!("unknown pool entry {instance:?} {model:?}"))?;
    let (changes, _) = preview_pool(paths, |reg| {
        reg.remove_pool(instance, model, options)?;
        Ok(None)
    })?;
    if !dry_run {
        Registry::update(paths, |reg| {
            reg.remove_pool(instance, model, options)?;
            Ok(())
        })?;
    }
    Ok(changes)
}

pub fn set_pool_enabled(
    paths: &Paths,
    instance: &str,
    model: &str,
    options: &BTreeMap<String, PoolValue>,
    enabled: bool,
    dry_run: bool,
) -> anyhow::Result<Vec<Change>> {
    let reg = Registry::load(paths)?;
    if !reg
        .pool
        .iter()
        .any(|e| e.instance == instance && e.model == model && &e.options == options)
    {
        let mut entry = catalog_pool(&reg, t3_dir().as_deref())?
            .into_iter()
            .find(|e| e.instance == instance && e.model == model && &e.options == options)
            .ok_or_else(|| anyhow::anyhow!("unknown pool entry {instance:?} {model:?}"))?;
        if !enabled {
            return Ok(Vec::new());
        }
        entry.enabled = true;
        return add_pool_entry(paths, entry, dry_run).map(|(changes, _)| changes);
    }
    let (changes, _) = preview_pool(paths, |reg| {
        reg.set_pool_enabled(instance, model, options, enabled)?;
        Ok(None)
    })?;
    if !dry_run {
        if changes.is_empty() {
            Registry::load(paths)?
                .pool
                .iter()
                .find(|e| e.instance == instance && e.model == model && &e.options == options)
                .ok_or_else(|| anyhow::anyhow!("unknown pool entry {instance:?} {model:?}"))?;
            return Ok(vec![]);
        }
        Registry::update(paths, |reg| {
            reg.set_pool_enabled(instance, model, options, enabled)?;
            Ok(())
        })?;
    }
    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{AddSpec, Harness};
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(100);

    struct Guard(PathBuf);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn test_paths(tag: &str) -> (Paths, Guard) {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-pool-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&dir);
        (
            Paths::new(dir.join("config"), dir.join("state")),
            Guard(dir),
        )
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

    fn entry(instance: &str, model: &str) -> PoolEntry {
        PoolEntry {
            instance: instance.into(),
            model: model.into(),
            options: BTreeMap::new(),
            enabled: true,
        }
    }

    #[test]
    fn round_trip_without_pool() {
        let (paths, _g) = test_paths("nopol");
        Registry::update(&paths, |r| r.add(spec("a"))).unwrap();
        let raw = fs::read_to_string(paths.registry_file()).unwrap();
        assert!(!raw.contains("[[pool]]"), "{raw}");
        let reg = Registry::load(&paths).unwrap();
        assert!(reg.pool.is_empty());
    }

    #[test]
    fn round_trip_with_pool() {
        let (paths, _g) = test_paths("roundtrip");
        Registry::update(&paths, |r| {
            r.add(spec("a"))?;
            let mut e = entry("a", "owner/model");
            e.options
                .insert("effort".into(), PoolValue::String("high".into()));
            e.enabled = false;
            r.pool.push(e);
            Ok(())
        })
        .unwrap();
        let raw = fs::read_to_string(paths.registry_file()).unwrap();
        assert!(raw.contains("[[pool]]"), "{raw}");
        let reg = Registry::load(&paths).unwrap();
        assert_eq!(reg.pool.len(), 1);
        assert_eq!(
            reg.pool[0].options["effort"],
            PoolValue::String("high".into())
        );
        assert!(!reg.pool[0].enabled);
        // enabled defaults to true when omitted
        let reg2 = Registry::from_toml(
            "version = 1\n[[instance]]\nid = \"a\"\nharness = \"claude\"\nhome = \"h\"\n\
             [[pool]]\ninstance = \"a\"\nmodel = \"m\"\n",
        )
        .unwrap();
        assert!(reg2.pool[0].enabled);
    }

    #[test]
    fn uniqueness_and_unknown_instance() {
        let (paths, _g) = test_paths("uniq");
        Registry::update(&paths, |r| r.add(spec("a"))).unwrap();
        let mut reg = Registry::load(&paths).unwrap();
        reg.add_pool(entry("a", "m")).unwrap();
        assert!(reg.add_pool(entry("a", "m")).is_err());
        assert!(reg.add_pool(entry("nope", "m")).is_err());
        // same instance+model but different options is fine
        let mut e2 = entry("a", "m");
        e2.options
            .insert("effort".into(), PoolValue::String("low".into()));
        reg.add_pool(e2).unwrap();
        assert!(
            Registry::from_toml("version = 1\n[[pool]]\ninstance = \"a\"\nmodel = \"m\"\n")
                .is_err()
        );
    }

    #[test]
    fn instance_removal_drops_its_pool_entries() {
        let (paths, _g) = test_paths("cascade");
        Registry::update(&paths, |r| {
            r.add(spec("a"))?;
            r.add(spec("b"))?;
            r.add_pool(entry("a", "m"))?;
            r.add_pool(entry("b", "m"))?;
            Ok(())
        })
        .unwrap();
        Registry::update(&paths, |r| {
            r.remove("a")?;
            Ok(())
        })
        .unwrap();
        let reg = Registry::load(&paths).unwrap();
        assert!(reg.get("a").is_none());
        let left: Vec<&str> = reg.pool.iter().map(|e| e.instance.as_str()).collect();
        assert_eq!(left, ["b"]);
    }

    #[test]
    fn cache_warning() {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-t3-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("caches")).unwrap();
        fs::write(
            dir.join("caches/a.json"),
            r#"{"models":[{"slug":"yes-model"}],"checkedAt":"2026-01-01"}"#,
        )
        .unwrap();
        assert!(check_model_in(&dir, "a", "yes-model").is_none());
        let w = check_model_in(&dir, "a", "no-model").unwrap();
        assert!(w.contains("no-model"), "{w}");
        assert!(check_model_in(&dir, "missing", "m").is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dry_run_writes_nothing() {
        let (paths, _g) = test_paths("dry");
        Registry::update(&paths, |r| r.add(spec("a"))).unwrap();
        let before = fs::read(paths.registry_file()).unwrap();
        let (changes, _) = add_pool_entry(&paths, entry("a", "m"), true).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(fs::read(paths.registry_file()).unwrap(), before);
        let (changes, _) = add_pool_entry(&paths, entry("a", "m"), false).unwrap();
        assert_eq!(changes.len(), 1);
        let changes = remove_pool_entry(&paths, "a", "m", &BTreeMap::new(), true).unwrap();
        assert_eq!(changes.len(), 1);
        assert!(
            Registry::load(&paths)
                .unwrap()
                .pool
                .iter()
                .any(|e| e.model == "m")
        );
    }

    #[test]
    fn catalog_includes_all_models_without_changing_selections() {
        let (paths, guard) = test_paths("catalog");
        let t3 = guard.0.join("t3");
        fs::create_dir_all(t3.join("caches")).unwrap();
        fs::write(
            t3.join("caches/a.json"),
            r#"{"models":[{"slug":"old"},{"slug":"new"},{"slug":"new"}]}"#,
        )
        .unwrap();
        fs::write(
            t3.join("caches/ghost.json"),
            r#"{"models":[{"slug":"ghost"}]}"#,
        )
        .unwrap();
        Registry::update(&paths, |r| {
            r.add(spec("a"))?;
            r.add_pool(entry("a", "old"))?;
            r.add_pool(entry("a", "custom"))?;
            Ok(())
        })
        .unwrap();
        let before = fs::read(paths.registry_file()).unwrap();
        let reg = Registry::load(&paths).unwrap();
        let models = catalog_pool(&reg, Some(&t3)).unwrap();
        assert_eq!(models.len(), 3);
        assert!(models.iter().any(|e| e.model == "old" && e.enabled));
        assert!(models.iter().any(|e| e.model == "new" && !e.enabled));
        assert!(models.iter().any(|e| e.model == "custom" && e.enabled));
        assert_eq!(fs::read(paths.registry_file()).unwrap(), before);
        fs::write(t3.join("caches/a.json"), "bad JSON").unwrap();
        assert!(
            catalog_pool(&reg, Some(&t3))
                .unwrap_err()
                .to_string()
                .contains("a.json")
        );
    }

    #[test]
    fn option_parsing() {
        assert_eq!(
            parse_option("effort=high").unwrap().1,
            PoolValue::String("high".into())
        );
        assert_eq!(parse_option("x=true").unwrap().1, PoolValue::Bool(true));
        assert_eq!(
            parse_option("x=False").unwrap().1,
            PoolValue::String("False".into())
        );
        assert!(parse_option("noeq").is_err());
    }
}
