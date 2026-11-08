//! `[[usage]]` quota sources in `instances.toml`: stable usage ids with
//! kind-specific source pointers (paths and env-var names only, never secrets).
//!
//! ```toml
//! [[usage]]
//! id = "codex-main"       # EXACT old ai-usage account id, kept byte-verbatim
//! kind = "codex"
//! instance = "codex-main" # optional registry instance id (absent for standalone kinds)
//! display = "Codex main"
//! codex_home = "~/.codex"
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
    pub config_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_home: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_file: Option<String>,
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
            UsageKind::Claude => {
                req("config_dir", &self.config_dir)?;
                no("codex_home", &self.codex_home)?;
                no("token_file", &self.token_file)?;
                no("api_key_env", &self.api_key_env)?;
                no("db_file", &self.db_file)?;
            }
            UsageKind::Codex => {
                req("codex_home", &self.codex_home)?;
                no("config_dir", &self.config_dir)?;
                no("token_file", &self.token_file)?;
                no("api_key_env", &self.api_key_env)?;
                no("db_file", &self.db_file)?;
            }
            UsageKind::Antigravity => {
                no("config_dir", &self.config_dir)?;
                no("codex_home", &self.codex_home)?;
                no("api_key_env", &self.api_key_env)?;
                no("db_file", &self.db_file)?;
            }
            UsageKind::Deepseek | UsageKind::Openrouter | UsageKind::Vercel => {
                // Keys live in the 0600 env file; the schema only holds the var name.
                req("api_key_env", &self.api_key_env)?;
                no("config_dir", &self.config_dir)?;
                no("codex_home", &self.codex_home)?;
                no("token_file", &self.token_file)?;
                no("db_file", &self.db_file)?;
            }
            UsageKind::OpencodeGo => {
                no("config_dir", &self.config_dir)?;
                no("codex_home", &self.codex_home)?;
                no("token_file", &self.token_file)?;
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
            config_dir: None,
            codex_home: None,
            token_file: None,
            api_key_env: None,
            db_file: None,
            extra: Default::default(),
        };
        match kind {
            UsageKind::Claude => e.config_dir = Some("~/.claude".into()),
            UsageKind::Codex => e.codex_home = Some("~/.codex".into()),
            UsageKind::Deepseek | UsageKind::Openrouter | UsageKind::Vercel => {
                e.api_key_env = Some("AGENTDECK_TEST_KEY".into());
            }
            UsageKind::Antigravity | UsageKind::OpencodeGo => {}
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
        // claude without config_dir
        assert!(
            Registry::from_toml("version = 1\n[[usage]]\nid = \"a\"\nkind = \"claude\"\n").is_err()
        );
        // codex pointer on a claude entry
        assert!(
            Registry::from_toml(
                "version = 1\n[[usage]]\nid = \"a\"\nkind = \"claude\"\nconfig_dir = \"x\"\ncodex_home = \"y\"\n"
            )
            .is_err()
        );
        // deepseek needs a key pointer
        assert!(
            Registry::from_toml("version = 1\n[[usage]]\nid = \"a\"\nkind = \"deepseek\"\n")
                .is_err()
        );
        // a stale api_key_file key is preserved verbatim, not rejected
        let reg = Registry::from_toml(
            "version = 1\n[[usage]]\nid = \"a\"\nkind = \"deepseek\"\napi_key_env = \"E\"\napi_key_file = \"f\"\n",
        )
        .unwrap();
        assert_eq!(
            reg.usage[0]
                .extra
                .get("api_key_file")
                .and_then(|v| v.as_str()),
            Some("f")
        );
        // duplicate usage ids
        assert!(
            Registry::from_toml(
                "version = 1\n[[usage]]\nid = \"a\"\nkind = \"codex\"\ncodex_home = \"x\"\n\
                 [[usage]]\nid = \"a\"\nkind = \"codex\"\ncodex_home = \"y\"\n"
            )
            .is_err()
        );
        // instance must exist
        assert!(
            Registry::from_toml(
                "version = 1\n[[usage]]\nid = \"a\"\nkind = \"codex\"\ncodex_home = \"x\"\ninstance = \"ghost\"\n"
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
