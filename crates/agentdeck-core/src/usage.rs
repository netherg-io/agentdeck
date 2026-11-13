//! `[[usage]]` quota sources in `instances.toml`: stable usage ids with
//! instance bindings or API-key env-var names (never secrets).
//!
//! ```toml
//! [[usage]]
//! id = "codex-main"       # EXACT old ai-usage account id, kept byte-verbatim
//! kind = "codex"
//! instance = "codex-main" # registry instance id (required for claude/codex/antigravity)
//! display = "Codex main"
//! ```

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::changes::{Change, plan_write_file};
use crate::paths::Paths;
use crate::registry::{Registry, check_id};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UsageKind {
    Claude,
    Codex,
    Antigravity,
    Deepseek,
    Openrouter,
    Vercel,
    #[serde(rename = "opencode-go")]
    OpencodeGo,
}

impl UsageKind {
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        match s {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "antigravity" => Ok(Self::Antigravity),
            "deepseek" => Ok(Self::Deepseek),
            "openrouter" => Ok(Self::Openrouter),
            "vercel" => Ok(Self::Vercel),
            "opencode-go" => Ok(Self::OpencodeGo),
            other => anyhow::bail!(
                "bad usage kind {other:?}: expected claude, codex, antigravity, deepseek, openrouter, vercel or opencode-go"
            ),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Antigravity => "antigravity",
            Self::Deepseek => "deepseek",
            Self::Openrouter => "openrouter",
            Self::Vercel => "vercel",
            Self::OpencodeGo => "opencode-go",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageEntry {
    pub id: String,
    pub kind: UsageKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    #[serde(default)]
    pub display: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub db_file: Option<String>,
    /// Unknown keys are preserved verbatim so a rewrite never loses them.
    #[serde(flatten, default)]
    pub extra: toml::map::Map<String, toml::Value>,
}

impl UsageEntry {
    pub(crate) fn validate(
        &mut self,
        instances: &[crate::registry::Instance],
    ) -> anyhow::Result<()> {
        check_id(&self.id)?;
        if let Some(inst) = &self.instance
            && instances.iter().all(|i| &i.id != inst)
        {
            anyhow::bail!("usage {:?} refers to unknown instance {:?}", self.id, inst);
        }
        if self.display.is_empty() {
            self.display.clone_from(&self.id);
        }
        let set = |v: &Option<String>| v.as_ref().is_some_and(|s| !s.is_empty());
        let req = |field: &str, v: &Option<String>| -> anyhow::Result<()> {
            if !set(v) {
                anyhow::bail!(
                    "usage {:?} (kind {}): missing {field:?}",
                    self.id,
                    self.kind.as_str()
                );
            }
            Ok(())
        };
        let no = |field: &str, v: &Option<String>| -> anyhow::Result<()> {
            if set(v) {
                anyhow::bail!(
                    "usage {:?} (kind {}): field {field:?} does not apply",
                    self.id,
                    self.kind.as_str()
                );
            }
            Ok(())
        };
        match self.kind {
            UsageKind::Claude | UsageKind::Codex | UsageKind::Antigravity => {
                req("instance", &self.instance)?;
                no("api_key_env", &self.api_key_env)?;
                no("db_file", &self.db_file)?;
            }
            UsageKind::Deepseek | UsageKind::Openrouter | UsageKind::Vercel => {
                // Keys live in the 0600 env file; the schema only holds the var name.
                req("api_key_env", &self.api_key_env)?;
                no("db_file", &self.db_file)?;
            }
            UsageKind::OpencodeGo => {
                no("api_key_env", &self.api_key_env)?;
            }
        }
        Ok(())
    }

    pub fn describe(&self) -> String {
        let mut s = format!("{} {}", self.id, self.kind.as_str());
        if let Some(i) = &self.instance {
            s.push_str(&format!(" instance={i}"));
        }
        s
    }
}

pub(crate) fn validate_all(
    usage: &mut [UsageEntry],
    instances: &[crate::registry::Instance],
) -> anyhow::Result<()> {
    let mut seen = BTreeSet::new();
    for u in usage.iter_mut() {
        u.validate(instances)?;
        if !seen.insert(u.id.clone()) {
            anyhow::bail!("duplicate usage id {:?}", u.id);
        }
    }
    Ok(())
}

impl Registry {
    pub fn usage(&self) -> &[UsageEntry] {
        &self.usage
    }

    pub fn add_usage(&mut self, entry: UsageEntry) -> anyhow::Result<()> {
        check_id(&entry.id)?;
        if self.usage.iter().any(|u| u.id == entry.id) {
            anyhow::bail!("duplicate usage id {:?}", entry.id);
        }
        self.usage.push(entry);
        self.validate()
    }

    pub fn remove_usage(&mut self, id: &str) -> anyhow::Result<UsageEntry> {
        let pos = self
            .usage
            .iter()
            .position(|u| u.id == id)
            .ok_or_else(|| anyhow::anyhow!("unknown usage id {id:?}"))?;
        Ok(self.usage.remove(pos))
    }
}

pub fn list_usage(paths: &Paths) -> anyhow::Result<Vec<UsageEntry>> {
    Ok(Registry::load(paths)?.usage)
}

pub fn usage_json(paths: &Paths) -> anyhow::Result<String> {
    Ok(serde_json::to_string_pretty(&list_usage(paths)?)?)
}

fn preview_usage(
    paths: &Paths,
    mutate: impl FnOnce(&mut Registry) -> anyhow::Result<()>,
) -> anyhow::Result<Vec<Change>> {
    let mut reg = Registry::load(paths)?;
    mutate(&mut reg)?;
    reg.validate()?;
    let new_text = reg.render()?;
    let old = std::fs::read(paths.registry_file()).ok();
    let changes = plan_write_file(paths.registry_file(), old.as_deref(), new_text.as_bytes())
        .map(|c| vec![c])
        .unwrap_or_default();
    Ok(changes)
}

pub fn add_usage_entry(
    paths: &Paths,
    entry: UsageEntry,
    dry_run: bool,
) -> anyhow::Result<Vec<Change>> {
    let changes = preview_usage(paths, |reg| {
        reg.add_usage(entry.clone())?;
        Ok(())
    })?;
    if changes.is_empty() {
        anyhow::bail!("usage {:?} already exists", entry.id);
    }
    if !dry_run {
        Registry::update(paths, |reg| {
            reg.add_usage(entry)?;
            Ok(())
        })?;
    }
    Ok(changes)
}

pub fn remove_usage_entry(paths: &Paths, id: &str, dry_run: bool) -> anyhow::Result<Vec<Change>> {
    Registry::load(paths)?
        .usage
        .iter()
        .find(|u| u.id == id)
        .ok_or_else(|| anyhow::anyhow!("unknown usage id {id:?}"))?;
    let changes = preview_usage(paths, |reg| {
        reg.remove_usage(id)?;
        Ok(())
    })?;
    if !dry_run {
        Registry::update(paths, |reg| {
            reg.remove_usage(id)?;
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

    static N: AtomicU64 = AtomicU64::new(0);

    struct Guard(std::path::PathBuf);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn test_paths(tag: &str) -> (Paths, Guard) {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-usage-{tag}-{}-{}",
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

    fn entry(id: &str, kind: UsageKind) -> UsageEntry {
        let mut e = UsageEntry {
            id: id.to_string(),
            kind,
            instance: None,
            display: String::new(),
            api_key_env: None,
            db_file: None,
            extra: Default::default(),
        };
        if matches!(
            kind,
            UsageKind::Deepseek | UsageKind::Openrouter | UsageKind::Vercel
        ) {
            e.api_key_env = Some("AGENTDECK_TEST_KEY".into());
        }
        e
    }

    #[test]
    fn round_trip_and_display_default() {
        let (paths, _g) = test_paths("roundtrip");
        Registry::update(&paths, |r| {
            r.add(spec("claude-main"))?;
            let mut e = entry("claude-main", UsageKind::Claude);
            e.instance = Some("claude-main".into());
            r.add_usage(e)?;
            r.add_usage(entry("openrouter-main", UsageKind::Openrouter))?;
            Ok(())
        })
        .unwrap();
        let raw = fs::read_to_string(paths.registry_file()).unwrap();
        assert!(raw.contains("[[usage]]"), "{raw}");
        let reg = Registry::load(&paths).unwrap();
        assert_eq!(reg.usage.len(), 2);
        // empty display defaults to the id on load
        assert_eq!(reg.usage[0].display, "claude-main");
    }

    #[test]
    fn rejects_unknown_kind_and_bad_fields() {
        assert!(
            Registry::from_toml("version = 1\n[[usage]]\nid = \"a\"\nkind = \"nope\"\n").is_err()
        );
        // subscription kinds need an instance (opencode-go defaults to "opencode")
        for kind in ["claude", "codex", "antigravity"] {
            let err = Registry::from_toml(&format!(
                "version = 1\n[[usage]]\nid = \"a\"\nkind = \"{kind}\"\n"
            ))
            .unwrap_err()
            .to_string();
            assert!(err.contains("missing \"instance\""), "{kind}: {err}");
        }
        assert!(
            Registry::from_toml("version = 1\n[[usage]]\nid = \"a\"\nkind = \"opencode-go\"\n")
                .is_ok()
        );
        // api_key_env on a subscription entry is rejected
        assert!(
            Registry::from_toml(
                "version = 1\n[[instance]]\nid = \"c\"\nharness = \"claude\"\nhome = \"~/.claude\"\n\
                 [[usage]]\nid = \"a\"\nkind = \"claude\"\ninstance = \"c\"\napi_key_env = \"E\"\n"
            )
            .is_err()
        );
        // deepseek needs a key pointer
        assert!(
            Registry::from_toml("version = 1\n[[usage]]\nid = \"a\"\nkind = \"deepseek\"\n")
                .is_err()
        );
        // stale legacy fields (config_dir, codex_home, command, limit_id, token_file, api_key_file)
        // stay in `extra` and load cleanly without error
        let reg = Registry::from_toml(
            "version = 1\n\
             [[instance]]\nid = \"c\"\nharness = \"claude\"\nhome = \"~/.claude\"\n\
             [[instance]]\nid = \"x\"\nharness = \"codex\"\nhome = \"~/.codex\"\n\
             [[instance]]\nid = \"g\"\nharness = \"agy\"\nhome = \"~/.gemini\"\n\
             [[usage]]\nid = \"c1\"\nkind = \"claude\"\ninstance = \"c\"\nconfig_dir = \"~/.claude\"\n\
             [[usage]]\nid = \"x1\"\nkind = \"codex\"\ninstance = \"x\"\ncodex_home = \"~/.codex\"\ncommand = \"codex\"\nlimit_id = \"codex\"\n\
             [[usage]]\nid = \"g1\"\nkind = \"antigravity\"\ninstance = \"g\"\ntoken_file = \"/etc/acp.json\"\n\
             [[usage]]\nid = \"d1\"\nkind = \"deepseek\"\napi_key_env = \"E\"\napi_key_file = \"f\"\n",
        )
        .unwrap();
        assert_eq!(reg.usage.len(), 4);
        assert_eq!(
            reg.usage[0].extra.get("config_dir").and_then(|v| v.as_str()),
            Some("~/.claude")
        );
        assert_eq!(
            reg.usage[1].extra.get("codex_home").and_then(|v| v.as_str()),
            Some("~/.codex")
        );
        assert_eq!(
            reg.usage[2].extra.get("token_file").and_then(|v| v.as_str()),
            Some("/etc/acp.json")
        );
        assert_eq!(
            reg.usage[3].extra.get("api_key_file").and_then(|v| v.as_str()),
            Some("f")
        );
        // duplicate usage ids
        assert!(
            Registry::from_toml(
                "version = 1\n\
                 [[instance]]\nid = \"x\"\nharness = \"codex\"\nhome = \"~/.codex\"\n\
                 [[usage]]\nid = \"a\"\nkind = \"codex\"\ninstance = \"x\"\n\
                 [[usage]]\nid = \"a\"\nkind = \"codex\"\ninstance = \"x\"\n"
            )
            .is_err()
        );
        // instance must exist
        assert!(
            Registry::from_toml(
                "version = 1\n[[usage]]\nid = \"a\"\nkind = \"codex\"\ninstance = \"ghost\"\n"
            )
            .is_err()
        );
    }

    #[test]
    fn instance_removal_drops_its_usage_entries() {
        let (paths, _g) = test_paths("cascade");
        Registry::update(&paths, |r| {
            r.add(spec("a"))?;
            r.add(spec("b"))?;
            let mut e = entry("u", UsageKind::Claude);
            e.instance = Some("a".into());
            r.add_usage(e)?;
            let mut e = entry("v", UsageKind::Claude);
            e.instance = Some("b".into());
            r.add_usage(e)?;
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
        let left: Vec<&str> = reg.usage.iter().map(|u| u.id.as_str()).collect();
        assert_eq!(left, ["v"]);
    }

    #[test]
    fn dry_run_writes_nothing() {
        let (paths, _g) = test_paths("dry");
        Registry::update(&paths, |r| r.add(spec("a"))).unwrap();
        let before = fs::read(paths.registry_file()).unwrap();
        let changes = add_usage_entry(&paths, entry("u", UsageKind::OpencodeGo), true).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(fs::read(paths.registry_file()).unwrap(), before);
        let changes = remove_usage_entry(&paths, "missing", true)
            .unwrap_err()
            .to_string();
        assert!(changes.contains("unknown usage id"), "{changes}");
    }
}
