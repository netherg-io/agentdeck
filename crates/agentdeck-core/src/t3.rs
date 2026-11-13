//! T3 integration: validated atomic merge-write of `providerInstances` in
//! `~/.t3/userdata/settings.json`, plus read-only health and thread counts.
//!
//! T3 itself is not managed: this module only edits `providerInstances` entries
//! for registry instances with `t3 = true` (validated, atomic, verified) and
//! reads `caches/<id>.json` and `statev2.sqlite` read-only. Registry instance
//! ids are never renamed (T3 thread history is bound to them). T3 root is
//! injectable: every function takes an explicit `t3_root` (production value is
//! [`t3_root()`); tests use temp dirs.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::Paths;
use crate::registry::{Harness, Instance, Registry};

/// Production T3 home: `$AGENTDECK_T3_DIR`, else `~/.t3`.
pub fn t3_root() -> anyhow::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("AGENTDECK_T3_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve T3 dir: set HOME or AGENTDECK_T3_DIR"))?;
    Ok(home.join(".t3"))
}

pub fn settings_file(t3_root: &Path) -> PathBuf {
    t3_root.join("userdata/settings.json")
}

fn caches_dir(t3_root: &Path) -> PathBuf {
    t3_root.join("caches")
}

fn state_db(t3_root: &Path) -> PathBuf {
    t3_root.join("userdata/statev2.sqlite")
}

/// Config field names agentdeck may write, per driver (R4 tables). `None` =
/// a driver agentdeck never writes (its instances are left untouched).
fn allowed_config_fields(driver: &str) -> Option<&'static [&'static str]> {
    match driver {
        "claudeAgent" => Some(&[
            "enabled",
            "binaryPath",
            "homePath",
            "customModels",
            "launchArgs",
            "autoCompactWindow",
            "idleCompaction",
        ]),
        "codex" => Some(&[
            "enabled",
            "binaryPath",
            "homePath",
            "shadowHomePath",
            "launchArgs",
            "setupMode",
            "customModels",
        ]),
        // R4 tables, minus secrets (never written; also rejected by name).
        "opencode" => Some(&["enabled", "binaryPath", "serverUrl", "customModels"]),
        "antigravity" => Some(&[
            "enabled",
            "authMethod",
            "gcpProject",
            "gcpLocation",
            "binaryPath",
            "customModels",
        ]),
        _ => None,
    }
}

/// Reject config keys agentdeck must not write (unknown per the R4 driver
/// table, or secret-carrying: secrets go through T3's own login/UI flows).
pub fn validate_config_fields(driver: &str, config: &Map<String, Value>) -> anyhow::Result<()> {
    let allowed = allowed_config_fields(driver)
        .ok_or_else(|| anyhow::anyhow!("no T3 mapping for driver {driver:?}: refusing to write"))?;
    for key in config.keys() {
        if !allowed.contains(&key.as_str()) {
            anyhow::bail!(
                "refusing to write unknown T3 config field {key:?} for driver {driver:?}"
            );
        }
        if is_secret_key(key) {
            anyhow::bail!("refusing to write secret field {key:?} for driver {driver:?}");
        }
    }
    Ok(())
}

fn is_secret_key(key: &str) -> bool {
    let lower = key.to_lowercase();
    ["secret", "password", "apikey", "api_key", "token"]
        .iter()
        .any(|s| lower.contains(s))
}

fn check_t3_slug(id: &str) -> anyhow::Result<()> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        anyhow::bail!("bad T3 instance id {id:?}: must match [A-Za-z0-9_-]+");
    }
    Ok(())
}

/// Desired T3 envelope for one registry instance. `main_codex_home` is the
/// home of the registry instance id `codex` when present.
/// T3 resolves `homePath` lexically, without expanding `~`.
fn absolute(path: &str) -> String {
    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    crate::sync::expand_tilde(path, &home).display().to_string()
}

fn desired_envelope(inst: &Instance, main_codex_home: Option<&str>) -> anyhow::Result<Value> {
    check_t3_slug(&inst.id)?;
    let driver = match inst.harness {
        Harness::Claude => "claudeAgent",
        Harness::Codex => "codex",
        Harness::Opencode => "opencode",
        Harness::Agy => "antigravity",
    };
    let mut config = Map::new();
    match inst.harness {
        Harness::Claude => {
            config.insert("homePath".into(), Value::String(absolute(&inst.home)));
            if let Some(bin) = &inst.binary {
                config.insert("binaryPath".into(), Value::String(bin.clone()));
            }
        }
        Harness::Codex => {
            let main = main_codex_home.unwrap_or(&inst.home);
            config.insert("homePath".into(), Value::String(absolute(main)));
            if inst.home != main {
                config.insert("shadowHomePath".into(), Value::String(inst.home.clone()));
            }
            if let Some(bin) = &inst.binary {
                config.insert("binaryPath".into(), Value::String(bin.clone()));
            }
        }
        // R4 drivers have no home concept; only the binary is managed.
        // Secrets (serverPassword, apiKey) are never written.
        Harness::Opencode | Harness::Agy => {
            if let Some(bin) = &inst.binary {
                config.insert("binaryPath".into(), Value::String(bin.clone()));
            }
        }
    }
    validate_config_fields(driver, &config)?;
    let display_name = if inst.display_name.is_empty() {
        inst.id.clone()
    } else {
        inst.display_name.clone()
    };
    let mut env = Map::new();
    env.insert("driver".into(), Value::String(driver.into()));
    env.insert("displayName".into(), Value::String(display_name));
    env.insert("enabled".into(), Value::Bool(inst.enabled));
    env.insert("config".into(), Value::Object(config));
    Ok(Value::Object(env))
}

/// Desired envelopes for every registry instance agentdeck manages in T3
/// (all four harnesses with `t3 = true`).
fn desired_map(reg: &Registry) -> anyhow::Result<BTreeMap<String, Value>> {
    let main_codex_home = reg
        .get("codex")
        .filter(|i| i.harness == Harness::Codex && i.t3)
        .map(|i| i.home.clone());
    let mut out = BTreeMap::new();
    for inst in &reg.instances {
        if !inst.t3 {
            continue;
        }
        out.insert(
            inst.id.clone(),
            desired_envelope(inst, main_codex_home.as_deref())?,
        );
    }
    Ok(out)
}

/// T3 keeps `homePath` absolute or empty (empty = the harness default), so an
/// equivalent `~/` spelling from the registry must not rewrite it.
fn same_home(old: &str, want: &Value) -> bool {
    let Some(want) = want.as_str() else {
        return false;
    };
    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let want = crate::sync::expand_tilde(want, &home);
    if old.is_empty() {
        return want == home.join(".claude") || want == home.join(".codex");
    }
    crate::sync::expand_tilde(old, &home) == want
}

/// Merge desired envelopes into parsed settings: only `providerInstances`
/// entries for managed ids change, and within an existing envelope only
/// `driver`/`displayName`/`enabled` plus our own `config` keys. Everything
/// else (accentColor, customModels, launchArgs, environment, unknown keys,
/// foreign instances, all other top-level keys) is preserved verbatim.
pub fn merge_settings(old: &Value, desired: &BTreeMap<String, Value>) -> anyhow::Result<Value> {
    let mut new = old.clone();
    let pi = new
        .get_mut("providerInstances")
        .and_then(|v| v.as_object_mut())
        .ok_or_else(|| anyhow::anyhow!("settings.json has no providerInstances object"))?;
    for (id, env) in desired {
        check_t3_slug(id)?;
        let want = env.as_object().expect("desired_envelope builds objects");
        match pi.get_mut(id) {
            Some(existing) => {
                let obj = existing
                    .as_object_mut()
                    .ok_or_else(|| anyhow::anyhow!("providerInstances[{id:?}] is not an object"))?;
                for key in ["driver", "enabled"] {
                    obj.insert(key.into(), want[key].clone());
                }
                // T3 has its own label for a nameless default instance: keep it nameless.
                if obj.contains_key("displayName") {
                    obj.insert("displayName".into(), want["displayName"].clone());
                }
                let cfg = obj
                    .entry("config")
                    .or_insert_with(|| Value::Object(Map::new()));
                let cfg_obj = cfg.as_object_mut().ok_or_else(|| {
                    anyhow::anyhow!("providerInstances[{id:?}].config is not an object")
                })?;
                let want_cfg = want["config"].as_object().expect("envelope has config");
                validate_config_fields(want["driver"].as_str().unwrap_or_default(), want_cfg)?;
                for (k, v) in want_cfg {
                    let unchanged = k == "homePath"
                        && same_home(cfg_obj.get(k).and_then(Value::as_str).unwrap_or(""), v);
                    if !unchanged {
                        cfg_obj.insert(k.clone(), v.clone());
                    }
                }
            }
            None => {
                pi.insert(id.clone(), env.clone());
            }
        }
    }
    Ok(new)
}

/// Leading-indent unit of a JSON file (`"  "`, `"    "`, …), from the first
/// indented line; defaults to two spaces (what T3 itself writes).
fn detect_indent(raw: &str) -> String {
    for line in raw.lines() {
        let n = line.chars().take_while(|c| *c == ' ').count();
        if n > 0 && line[n..].starts_with('"') {
            return " ".repeat(n.min(8));
        }
    }
    "  ".to_string()
}

fn apply_indent(pretty: &str, indent: &str) -> String {
    if indent == "  " {
        return pretty.to_string();
    }
    pretty
        .lines()
        .map(|line| {
            let n = line.chars().take_while(|c| *c == ' ').count();
            let level = n / 2;
            format!("{}{}", indent.repeat(level), &line[n..])
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Backup, atomically write (tmp + fsync + rename, original mode kept),
/// re-read and verify the result equals what was intended.
fn write_settings_verified(
    t3_root: &Path,
    new: &Value,
    indent: &str,
    backup_dir: &Path,
) -> anyhow::Result<()> {
    let path = settings_file(t3_root);
    let old_bytes = fs::read(&path)?;
    fs::create_dir_all(backup_dir)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    fs::write(
        backup_dir.join(format!(
            "settings.json.{}.{}",
            stamp.as_secs(),
            stamp.subsec_nanos()
        )),
        &old_bytes,
    )?;
    let mut text = apply_indent(&serde_json::to_string_pretty(new)?, indent);
    text.push('\n');
    let mode = fs::metadata(&path)?.permissions().mode() & 0o777;
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        use std::io::Write as _;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    fs::rename(&tmp, &path)?;
    // Verify: re-read must parse and equal what was intended.
    let back: Value = serde_json::from_slice(&fs::read(&path)?)
        .map_err(|e| anyhow::anyhow!("wrote settings.json but cannot re-parse it: {e}"))?;
    if back != *new {
        anyhow::bail!("wrote settings.json but re-read differs from intent");
    }
    Ok(())
}

fn parse_settings_strict(raw: &[u8]) -> anyhow::Result<Value> {
    // Strict parse: any decode problem aborts (T3 itself discards the whole
    // file to defaults on error, so agentdeck must never write then).
    serde_json::from_slice(raw).map_err(|e| anyhow::anyhow!("cannot parse settings.json: {e}"))
}

/// Redact secret values for dry-run output: secret-named keys, and
/// `environment[]` entries with `sensitive: true`.
pub fn redact(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = Map::with_capacity(map.len());
            for (k, v) in map {
                if k == "environment"
                    && let Value::Array(items) = v
                {
                    out.insert(
                        k.clone(),
                        Value::Array(
                            items
                                .iter()
                                .map(|e| {
                                    let sensitive = e
                                        .get("sensitive")
                                        .and_then(Value::as_bool)
                                        .unwrap_or(false);
                                    if sensitive && let Value::Object(mut em) = e.clone() {
                                        if em.contains_key("value") {
                                            em.insert("value".into(), Value::String("***".into()));
                                        }
                                        return Value::Object(em);
                                    }
                                    redact(e)
                                })
                                .collect(),
                        ),
                    );
                } else if is_secret_key(k) {
                    out.insert(k.clone(), Value::String("***".into()));
                } else {
                    out.insert(k.clone(), redact(v));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(redact).collect()),
        _ => value.clone(),
    }
}

#[derive(Debug, Clone)]
pub struct SyncEntry {
    pub id: String,
    pub action: &'static str, // "created" | "updated" | "unchanged"
    pub before: Option<Value>,
    pub after: Value,
}

/// Re-read settings.json, merge managed `providerInstances`, and (unless
/// dry-run) backup + atomic-write + verify. Never deletes T3 instances that
/// are not in the registry. Returns per-instance entries; dry-run writes
/// nothing.
pub fn sync_t3(paths: &Paths, t3_root: &Path, dry_run: bool) -> anyhow::Result<Vec<SyncEntry>> {
    let raw = fs::read(settings_file(t3_root))
        .map_err(|e| anyhow::anyhow!("cannot read settings.json: {e}"))?;
    let old = parse_settings_strict(&raw)?;
    if old
        .get("providerInstances")
        .and_then(|v| v.as_object())
        .is_none()
    {
        anyhow::bail!("settings.json has no providerInstances object");
    }
    let reg = Registry::load(paths)?;
    let desired = desired_map(&reg)?;
    let new = merge_settings(&old, &desired)?;
    let old_pi = &old["providerInstances"];
    let mut entries = Vec::with_capacity(desired.len());
    for id in desired.keys() {
        let before = old_pi.get(id).cloned();
        let after = new["providerInstances"][id].clone();
        let action = match &before {
            None => "created",
            Some(b) if *b == after => "unchanged",
            _ => "updated",
        };
        entries.push(SyncEntry {
            id: id.clone(),
            action,
            before,
            after,
        });
    }
    if new != old && !dry_run {
        write_settings_verified(
            t3_root,
            &new,
            &detect_indent(&String::from_utf8_lossy(&raw)),
            &paths.backup_dir(),
        )?;
    }
    Ok(entries)
}

/// Update just the display name; provider ids, config and thread history stay intact.
pub fn rename_instance(
    paths: &Paths,
    root: Option<&Path>,
    id: &str,
    name: &str,
    dry_run: bool,
) -> anyhow::Result<Vec<crate::Change>> {
    let mut reg = Registry::load(paths)?;
    reg.rename(id, name)?;
    let inst = reg.get(id).expect("rename validated the instance");
    let mut settings = None;
    if inst.t3
        && let Some(root) = root
        && settings_file(root).exists()
    {
        let raw = fs::read(settings_file(root))?;
        let mut value = parse_settings_strict(&raw)?;
        if let Some(provider) = value
            .get_mut("providerInstances")
            .and_then(|p| p.get_mut(id))
        {
            let object = provider
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("invalid T3 instance {id:?}"))?;
            if object.get("displayName").and_then(Value::as_str) != Some(&inst.display_name) {
                object.insert(
                    "displayName".into(),
                    Value::String(inst.display_name.clone()),
                );
                settings = Some((root, value, detect_indent(&String::from_utf8_lossy(&raw))));
            }
        }
    }
    let changes = crate::registry::rename_instance(paths, id, name, dry_run)?;
    if !dry_run && let Some((root, value, indent)) = settings {
        write_settings_verified(root, &value, &indent, &paths.backup_dir())
            .map_err(|e| anyhow::anyhow!("name saved in Agentdeck; T3 update failed: {e}"))?;
    }
    Ok(changes)
}

#[derive(Debug, Clone)]
pub struct CacheHealth {
    pub status: String,
    pub checked_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct T3StatusRow {
    pub id: String,
    pub driver: String,
    pub in_registry: bool,
    pub enabled: bool,
    pub cache: Option<CacheHealth>,
    pub threads: Option<u64>,
}

/// Read-only health for one instance: `caches/<id>.json` (`status` +
/// `checkedAt`; missing file means no health).
pub fn cache_health(t3_root: &Path, id: &str) -> Option<CacheHealth> {
    let raw = fs::read(caches_dir(t3_root).join(format!("{id}.json"))).ok()?;
    let v: Value = serde_json::from_slice(&raw).ok()?;
    Some(CacheHealth {
        status: v.get("status")?.as_str()?.to_string(),
        checked_at: v
            .get("checkedAt")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn empty_if_null<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct T3UsageWindow {
    pub id: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub window_duration_mins: Option<u64>,
    #[serde(default)]
    pub used_percent: Option<f64>,
    #[serde(default)]
    pub resets_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct T3UsageLimitsUnavailable {
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct T3UsageLimits {
    #[serde(default)]
    pub checked_at: Option<String>,
    #[serde(default, deserialize_with = "empty_if_null")]
    pub windows: Vec<T3UsageWindow>,
    #[serde(default)]
    pub unavailable: Option<T3UsageLimitsUnavailable>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct T3AuthInfo {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default, rename = "type")]
    pub auth_type: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct T3ProviderCache {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub checked_at: Option<String>,
    #[serde(default)]
    pub auth: Option<T3AuthInfo>,
    #[serde(default)]
    pub usage_limits: Option<T3UsageLimits>,
}

pub fn cache_file(t3_root: &Path, id: &str) -> PathBuf {
    caches_dir(t3_root).join(format!("{id}.json"))
}

/// Authoritative enabled flag for one T3 instance from `userdata/settings.json`
/// (`providerInstances.<id>.enabled`). Returns `Ok(None)` when the instance id is
/// absent from `providerInstances`; refuses missing/corrupt settings.
pub fn read_instance_enabled(t3_root: &Path, id: &str) -> anyhow::Result<Option<bool>> {
    check_t3_slug(id)?;
    if !t3_root.exists() {
        anyhow::bail!(
            "T3 data directory not found at {}; start T3 Code",
            t3_root.display()
        );
    }
    let settings_path = settings_file(t3_root);
    let raw = fs::read(&settings_path)
        .map_err(|e| anyhow::anyhow!("cannot read T3 {}: {e}", settings_path.display()))?;
    let settings = parse_settings_strict(&raw)?;
    let pi = settings
        .get("providerInstances")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("T3 settings.json has no providerInstances object"))?;
    let Some(entry) = pi.get(id) else {
        return Ok(None);
    };
    Ok(Some(
        entry
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    ))
}

/// Read and strictly parse `caches/<id>.json` (`Ok(None)` if the file does not
/// exist; `Err` on half-written/malformed JSON or I/O error).
pub fn read_usage_cache(t3_root: &Path, id: &str) -> anyhow::Result<Option<T3ProviderCache>> {
    check_t3_slug(id)?;
    let path = cache_file(t3_root, id);
    let raw = match fs::read(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => anyhow::bail!("cannot read T3 cache {}: {e}", path.display()),
    };
    let parsed: T3ProviderCache = serde_json::from_slice(&raw)
        .map_err(|e| anyhow::anyhow!("invalid T3 cache {}: {e}", path.display()))?;
    Ok(Some(parsed))
}

/// Read-only thread count from `statev2.sqlite` (read-only connection, never
/// touches WAL). `Ok(None)` = db missing (or table absent): "n/a".
pub fn thread_count(t3_root: &Path, id: &str) -> anyhow::Result<Option<u64>> {
    let db = state_db(t3_root);
    if !db.exists() {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| anyhow::anyhow!("cannot open statev2.sqlite read-only: {e}"))?;
    match conn.query_row(
        "SELECT COUNT(*) FROM orchestration_v2_projection_provider_threads WHERE provider_instance_id = ?1",
        [id],
        |row| row.get::<_, u64>(0),
    ) {
        Ok(n) => Ok(Some(n)),
        Err(e) if e.to_string().contains("no such table") => Ok(None),
        Err(e) => Err(anyhow::anyhow!("thread count query failed: {e}")),
    }
}

/// One read-only row per T3 instance.
pub fn t3_status(paths: &Paths, t3_root: &Path) -> anyhow::Result<Vec<T3StatusRow>> {
    let raw = fs::read(settings_file(t3_root))
        .map_err(|e| anyhow::anyhow!("cannot read settings.json: {e}"))?;
    let settings = parse_settings_strict(&raw)?;
    let pi = settings
        .get("providerInstances")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow::anyhow!("settings.json has no providerInstances object"))?;
    let reg = Registry::load(paths).unwrap_or_default();
    let mut rows = Vec::with_capacity(pi.len());
    for (id, env) in pi {
        let threads = thread_count(t3_root, id)?;
        rows.push(T3StatusRow {
            id: id.clone(),
            driver: env
                .get("driver")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string(),
            in_registry: reg.get(id).is_some(),
            enabled: env.get("enabled").and_then(Value::as_bool).unwrap_or(false),
            cache: cache_health(t3_root, id),
            threads,
        });
    }
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(rows)
}

#[derive(Debug, Clone)]
pub struct Imported {
    pub id: String,
    pub harness: Harness,
    pub display_name: String,
    pub home: String,
}

/// Add T3 claudeAgent/codex instances missing from the registry into it
/// (id, harness, display_name, home from config; t3 = true).
pub fn import_t3(paths: &Paths, t3_root: &Path, dry_run: bool) -> anyhow::Result<Vec<Imported>> {
    let raw = fs::read(settings_file(t3_root))
        .map_err(|e| anyhow::anyhow!("cannot read settings.json: {e}"))?;
    let settings = parse_settings_strict(&raw)?;
    let pi = settings
        .get("providerInstances")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow::anyhow!("settings.json has no providerInstances object"))?;
    let reg = Registry::load(paths)?;
    let mut found = Vec::new();
    for (id, env) in pi {
        if reg.get(id).is_some() {
            continue;
        }
        let driver = env.get("driver").and_then(Value::as_str).unwrap_or("");
        let harness = match driver {
            "claudeAgent" => Harness::Claude,
            "codex" => Harness::Codex,
            "opencode" => Harness::Opencode,
            "antigravity" => Harness::Agy,
            _ => continue,
        };
        check_t3_slug(id)?;
        let cfg = env.get("config").and_then(Value::as_object);
        let str_field = |k: &str| {
            cfg.and_then(|c| c.get(k))
                .and_then(Value::as_str)
                .unwrap_or("")
        };
        let home = match harness {
            Harness::Codex => {
                let shadow = str_field("shadowHomePath");
                if !shadow.is_empty() {
                    shadow.to_string()
                } else {
                    let main = str_field("homePath");
                    if main.is_empty() {
                        "~/.codex".to_string()
                    } else {
                        main.to_string()
                    }
                }
            }
            Harness::Claude => {
                let h = str_field("homePath");
                if h.is_empty() {
                    "~/.claude".to_string()
                } else {
                    h.to_string()
                }
            }
            // R4 drivers carry no home: registry home is the login root.
            Harness::Opencode | Harness::Agy => "~".to_string(),
        };
        let display_name = env
            .get("displayName")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(id)
            .to_string();
        let enabled = env.get("enabled").and_then(Value::as_bool).unwrap_or(true);
        found.push(Imported {
            id: id.clone(),
            harness,
            display_name: display_name.clone(),
            home: home.clone(),
        });
        if !dry_run {
            let (id_c, harness_c, name_c, home_c) =
                (id.clone(), harness, display_name.clone(), home.clone());
            Registry::update(paths, |r| {
                use crate::registry::AddSpec;
                r.add(AddSpec {
                    id: id_c.clone(),
                    harness: harness_c,
                    display_name: Some(name_c.clone()),
                    home: home_c.clone(),
                    binary: None,
                    no_t3: false,
                })?;
                r.set_enabled(&id_c, enabled)?;
                Ok(())
            })?;
        }
    }
    found.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(found)
}

/// Outcome of [`reenable`]. Exit codes map as 0 = Recovered or DryRun,
/// 1 = FailedAgain, 2 = Timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReenableOutcome {
    Recovered,
    FailedAgain,
    Timeout,
    DryRun,
}

#[derive(Debug, Clone)]
pub struct ReenableOpts {
    /// Stage-2 deadline in seconds: wait for a fresh probe after writing
    /// `enabled = true` (E2: 20 s probe + watch/cache margin; default 30).
    pub timeout_secs: u64,
    /// Skip the enabled-and-ready refusal.
    pub force: bool,
    /// Validate and report only; write nothing.
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
pub struct ReenableReport {
    pub outcome: ReenableOutcome,
    /// Settings writes plus every observed cache transition, in order.
    pub events: Vec<String>,
    /// Final line for the CLI: recovered / failed again / timeout / dry-run.
    pub final_line: String,
}

/// Full cache snapshot for the toggle protocol (E2 §4-5): T3 writes
/// `enabled`, `status`, `checkedAt` (and a `message` on failure).
#[derive(Debug, Clone, PartialEq, Eq)]
struct CacheState {
    enabled: Option<bool>,
    status: Option<String>,
    checked_at: Option<String>,
    message: Option<String>,
}

fn read_cache_state(t3_root: &Path, id: &str) -> Option<CacheState> {
    let raw = fs::read(caches_dir(t3_root).join(format!("{id}.json"))).ok()?;
    let v: Value = serde_json::from_slice(&raw).ok()?;
    let text = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    Some(CacheState {
        enabled: v.get("enabled").and_then(Value::as_bool),
        status: text("status"),
        checked_at: text("checkedAt"),
        message: text("message"),
    })
}

fn cache_line(id: &str, s: &CacheState) -> String {
    format!(
        "cache: {id} enabled={} status={} checkedAt={}",
        s.enabled.map_or("?".to_string(), |b| b.to_string()),
        s.status.as_deref().unwrap_or("?"),
        s.checked_at.as_deref().unwrap_or("?"),
    )
}

/// Re-read settings, set only `providerInstances[id].enabled` (plus a
/// conflicting legacy nested `config.enabled`, which would also force the
/// instance off), then backup + atomic-write + verify. Other instances and
/// fields are preserved verbatim. Returns whether the file changed.
fn set_instance_enabled(
    t3_root: &Path,
    backup_dir: &Path,
    id: &str,
    enabled: bool,
) -> anyhow::Result<bool> {
    check_t3_slug(id)?;
    let raw = fs::read(settings_file(t3_root))
        .map_err(|e| anyhow::anyhow!("cannot read settings.json: {e}"))?;
    let indent = detect_indent(&String::from_utf8_lossy(&raw));
    let old = parse_settings_strict(&raw)?;
    let mut new = old.clone();
    let pi = new
        .get_mut("providerInstances")
        .and_then(|v| v.as_object_mut())
        .ok_or_else(|| anyhow::anyhow!("settings.json has no providerInstances object"))?;
    let env = pi
        .get_mut(id)
        .ok_or_else(|| anyhow::anyhow!("unknown T3 instance {id:?}"))?;
    let obj = env
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("providerInstances[{id:?}] is not an object"))?;
    obj.insert("enabled".into(), Value::Bool(enabled));
    if let Some(cfg) = obj.get_mut("config").and_then(|v| v.as_object_mut())
        && cfg.get("enabled").and_then(Value::as_bool) == Some(!enabled)
    {
        cfg.insert("enabled".into(), Value::Bool(enabled));
    }
    if new == old {
        return Ok(false);
    }
    write_settings_verified(t3_root, &new, &indent, backup_dir)?;
    Ok(true)
}

/// Poll `caches/<id>.json` every 100 ms until `accept` matches or the
/// deadline passes. Every observed state change is pushed to `events`.
fn poll_cache(
    t3_root: &Path,
    id: &str,
    deadline: std::time::Instant,
    events: &mut Vec<String>,
    last: &mut Option<CacheState>,
    accept: impl Fn(&CacheState) -> bool,
) -> Option<CacheState> {
    loop {
        if let Some(state) = read_cache_state(t3_root, id) {
            if last.as_ref() != Some(&state) {
                events.push(cache_line(id, &state));
                *last = Some(state.clone());
            }
            if accept(&state) {
                return Some(state);
            }
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Re-enable a failed instance with the false→observe→true toggle (E2 §6):
/// write `enabled = false`, wait up to 5 s for a fresh disabled cache result
/// (proves T3 saw the write despite its 100 ms settings debounce), re-read
/// and write `enabled = true`, then wait up to `timeout_secs` for a fresh
/// probe newer than the disabled acknowledgement. Refuses when the instance
/// is enabled and its cache says ready (nothing to do) unless forced.
pub fn reenable(
    paths: &Paths,
    t3_root: &Path,
    id: &str,
    opts: &ReenableOpts,
) -> anyhow::Result<ReenableReport> {
    use std::time::{Duration, Instant};
    check_t3_slug(id)?;
    if opts.timeout_secs == 0 {
        anyhow::bail!("timeout-secs must be at least 1");
    }
    let mut events = Vec::new();
    let raw = fs::read(settings_file(t3_root))
        .map_err(|e| anyhow::anyhow!("cannot read settings.json: {e}"))?;
    let settings = parse_settings_strict(&raw)?;
    let env = settings
        .get("providerInstances")
        .and_then(|v| v.as_object())
        .and_then(|pi| pi.get(id))
        .ok_or_else(|| anyhow::anyhow!("unknown T3 instance {id:?}"))?;
    let enabled_now = env.get("enabled").and_then(Value::as_bool).unwrap_or(false);
    let pre = read_cache_state(t3_root, id);
    if enabled_now
        && !opts.force
        && pre
            .as_ref()
            .is_some_and(|s| s.status.as_deref() == Some("ready"))
    {
        anyhow::bail!("nothing to do: {id:?} is enabled and its cache says ready (use --force)");
    }
    if opts.dry_run {
        return Ok(ReenableReport {
            outcome: ReenableOutcome::DryRun,
            events,
            final_line: format!(
                "dry-run: would set {id:?} enabled=false, wait for a fresh disabled cache, set enabled=true, wait up to {}s; settings.json unchanged",
                opts.timeout_secs
            ),
        });
    }
    // Stage 1: false, then wait for acknowledgement (5 s deadline per E2).
    let mut last = pre.clone();
    if set_instance_enabled(t3_root, &paths.backup_dir(), id, false)? {
        events.push(format!("settings: {id} enabled=false"));
    } else {
        events.push(format!("settings: {id} already enabled=false"));
    }
    let baseline = pre.as_ref().and_then(|s| s.checked_at.clone());
    let already_acked = !enabled_now
        && pre
            .as_ref()
            .is_some_and(|s| s.enabled == Some(false) && s.status.as_deref() == Some("disabled"));
    let ack = if already_acked {
        pre.clone().expect("already_acked implies cache present")
    } else {
        match poll_cache(
            t3_root,
            id,
            Instant::now() + Duration::from_secs(5),
            &mut events,
            &mut last,
            |s| {
                s.enabled == Some(false)
                    && s.status.as_deref() == Some("disabled")
                    && s.checked_at != baseline
            },
        ) {
            Some(s) => s,
            None => {
                return Ok(ReenableReport {
                    outcome: ReenableOutcome::Timeout,
                    events,
                    final_line: format!(
                        "{id}: timeout: no fresh disabled cache; settings enabled=false, runtime recovery unverified"
                    ),
                });
            }
        }
    };
    // Stage 2: true, then wait for a fresh probe newer than the ack.
    let ack_ts = ack.checked_at.clone();
    set_instance_enabled(t3_root, &paths.backup_dir(), id, true)?;
    events.push(format!("settings: {id} enabled=true"));
    let fresh_terminal = poll_cache(
        t3_root,
        id,
        Instant::now() + Duration::from_secs(opts.timeout_secs),
        &mut events,
        &mut last,
        |s| {
            let fresh = s.checked_at != ack_ts
                || (ack_ts.is_none()
                    && s.enabled == Some(true)
                    && matches!(
                        s.status.as_deref(),
                        Some("ready" | "error" | "disabled" | "unavailable")
                    ));
            fresh
                && matches!(
                    s.status.as_deref(),
                    Some("ready" | "error" | "disabled" | "unavailable")
                )
        },
    );
    match fresh_terminal {
        Some(s) if s.status.as_deref() == Some("ready") && s.enabled != Some(false) => {
            Ok(ReenableReport {
                outcome: ReenableOutcome::Recovered,
                events,
                final_line: format!(
                    "{id}: recovered: ready @ {}",
                    s.checked_at.as_deref().unwrap_or("?")
                ),
            })
        }
        Some(s) => {
            let msg = s
                .message
                .clone()
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| s.status.clone().unwrap_or_else(|| "unknown".into()));
            Ok(ReenableReport {
                outcome: ReenableOutcome::FailedAgain,
                events,
                final_line: format!("{id}: failed again: {msg}"),
            })
        }
        None => Ok(ReenableReport {
            outcome: ReenableOutcome::Timeout,
            events,
            final_line: format!("{id}: timeout: settings updated, runtime recovery unverified"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::AddSpec;
    use std::sync::atomic::{AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);

    struct Guard(PathBuf);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn setup(tag: &str, settings: &str) -> (Paths, PathBuf, Guard) {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-t3-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&dir);
        let paths = Paths::new(dir.join("config"), dir.join("state"));
        let t3 = dir.join("t3");
        fs::create_dir_all(t3.join("userdata")).unwrap();
        fs::create_dir_all(t3.join("caches")).unwrap();
        fs::write(t3.join("userdata/settings.json"), settings).unwrap();
        (paths, t3, Guard(dir))
    }

    fn reg(paths: &Paths, specs: &[(&str, Harness, &str)]) {
        Registry::update(paths, |r| {
            for (id, h, home) in specs {
                r.add(AddSpec {
                    id: id.to_string(),
                    harness: *h,
                    display_name: None,
                    home: home.to_string(),
                    binary: None,
                    no_t3: false,
                })?;
            }
            Ok(())
        })
        .unwrap();
    }

    const SETTINGS: &str = r##"{
  "favorites": [],
  "providerHealthRefreshInterval": 300000,
  "providerInstances": {
    "codex": {
      "driver": "codex",
      "enabled": true,
      "config": {
        "binaryPath": "codex",
        "homePath": "",
        "shadowHomePath": "",
        "launchArgs": "",
        "customModels": []
      }
    },
    "codex_second": {
      "driver": "codex",
      "displayName": "Codex 2",
      "enabled": true,
      "accentColor": "#123456",
      "config": {
        "setupMode": "existing",
        "binaryPath": "codex",
        "homePath": "/main/.codex",
        "shadowHomePath": "/second/.codex",
        "launchArgs": "--foo"
      },
      "environment": [{"name": "X", "value": "1", "sensitive": false}]
    },
    "foreign": {
      "driver": "grok",
      "enabled": false,
      "config": {"binaryPath": "grok"}
    }
  }
}
"##;

    #[test]
    fn rename_keeps_ids_config_and_other_instances() {
        let dir = std::env::temp_dir().join(format!("t3-rename-{}", std::process::id()));
        let paths = Paths::new(dir.join("config"), dir.join("state"));
        let root = dir.join("t3");
        Registry::update(&paths, |r| {
            r.add(crate::AddSpec {
                id: "a".into(),
                harness: Harness::Codex,
                display_name: Some("Old".into()),
                home: "/fake/a".into(),
                binary: None,
                no_t3: false,
            })?;
            Ok(())
        })
        .unwrap();
        fs::create_dir_all(root.join("userdata")).unwrap();
        let old = serde_json::json!({"providerInstances":{"a":{"driver":"codex","displayName":"Old","config":{"customModels":["m"]}},"b":{"displayName":"Other"}},"favorites":[]});
        fs::write(settings_file(&root), old.to_string()).unwrap();
        rename_instance(&paths, Some(&root), "a", "  Новое имя  ", true).unwrap();
        assert_eq!(
            Registry::load(&paths)
                .unwrap()
                .get("a")
                .unwrap()
                .display_name,
            "Old"
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(settings_file(&root)).unwrap()).unwrap(),
            old
        );
        rename_instance(&paths, Some(&root), "a", "Новое имя", false).unwrap();
        let after: Value =
            serde_json::from_slice(&fs::read(settings_file(&root)).unwrap()).unwrap();
        let mut expected = old;
        expected["providerInstances"]["a"]["displayName"] = Value::from("Новое имя");
        assert_eq!(after, expected);
        let reg = Registry::load(&paths).unwrap();
        assert_eq!(reg.get("a").unwrap().display_name, "Новое имя");
        assert!(rename_instance(&paths, Some(&root), "a", "   ", false).is_err());
        fs::write(settings_file(&root), "bad json").unwrap();
        assert!(rename_instance(&paths, Some(&root), "a", "Not saved", false).is_err());
        assert_eq!(
            Registry::load(&paths)
                .unwrap()
                .get("a")
                .unwrap()
                .display_name,
            "Новое имя"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn merge_preserves_unknown_fields_order_and_foreign() {
        let (paths, t3, _g) = setup("merge", SETTINGS);
        reg(
            &paths,
            &[("codex_second", Harness::Codex, "/second/.codex")],
        );
        // Registry's codex main home is unknown here, so homePath stays as-is
        // for codex_second? No: no "codex" in registry -> main = own home.
        let entries = sync_t3(&paths, &t3, false).unwrap();
        assert_eq!(entries.len(), 1);
        let raw = fs::read_to_string(settings_file(&t3)).unwrap();
        let v: Value = serde_json::from_str(&raw).unwrap();
        let cs = &v["providerInstances"]["codex_second"];
        // Unknown fields preserved.
        assert_eq!(cs["accentColor"], Value::String("#123456".into()));
        assert_eq!(cs["displayName"], Value::String("codex_second".into()));
        assert_eq!(cs["config"]["setupMode"], Value::String("existing".into()));
        assert_eq!(cs["config"]["launchArgs"], Value::String("--foo".into()));
        assert!(cs.get("environment").is_some());
        // Foreign instance untouched, main codex untouched.
        assert!(v["providerInstances"].get("foreign").is_some());
        assert_eq!(
            v["providerInstances"]["codex"]["config"]["homePath"],
            Value::String("".into())
        );
        // Top-level order preserved: favorites still first, providerInstances last.
        let top: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
        assert_eq!(top.first().map(String::as_str), Some("favorites"));
        assert_eq!(top.last().map(String::as_str), Some("providerInstances"));
        // Formatting: 2-space indent + trailing newline.
        assert!(raw.starts_with("{\n  \"favorites\""));
        assert!(raw.ends_with("}\n"));
    }

    #[test]
    fn dry_run_writes_nothing() {
        let (paths, t3, _g) = setup("dryrun", SETTINGS);
        reg(
            &paths,
            &[("codex_second", Harness::Codex, "/second/.codex")],
        );
        let before = fs::read(settings_file(&t3)).unwrap();
        let entries = sync_t3(&paths, &t3, true).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].action, "updated");
        assert_eq!(fs::read(settings_file(&t3)).unwrap(), before);
        assert!(!paths.backup_dir().exists());
    }

    #[test]
    fn validation_rejects_unknown_field() {
        let mut cfg = Map::new();
        cfg.insert("homePath".into(), Value::String("/h".into()));
        cfg.insert("evilField".into(), Value::String("x".into()));
        assert!(validate_config_fields("codex", &cfg).is_err());
        assert!(validate_config_fields("nope", &cfg).is_err());
        cfg.remove("evilField");
        assert!(validate_config_fields("codex", &cfg).is_ok());
        cfg.insert("apiKey".into(), Value::String("s3cret".into()));
        assert!(validate_config_fields("antigravity", &cfg).is_err());
    }

    #[test]
    fn import_adds_missing_instances() {
        let (paths, t3, _g) = setup("import", SETTINGS);
        let found = import_t3(&paths, &t3, true).unwrap();
        let ids: Vec<_> = found.iter().map(|f| f.id.as_str()).collect();
        assert!(ids.contains(&"codex"), "{ids:?}");
        assert!(ids.contains(&"codex_second"), "{ids:?}");
        assert!(!ids.contains(&"foreign"), "{ids:?}");
        assert!(Registry::load(&paths).unwrap().instances.is_empty());
        let found = import_t3(&paths, &t3, false).unwrap();
        assert_eq!(found.len(), 2);
        let reg = Registry::load(&paths).unwrap();
        let cs = reg.get("codex_second").unwrap();
        assert!(matches!(cs.harness, Harness::Codex));
        assert_eq!(cs.home, "/second/.codex");
        assert_eq!(cs.display_name, "Codex 2");
        assert!(cs.t3);
        // Second import is a no-op.
        assert!(import_t3(&paths, &t3, false).unwrap().is_empty());
    }

    #[test]
    fn thread_count_from_fixture_db() {
        let (paths, t3, _g) = setup("threads", SETTINGS);
        assert_eq!(thread_count(&t3, "codex").unwrap(), None); // no db yet
        let db = state_db(&t3);
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE orchestration_v2_projection_provider_threads(provider_instance_id TEXT);
             INSERT INTO orchestration_v2_projection_provider_threads VALUES ('codex'), ('codex'), ('other');",
        )
        .unwrap();
        drop(conn);
        assert_eq!(thread_count(&t3, "codex").unwrap(), Some(2));
        assert_eq!(thread_count(&t3, "other").unwrap(), Some(1));
        assert_eq!(thread_count(&t3, "missing").unwrap(), Some(0));
        let _ = &paths;
    }

    #[test]
    fn status_rows_read_only() {
        let (paths, t3, _g) = setup("status", SETTINGS);
        reg(
            &paths,
            &[("codex_second", Harness::Codex, "/second/.codex")],
        );
        fs::write(
            t3.join("caches/codex_second.json"),
            r#"{"status": "ready", "checkedAt": "2026-10-24T00:00:00.000Z", "models": []}"#,
        )
        .unwrap();
        let rows = t3_status(&paths, &t3).unwrap();
        assert_eq!(rows.len(), 3);
        let cs = rows.iter().find(|r| r.id == "codex_second").unwrap();
        assert!(cs.in_registry);
        assert_eq!(cs.cache.as_ref().unwrap().status, "ready");
        assert!(cs.cache.as_ref().unwrap().checked_at.is_some());
        let foreign = rows.iter().find(|r| r.id == "foreign").unwrap();
        assert!(!foreign.in_registry);
        assert!(foreign.cache.is_none());
        assert_eq!(foreign.threads, None);
        // Status never writes.
        let raw = fs::read_to_string(settings_file(&t3)).unwrap();
        assert_eq!(raw, SETTINGS);
    }

    #[test]
    fn whole_file_rejected_on_error() {
        let (paths, t3, _g) = setup("broken", "{ not json");
        reg(&paths, &[("codex", Harness::Codex, "/h")]);
        assert!(sync_t3(&paths, &t3, false).is_err());
        assert!(t3_status(&paths, &t3).is_err());
        assert!(import_t3(&paths, &t3, false).is_err());
    }

    // --- reenable: fake T3 thread reacting to settings writes with cache files.

    const RE_SETTINGS: &str = r##"{
  "favorites": [],
  "providerHealthRefreshInterval": 300000,
  "customTop": {
    "keep": [
      1,
      2,
      3
    ]
  },
  "providerInstances": {
    "broken": {
      "driver": "codex",
      "displayName": "Broken",
      "enabled": true,
      "accentColor": "#abcdef",
      "config": {
        "binaryPath": "codex",
        "homePath": "/main/.codex",
        "shadowHomePath": "/broken/.codex"
      },
      "environment": [
        {
          "name": "S",
          "value": "x",
          "sensitive": true
        }
      ]
    },
    "other": {
      "driver": "claudeAgent",
      "displayName": "Other",
      "enabled": true,
      "config": {
        "binaryPath": "claude",
        "homePath": "/other/.claude"
      }
    }
  }
}
"##;

    #[derive(Clone, Copy)]
    enum FakeBehavior {
        Recover,
        Fail,
        Silent,
    }

    /// Fake T3: watches settings.json; on an `enabled` transition of `id`,
    /// writes a disabled cache for false, and per behavior a ready / error
    /// cache (or nothing) for true. First observation never writes.
    fn spawn_fake(
        t3: PathBuf,
        id: &str,
        behavior: FakeBehavior,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> std::thread::JoinHandle<()> {
        use std::sync::atomic::Ordering;
        let (t3, id) = (t3, id.to_string());
        std::thread::spawn(move || {
            let mut n: u64 = 0;
            let mut last: Option<bool> = None;
            while !stop.load(Ordering::SeqCst) {
                let enabled = fs::read(t3.join("userdata/settings.json"))
                    .ok()
                    .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
                    .and_then(|v| {
                        v.get("providerInstances")?
                            .get(&id)?
                            .get("enabled")?
                            .as_bool()
                    });
                if let Some(en) = enabled {
                    match last {
                        None => last = Some(en),
                        Some(prev) if prev != en => {
                            last = Some(en);
                            n += 1;
                            let body = if !en {
                                format!(
                                    r#"{{"enabled": false, "status": "disabled", "checkedAt": "test-{n}", "models": []}}"#
                                )
                            } else {
                                match behavior {
                                    FakeBehavior::Recover => format!(
                                        r#"{{"enabled": true, "status": "ready", "checkedAt": "test-{n}", "models": []}}"#
                                    ),
                                    FakeBehavior::Fail => format!(
                                        r#"{{"enabled": true, "status": "error", "checkedAt": "test-{n}", "message": "boom: auth failed", "models": []}}"#
                                    ),
                                    FakeBehavior::Silent => String::new(),
                                }
                            };
                            if !body.is_empty() {
                                let _ = fs::write(t3.join(format!("caches/{id}.json")), body);
                            }
                        }
                        _ => {}
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        })
    }

    fn re_opts(timeout_secs: u64) -> ReenableOpts {
        ReenableOpts {
            timeout_secs,
            force: false,
            dry_run: false,
        }
    }

    fn stop_fake(
        stop: &std::sync::Arc<std::sync::atomic::AtomicBool>,
        h: std::thread::JoinHandle<()>,
    ) {
        use std::sync::atomic::Ordering;
        stop.store(true, Ordering::SeqCst);
        h.join().unwrap();
    }

    #[test]
    fn reenable_recovered_roundtrip_is_byte_identical() {
        let (paths, t3, _g) = setup("re-ok", RE_SETTINGS);
        let before = fs::read(settings_file(&t3)).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let h = spawn_fake(t3.clone(), "broken", FakeBehavior::Recover, stop.clone());
        let r = reenable(&paths, &t3, "broken", &re_opts(10)).unwrap();
        stop_fake(&stop, h);
        assert_eq!(r.outcome, ReenableOutcome::Recovered);
        assert!(r.final_line.contains("recovered"), "{}", r.final_line);
        assert!(r.events.iter().any(|e| e.contains("enabled=false")));
        assert!(r.events.iter().any(|e| e.contains("enabled=true")));
        // Full toggle restores the file byte-for-byte: nothing else touched.
        assert_eq!(fs::read(settings_file(&t3)).unwrap(), before);
        let v: Value = serde_json::from_slice(&before).unwrap();
        assert_eq!(
            v["providerInstances"]["other"]["config"]["homePath"],
            "/other/.claude"
        );
        assert_eq!(v["customTop"]["keep"], serde_json::json!([1, 2, 3]));
        // Both writes were backed up.
        assert!(fs::read_dir(paths.backup_dir()).unwrap().count() >= 2);
    }

    #[test]
    fn reenable_failed_again_reports_message() {
        let (paths, t3, _g) = setup("re-fail", RE_SETTINGS);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let h = spawn_fake(t3.clone(), "broken", FakeBehavior::Fail, stop.clone());
        let r = reenable(&paths, &t3, "broken", &re_opts(10)).unwrap();
        stop_fake(&stop, h);
        assert_eq!(r.outcome, ReenableOutcome::FailedAgain);
        assert!(r.final_line.contains("failed again"), "{}", r.final_line);
        assert!(
            r.final_line.contains("boom: auth failed"),
            "{}",
            r.final_line
        );
        // Settings still end enabled (the true write landed).
        let v: Value = serde_json::from_slice(&fs::read(settings_file(&t3)).unwrap()).unwrap();
        assert_eq!(
            v["providerInstances"]["broken"]["enabled"],
            Value::Bool(true)
        );
    }

    #[test]
    fn reenable_timeout_when_no_fresh_probe() {
        let (paths, t3, _g) = setup("re-timeout", RE_SETTINGS);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let h = spawn_fake(t3.clone(), "broken", FakeBehavior::Silent, stop.clone());
        let r = reenable(&paths, &t3, "broken", &re_opts(1)).unwrap();
        stop_fake(&stop, h);
        assert_eq!(r.outcome, ReenableOutcome::Timeout);
        assert!(r.final_line.contains("timeout"), "{}", r.final_line);
        assert!(
            r.final_line.contains("settings updated"),
            "{}",
            r.final_line
        );
        let v: Value = serde_json::from_slice(&fs::read(settings_file(&t3)).unwrap()).unwrap();
        assert_eq!(
            v["providerInstances"]["broken"]["enabled"],
            Value::Bool(true)
        );
    }

    #[test]
    fn reenable_refuses_enabled_and_ready_unless_forced() {
        let (paths, t3, _g) = setup("re-refuse", RE_SETTINGS);
        fs::write(
            t3.join("caches/broken.json"),
            r#"{"enabled": true, "status": "ready", "checkedAt": "t0", "models": []}"#,
        )
        .unwrap();
        let before = fs::read(settings_file(&t3)).unwrap();
        let err = reenable(&paths, &t3, "broken", &re_opts(2)).unwrap_err();
        assert!(err.to_string().contains("nothing to do"), "{err}");
        assert_eq!(fs::read(settings_file(&t3)).unwrap(), before);
        // Forced: the toggle runs against the fake and recovers.
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let h = spawn_fake(t3.clone(), "broken", FakeBehavior::Recover, stop.clone());
        let mut opts = re_opts(10);
        opts.force = true;
        let r = reenable(&paths, &t3, "broken", &opts).unwrap();
        stop_fake(&stop, h);
        assert_eq!(r.outcome, ReenableOutcome::Recovered);
    }

    #[test]
    fn reenable_dry_run_and_unknown_id_write_nothing() {
        let (paths, t3, _g) = setup("re-dry", RE_SETTINGS);
        let before = fs::read(settings_file(&t3)).unwrap();
        let mut opts = re_opts(2);
        opts.dry_run = true;
        let r = reenable(&paths, &t3, "broken", &opts).unwrap();
        assert_eq!(r.outcome, ReenableOutcome::DryRun);
        assert!(r.final_line.contains("dry-run"), "{}", r.final_line);
        assert_eq!(fs::read(settings_file(&t3)).unwrap(), before);
        assert!(!paths.backup_dir().exists());
        assert!(reenable(&paths, &t3, "nope", &re_opts(2)).is_err());
        assert!(reenable(&paths, &t3, "nope", &opts).is_err());
    }

    #[test]
    fn home_spelling_and_id_names_do_not_rewrite_settings() {
        let home = std::env::var("HOME").unwrap_or_default();
        let old = serde_json::json!({"providerInstances": {
            "codex": {"driver": "codex", "enabled": true, "config": {"homePath": "", "binaryPath": "codex"}},
            "two": {"driver": "codex", "enabled": true, "config": {"homePath": format!("{home}/.codex")}},
        }});
        let want = |id: &str| {
            serde_json::json!({"driver": "codex", "displayName": id, "enabled": true,
                "config": {"homePath": "~/.codex"}})
        };
        let desired = BTreeMap::from([
            ("codex".to_string(), want("codex")),
            ("two".to_string(), want("two")),
        ]);
        assert_eq!(merge_settings(&old, &desired).unwrap(), old);
    }

    #[test]
    fn read_instance_enabled_reads_settings_not_cache() {
        let (_paths, t3, _g) = setup("enabled", SETTINGS);
        assert_eq!(read_instance_enabled(&t3, "codex").unwrap(), Some(true));
        assert_eq!(read_instance_enabled(&t3, "foreign").unwrap(), Some(false));
        assert_eq!(read_instance_enabled(&t3, "ghost").unwrap(), None);
        assert!(read_instance_enabled(&t3.join("missing"), "codex").is_err());
        assert!(read_instance_enabled(&t3, "../escape").is_err());
    }

    #[test]
    fn read_usage_cache_handles_valid_missing_and_half_written() {
        let (_paths, t3, _g) = setup("usage-cache", SETTINGS);
        assert!(read_usage_cache(&t3, "codex").unwrap().is_none());

        fs::write(
            cache_file(&t3, "codex"),
            r#"{"enabled":true,"status":"ready","usageLimits":{"checkedAt":"2026-11-13T10:00:00.000Z","windows":[{"id":"primary","kind":"session","label":"Session","usedPercent":12.5},{"id":"custom","kind":"other","label":"Custom","windowDurationMins":60,"usedPercent":40,"resetsAt":"2026-11-13T11:00:00Z"}]}}"#,
        )
        .unwrap();
        let cache = read_usage_cache(&t3, "codex").unwrap().unwrap();
        assert_eq!(cache.status.as_deref(), Some("ready"));
        let limits = cache.usage_limits.unwrap();
        assert_eq!(limits.windows.len(), 2);
        assert_eq!(limits.windows[0].used_percent, Some(12.5));
        assert_eq!(limits.windows[0].window_duration_mins, None);
        assert_eq!(limits.windows[0].resets_at, None);

        fs::write(cache_file(&t3, "broken"), "{\"status\":\"ready\",\"usage").unwrap();
        assert!(read_usage_cache(&t3, "broken").is_err());
    }
}
