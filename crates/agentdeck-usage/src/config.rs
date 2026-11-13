use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use agentdeck_core::usage::{UsageEntry, UsageKind};

/// Minimum interval between balance polls and T3 cache reads.
pub const MIN_REFRESH_SECONDS: u64 = 15;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_refresh_seconds")]
    pub refresh_seconds: u64,
    #[serde(default = "default_stale_seconds")]
    pub stale_seconds: i64,
    #[serde(default)]
    pub accounts: Vec<AccountConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            refresh_seconds: default_refresh_seconds(),
            stale_seconds: default_stale_seconds(),
            accounts: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum AccountConfig {
    Claude {
        id: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instance: Option<String>,
        plan: Option<String>,
    },
    Codex {
        id: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instance: Option<String>,
    },
    Deepseek {
        id: String,
        name: String,
        api_key_env: String,
        #[serde(default = "default_deepseek_base_url")]
        base_url: String,
    },
    Openrouter {
        id: String,
        name: String,
        api_key_env: String,
        #[serde(default = "default_openrouter_base_url")]
        base_url: String,
    },
    /// Vercel AI Gateway credits.
    Vercel {
        id: String,
        name: String,
        api_key_env: String,
        #[serde(default = "default_vercel_base_url")]
        base_url: String,
    },
    Antigravity {
        id: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instance: Option<String>,
    },
    #[serde(rename = "opencode-go")]
    OpencodeGo {
        id: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instance: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        db_file: Option<PathBuf>,
    },
}

impl AccountConfig {
    pub fn id(&self) -> &str {
        match self {
            Self::Claude { id, .. }
            | Self::Codex { id, .. }
            | Self::Deepseek { id, .. }
            | Self::Openrouter { id, .. }
            | Self::Vercel { id, .. }
            | Self::Antigravity { id, .. }
            | Self::OpencodeGo { id, .. } => id,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Claude { name, .. }
            | Self::Codex { name, .. }
            | Self::Deepseek { name, .. }
            | Self::Openrouter { name, .. }
            | Self::Vercel { name, .. }
            | Self::Antigravity { name, .. }
            | Self::OpencodeGo { name, .. } => name,
        }
    }

    pub fn provider(&self) -> &'static str {
        match self {
            Self::Claude { .. } => "claude",
            Self::Codex { .. } => "codex",
            Self::Deepseek { .. } => "deepseek",
            Self::Openrouter { .. } => "openrouter",
            Self::Vercel { .. } => "vercel",
            Self::Antigravity { .. } => "antigravity",
            Self::OpencodeGo { .. } => "opencode-go",
        }
    }

    pub fn t3_instance(&self) -> Option<String> {
        let instance = match self {
            Self::Claude { instance, .. }
            | Self::Codex { instance, .. }
            | Self::Antigravity { instance, .. }
            | Self::OpencodeGo { instance, .. } => instance.as_deref(),
            Self::Deepseek { .. } | Self::Openrouter { .. } | Self::Vercel { .. } => return None,
        };
        crate::providers::t3::resolve_instance_slug(self.provider(), instance)
    }
}

fn default_refresh_seconds() -> u64 {
    120
}

fn default_stale_seconds() -> i64 {
    crate::providers::claude::DEFAULT_STALE_SECONDS
}

pub(crate) fn default_deepseek_base_url() -> String {
    "https://api.deepseek.com".to_owned()
}

pub(crate) fn default_vercel_base_url() -> String {
    "https://ai-gateway.vercel.sh/v1".to_owned()
}

fn default_openrouter_base_url() -> String {
    "https://openrouter.ai/api/v1".to_owned()
}

/// Parse an old `config.toml` from explicit bytes: the migrator reads a
/// fixture or an injected HOME instead of the production path.
pub fn parse_str(raw: &str, what: &Path) -> Result<Config> {
    let config: Config =
        toml::from_str(raw).with_context(|| format!("Некорректный TOML в {}", what.display()))?;
    validate(&config).with_context(|| format!("Некорректная конфигурация в {}", what.display()))?;
    Ok(config)
}

/// Проверяет инварианты, которые нельзя выразить типами: уникальность ID и
/// уникальность привязки к инстансам T3.
pub fn validate(config: &Config) -> Result<()> {
    let mut seen_ids: HashMap<&str, ()> = HashMap::new();
    let mut seen_instances: HashMap<String, &str> = HashMap::new();

    for account in &config.accounts {
        crate::util::validate_id(account.id())
            .with_context(|| format!("Некорректный ID аккаунта '{}'", account.id()))?;

        if seen_ids.insert(account.id(), ()).is_some() {
            bail!("Дублирующийся ID аккаунта: '{}'", account.id());
        }

        if let Some(instance) = account.t3_instance()
            && let Some(previous) = seen_instances.insert(instance.clone(), account.id())
        {
            bail!(
                "Источники '{}' и '{}' указывают на один инстанс T3 '{}'",
                previous,
                account.id(),
                instance
            );
        }
    }

    if config.stale_seconds <= 0 {
        bail!("stale_seconds должен быть положительным");
    }

    Ok(())
}

/// Build the provider account list from validated `[[usage]]` registry entries.
/// Reuses [`validate`] so duplicate ids and duplicate T3 instances are rejected.
pub fn accounts_from_registry(entries: &[UsageEntry]) -> Result<Vec<AccountConfig>> {
    let mut accounts = Vec::with_capacity(entries.len());
    for e in entries {
        let name = if e.display.is_empty() {
            e.id.clone()
        } else {
            e.display.clone()
        };
        accounts.push(match e.kind {
            UsageKind::Claude => AccountConfig::Claude {
                id: e.id.clone(),
                name,
                instance: e.instance.clone(),
                plan: None,
            },
            UsageKind::Codex => AccountConfig::Codex {
                id: e.id.clone(),
                name,
                instance: e.instance.clone(),
            },
            UsageKind::Deepseek => AccountConfig::Deepseek {
                id: e.id.clone(),
                name,
                api_key_env: key_env(e)?,
                base_url: default_deepseek_base_url(),
            },
            UsageKind::Openrouter => AccountConfig::Openrouter {
                id: e.id.clone(),
                name,
                api_key_env: key_env(e)?,
                base_url: default_openrouter_base_url(),
            },
            UsageKind::Vercel => AccountConfig::Vercel {
                id: e.id.clone(),
                name,
                api_key_env: key_env(e)?,
                base_url: default_vercel_base_url(),
            },
            UsageKind::Antigravity => AccountConfig::Antigravity {
                id: e.id.clone(),
                name,
                instance: e.instance.clone(),
            },
            UsageKind::OpencodeGo => AccountConfig::OpencodeGo {
                id: e.id.clone(),
                name,
                instance: e.instance.clone(),
                db_file: e.db_file.as_ref().map(PathBuf::from),
            },
        });
    }
    let config = Config {
        accounts,
        ..Config::default()
    };
    validate(&config)?;
    Ok(config.accounts)
}

/// Env-var pointer for key-based kinds. Only `api_key_env` is supported:
/// keys live in the 0600 env file, never inline.
fn key_env(e: &UsageEntry) -> Result<String> {
    e.api_key_env.clone().with_context(|| {
        format!(
            "usage {:?} (kind {}): missing \"api_key_env\"",
            e.id,
            e.kind.as_str()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn claude(id: &str, instance: Option<&str>) -> AccountConfig {
        AccountConfig::Claude {
            id: id.to_owned(),
            name: id.to_owned(),
            instance: instance.map(str::to_owned),
            plan: None,
        }
    }

    fn codex(id: &str, instance: Option<&str>) -> AccountConfig {
        AccountConfig::Codex {
            id: id.to_owned(),
            name: id.to_owned(),
            instance: instance.map(str::to_owned),
        }
    }

    fn antigravity(id: &str, instance: Option<&str>) -> AccountConfig {
        AccountConfig::Antigravity {
            id: id.to_owned(),
            name: id.to_owned(),
            instance: instance.map(str::to_owned),
        }
    }

    #[test]
    fn round_trips_every_provider_through_toml() {
        let config = Config {
            refresh_seconds: 60,
            stale_seconds: 3_600,
            accounts: vec![
                claude("claude-main", Some("claude-main")),
                codex("codex-main", Some("codex")),
                AccountConfig::Deepseek {
                    id: "deepseek-main".to_owned(),
                    name: "DeepSeek".to_owned(),
                    api_key_env: "AI_USAGE_DEEPSEEK_DEEPSEEK_MAIN".to_owned(),
                    base_url: "https://api.deepseek.com".to_owned(),
                },
                AccountConfig::Openrouter {
                    id: "openrouter-main".to_owned(),
                    name: "OpenRouter".to_owned(),
                    api_key_env: "AI_USAGE_OPENROUTER_OPENROUTER_MAIN".to_owned(),
                    base_url: "https://openrouter.ai/api/v1".to_owned(),
                },
                antigravity("agy-main", Some("antigravity")),
            ],
        };

        let raw = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&raw).unwrap();

        assert_eq!(parsed.accounts.len(), 5);
        assert_eq!(parsed.refresh_seconds, 60);
        assert_eq!(parsed.stale_seconds, 3_600);
        assert_eq!(
            parsed
                .accounts
                .iter()
                .map(AccountConfig::provider)
                .collect::<Vec<_>>(),
            ["claude", "codex", "deepseek", "openrouter", "antigravity"]
        );
        validate(&parsed).unwrap();
    }

    #[test]
    fn saves_and_loads_round_trip() {
        let unique = format!(
            "ai-usage-test-{}-{:?}.toml",
            std::process::id(),
            std::thread::current().id()
        );
        let temp = std::env::temp_dir().join(unique);

        let config = Config {
            refresh_seconds: 30,
            stale_seconds: 1_800,
            accounts: vec![
                claude("primary", Some("claude-1")),
                claude("secondary", Some("claude-2")),
            ],
        };

        crate::util::atomic_write(&temp, toml::to_string_pretty(&config).unwrap().as_bytes())
            .unwrap();

        let raw = fs::read_to_string(&temp).unwrap();
        let loaded: Config = toml::from_str(&raw).unwrap();
        validate(&loaded).unwrap();

        assert_eq!(loaded.accounts.len(), 2);
        assert_eq!(loaded.accounts[0].id(), "primary");
        assert_eq!(loaded.accounts[1].id(), "secondary");

        let _ = fs::remove_file(temp);
    }

    #[test]
    fn rejects_duplicate_account_ids_across_providers() {
        let config = Config {
            accounts: vec![claude("same", Some("c")), codex("same", Some("x"))],
            ..Config::default()
        };
        let error = validate(&config).unwrap_err().to_string();
        assert!(error.contains("Дублирующийся ID"), "{error}");
    }

    #[test]
    fn old_shape_config_with_legacy_fields_loads_without_error() {
        let old_toml = r#"
refresh_seconds = 120
stale_seconds = 3600

[[accounts]]
provider = "claude"
id = "claude-main"
name = "Claude"
instance = "claudeAgent"
config_dir = "~/.claude"

[[accounts]]
provider = "codex"
id = "codex-main"
name = "Codex"
instance = "codex"
codex_home = "~/.codex"
command = "codex"
limit_id = "codex"

[[accounts]]
provider = "antigravity"
id = "agy-main"
name = "Google AI Pro"
instance = "antigravity"
token_file = "/etc/acp.json"
"#;
        let parsed = parse_str(old_toml, Path::new("old-config.toml")).unwrap();
        assert_eq!(parsed.accounts.len(), 3);
        assert_eq!(
            parsed.accounts[0].t3_instance().as_deref(),
            Some("claudeAgent")
        );
        assert_eq!(parsed.accounts[1].t3_instance().as_deref(), Some("codex"));
        assert_eq!(
            parsed.accounts[2].t3_instance().as_deref(),
            Some("antigravity")
        );
    }

    #[test]
    fn rejects_ids_that_would_escape_the_cache_directory() {
        let config = Config {
            accounts: vec![claude("../../etc", Some("claude"))],
            ..Config::default()
        };
        assert!(validate(&config).is_err());
    }

    #[test]
    fn rejects_non_positive_stale_seconds() {
        let config = Config {
            stale_seconds: 0,
            ..Config::default()
        };
        assert!(validate(&config).is_err());
    }

    #[test]
    fn registry_entries_match_old_config_accounts() {
        let old_toml = r#"
refresh_seconds = 120
stale_seconds = 3600

[[accounts]]
provider = "claude"
id = "claude-main"
name = "Claude"
instance = "claude-main"
config_dir = "~/.claude"

[[accounts]]
provider = "codex"
id = "codex-main"
name = "Codex Personal"
instance = "codex-main"
codex_home = "~/.codex"
command = "codex"
limit_id = "codex"

[[accounts]]
provider = "deepseek"
id = "deepseek-main"
name = "DeepSeek"
api_key_env = "AGENTDECK_DEEPSEEK_MAIN"
base_url = "https://api.deepseek.com"

[[accounts]]
provider = "openrouter"
id = "openrouter-main"
name = "OpenRouter"
api_key_env = "AGENTDECK_OPENROUTER_MAIN"
base_url = "https://openrouter.ai/api/v1"

[[accounts]]
provider = "antigravity"
id = "agy-main"
name = "Google AI Pro"
instance = "agy-main"

[[accounts]]
provider = "antigravity"
id = "agy-file"
name = "Custom ACP"
instance = "agy-file"
token_file = "/etc/acp.json"

[[accounts]]
provider = "opencode-go"
id = "opencode-go"
name = "OpenCode Go"
db_file = "/data/opencode.db"
"#;
        let reg_toml = r#"
version = 1

[[instance]]
id = "claude-main"
harness = "claude"
home = "~/.claude"

[[instance]]
id = "codex-main"
harness = "codex"
home = "~/.codex"

[[instance]]
id = "agy-main"
harness = "agy"
home = "~/.gemini"

[[instance]]
id = "agy-file"
harness = "agy"
home = "~/.gemini-alt"

[[usage]]
id = "claude-main"
kind = "claude"
display = "Claude"
instance = "claude-main"

[[usage]]
id = "codex-main"
kind = "codex"
display = "Codex Personal"
instance = "codex-main"

[[usage]]
id = "deepseek-main"
kind = "deepseek"
display = "DeepSeek"
api_key_env = "AGENTDECK_DEEPSEEK_MAIN"

[[usage]]
id = "openrouter-main"
kind = "openrouter"
display = "OpenRouter"
api_key_env = "AGENTDECK_OPENROUTER_MAIN"

[[usage]]
id = "agy-main"
kind = "antigravity"
display = "Google AI Pro"
instance = "agy-main"

[[usage]]
id = "agy-file"
kind = "antigravity"
display = "Custom ACP"
instance = "agy-file"

[[usage]]
id = "opencode-go"
kind = "opencode-go"
display = "OpenCode Go"
db_file = "/data/opencode.db"
"#;
        let old: Config = toml::from_str(old_toml).unwrap();
        validate(&old).unwrap();
        let reg = agentdeck_core::Registry::from_toml(reg_toml).unwrap();
        assert_eq!(accounts_from_registry(&reg.usage).unwrap(), old.accounts);
    }

    #[test]
    fn registry_adapter_propagates_instance() {
        let reg = agentdeck_core::Registry::from_toml(
            "version = 1\n\
             [[instance]]\nid = \"claude-main\"\nharness = \"claude\"\nhome = \"~/.claude\"\n\
             [[usage]]\nid = \"claude-main\"\nkind = \"claude\"\ninstance = \"claude-main\"\n\
             [[usage]]\nid = \"opencode-go\"\nkind = \"opencode-go\"\n",
        )
        .unwrap();
        let accounts = accounts_from_registry(&reg.usage).unwrap();
        assert!(matches!(
            &accounts[0],
            AccountConfig::Claude { instance: Some(i), .. } if i == "claude-main"
        ));
        assert!(matches!(
            &accounts[1],
            AccountConfig::OpencodeGo { instance: None, .. }
        ));
    }

    #[test]
    fn rejects_two_usage_sources_pointing_at_same_t3_instance() {
        let reg = agentdeck_core::Registry::from_toml(
            "version = 1\n\
             [[instance]]\nid = \"claude-main\"\nharness = \"claude\"\nhome = \"~/.claude\"\n\
             [[usage]]\nid = \"one\"\nkind = \"claude\"\ninstance = \"claude-main\"\n\
             [[usage]]\nid = \"two\"\nkind = \"claude\"\ninstance = \"claude-main\"\n",
        )
        .unwrap();
        let err = accounts_from_registry(&reg.usage).unwrap_err().to_string();
        assert!(
            err.contains("one") && err.contains("two") && err.contains("claude-main"),
            "{err}"
        );

        let go_reg = agentdeck_core::Registry::from_toml(
            "version = 1\n\
             [[instance]]\nid = \"opencode\"\nharness = \"opencode\"\nhome = \"~\"\n\
             [[usage]]\nid = \"go-a\"\nkind = \"opencode-go\"\n\
             [[usage]]\nid = \"go-b\"\nkind = \"opencode-go\"\ninstance = \"opencode\"\n",
        )
        .unwrap();
        let err = accounts_from_registry(&go_reg.usage)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("go-a") && err.contains("go-b") && err.contains("opencode"),
            "{err}"
        );
    }
}
