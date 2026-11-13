//! `agentdeck migrate ai-usage [--dry-run]`: copy-only, idempotent migration
//! from the old `~/.config/ai-usage` + `~/.local/state/ai-usage` trees.
//! Never modifies or deletes anything in the old tree, never touches services.

use std::fs;
use std::path::{Path, PathBuf};

use agentdeck_core::{
    Change, Harness, Instance, Paths, Registry, UsageEntry, UsageKind, plan_write_file,
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct LegacyConfig {
    #[serde(default)]
    accounts: Vec<LegacyAccount>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase")]
enum LegacyAccount {
    Claude {
        id: String,
        name: String,
        #[serde(default)]
        config_dir: PathBuf,
    },
    Codex {
        id: String,
        name: String,
        #[serde(default)]
        codex_home: PathBuf,
        #[serde(default = "default_codex_command")]
        command: String,
    },
    Deepseek {
        id: String,
        name: String,
        api_key_env: String,
    },
    Openrouter {
        id: String,
        name: String,
        api_key_env: String,
    },
    Vercel {
        id: String,
        name: String,
        api_key_env: String,
    },
    Antigravity {
        id: String,
        name: String,
        #[serde(default)]
        token_file: Option<PathBuf>,
    },
    #[serde(rename = "opencode-go")]
    OpencodeGo {
        id: String,
        name: String,
        #[serde(default)]
        db_file: Option<PathBuf>,
    },
}

fn default_codex_command() -> String {
    "codex".to_owned()
}

impl LegacyAccount {
    fn id(&self) -> &str {
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

    fn name(&self) -> &str {
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

    fn provider(&self) -> &'static str {
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

    fn needs_instance(&self) -> bool {
        matches!(
            self,
            Self::Claude { .. } | Self::Codex { .. } | Self::Antigravity { .. }
        )
    }
}

#[derive(clap::Subcommand)]
pub enum MigrateCmd {
    /// Copy old ai-usage accounts, secrets and state files. Copy-only:
    /// the old tree is never modified or deleted.
    AiUsage {
        /// Print the mapping table and changes without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
}

/// Old-tree locations. Explicit so tests run on fixture dirs; production
/// resolves from the real HOME with XDG overrides honored.
#[derive(Debug, Clone)]
pub struct OldTree {
    pub config_toml: PathBuf,
    pub secrets_env: PathBuf,
    pub state_dir: PathBuf,
}

pub fn old_tree_for(home: &Path) -> OldTree {
    let config_base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    let state_base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/state"));
    OldTree {
        config_toml: config_base.join("ai-usage").join("config.toml"),
        secrets_env: config_base.join("ai-usage").join("secrets.env"),
        state_dir: state_base.join("ai-usage"),
    }
}

#[derive(Debug, Clone)]
pub struct MappingRow {
    pub id: String,
    pub kind: &'static str,
    pub instance: Option<String>,
    /// Path or env-var NAME, never a secret value.
    pub source: String,
    pub note: Option<String>,
}

#[derive(Debug, Default)]
pub struct MigrationPlan {
    pub rows: Vec<MappingRow>,
    pub new_entries: Vec<UsageEntry>,
    pub registry_change: Option<Change>,
    pub file_changes: Vec<Change>,
    /// Destinations left alone: differ from the old copy but are newer.
    pub kept_newer: Vec<PathBuf>,
}

pub fn run(paths: &Paths, cmd: MigrateCmd) -> anyhow::Result<()> {
    match cmd {
        MigrateCmd::AiUsage { dry_run } => {
            let home = std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| {
                anyhow::anyhow!("cannot resolve home: set HOME or pass an explicit tree")
            })?;
            migrate_ai_usage(paths, &old_tree_for(&home), dry_run)?;
        }
    }
    Ok(())
}

pub fn migrate_ai_usage(paths: &Paths, old: &OldTree, dry_run: bool) -> anyhow::Result<()> {
    let plan = plan_migration(paths, old)?;
    println!("migrate ai-usage");
    println!("  old config: {}", old.config_toml.display());
    println!("  old state:  {}", old.state_dir.display());
    for row in &plan.rows {
        let instance = row.instance.as_deref().unwrap_or("-");
        let mut line = format!(
            "  {:<16} {:<12} instance={:<12} {}",
            row.id, row.kind, instance, row.source
        );
        if let Some(note) = &row.note {
            line.push_str(&format!(" [{note}]"));
        }
        println!("{line}");
    }
    match &plan.registry_change {
        Some(change) => println!("  registry: {}", change.describe()),
        None => println!("  registry: unchanged (all ids already present)"),
    }
    for change in &plan.file_changes {
        println!("  file: {}", change.describe());
    }
    for kept in &plan.kept_newer {
        println!("  file: kept newer {}", kept.display());
    }
    println!("  remaining owner steps (owner-run, this command never runs them):");
    println!("  - agentdeck usage doctor   (check sources, backoffs, freshness)");
    println!(
        "  - agentdeck usage once --compat-write-ai-usage   (smoke poll while the old daemon still runs)"
    );
    println!("  - stop the old ai-usage service, then start the new daemon unit");
    println!(
        "  - retire the old tree only after a soak week (copies were made; the old daemon can roll back)"
    );
    if dry_run {
        println!("dry-run: nothing written");
        return Ok(());
    }
    if !plan.new_entries.is_empty() {
        // Backs up instances.toml; validates before writing.
        Registry::update(paths, |reg| {
            for entry in &plan.new_entries {
                reg.add_usage(entry.clone())?;
            }
            Ok(())
        })?;
    }
    agentdeck_core::apply_changes(&plan.file_changes)?;
    println!(
        "migrated {} accounts, {} files",
        plan.new_entries.len(),
        plan.file_changes.len()
    );
    Ok(())
}

pub fn plan_migration(paths: &Paths, old: &OldTree) -> anyhow::Result<MigrationPlan> {
    let raw = fs::read_to_string(&old.config_toml).map_err(|error| {
        anyhow::anyhow!("no old config at {}: {error}", old.config_toml.display())
    })?;
    let config: LegacyConfig = toml::from_str(&raw)
        .map_err(|error| anyhow::anyhow!("invalid TOML in {}: {error}", old.config_toml.display()))?;
    let reg = Registry::load(paths)?;

    let mut plan = MigrationPlan::default();
    for account in &config.accounts {
        let instance = resolve_instance(account, &reg.instances);
        let entry = usage_entry(account, instance.clone())?;
        let already = reg.usage.iter().any(|u| u.id == entry.id);
        let missing_instance = !already && entry.instance.is_none() && account.needs_instance();
        plan.rows.push(MappingRow {
            id: account.id().to_owned(),
            kind: account.provider(),
            instance,
            source: source_pointer(account),
            note: if already {
                Some("already present".to_owned())
            } else if missing_instance {
                Some("no matching instance".to_owned())
            } else {
                None
            },
        });
        if !already && !missing_instance {
            plan.new_entries.push(entry);
        }
    }

    if !plan.new_entries.is_empty() {
        let mut preview = reg.clone();
        for entry in &plan.new_entries {
            preview.add_usage(entry.clone())?;
        }
        let new_text = preview.render()?;
        let old_bytes = fs::read(paths.registry_file()).ok();
        plan.registry_change = plan_write_file(
            paths.registry_file(),
            old_bytes.as_deref(),
            new_text.as_bytes(),
        );
    }

    // secrets.env -> usage.env (0600 via Change::apply for new files).
    // Env names are kept unchanged; only names are ever printed, never values.
    if old.secrets_env.exists() {
        let bytes = fs::read(&old.secrets_env)?;
        let dest = agentdeck_usage::daemon::agentdeck_secrets_file(&paths.config_dir);
        plan_file_copy(&mut plan, &old.secrets_env, &dest, &bytes);
    }

    let new_usage_dir = paths.state_dir.join("usage");
    if let Ok(dir) = fs::read_dir(&old.state_dir) {
        let mut names: Vec<String> = dir
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| is_migrated_state_file(n))
            .collect();
        names.sort();
        for name in names {
            let src = old.state_dir.join(&name);
            let bytes = fs::read(&src)?;
            plan_file_copy(&mut plan, &src, &new_usage_dir.join(&name), &bytes);
        }
    }
    Ok(plan)
}

fn plan_file_copy(plan: &mut MigrationPlan, src: &Path, dest: &Path, bytes: &[u8]) {
    let old_dest = fs::read(dest).ok();
    if old_dest.as_deref() == Some(bytes) {
        return;
    }
    if should_copy(src, dest)
        && let Some(change) = plan_write_file(dest.to_path_buf(), old_dest.as_deref(), bytes)
    {
        plan.file_changes.push(change);
    } else if old_dest.as_deref() != Some(bytes) {
        plan.kept_newer.push(dest.to_path_buf());
    }
}

/// Copy when the destination is missing or older. Never overwrites newer files.
fn should_copy(src: &Path, dest: &Path) -> bool {
    let (Ok(s), Ok(d)) = (fs::metadata(src), fs::metadata(dest)) else {
        return true;
    };
    let (Ok(s), Ok(d)) = (s.modified(), d.modified()) else {
        return true;
    };
    s >= d
}

/// Cooldowns, snapshots, history. `state.json` is regenerated by the daemon
/// and stays out; anything else in the old tree is not ours to copy.
fn is_migrated_state_file(name: &str) -> bool {
    ((name.starts_with("claude-usage-") || name.starts_with("antigravity-usage-"))
        && name.ends_with(".json"))
        || name == "history.jsonl"
        || name == "history.jsonl.1"
}

/// `instance` matches the account's login dir against registry instance homes,
/// or resolves the unique `agy` instance for Antigravity.
fn resolve_instance(account: &LegacyAccount, instances: &[Instance]) -> Option<String> {
    if matches!(account, LegacyAccount::Antigravity { .. }) {
        let mut agy = instances.iter().filter(|i| i.harness == Harness::Agy);
        let first = agy.next()?;
        return agy.next().is_none().then(|| first.id.clone());
    }
    let dir = match account {
        LegacyAccount::Claude { config_dir, .. } => Some(config_dir),
        LegacyAccount::Codex { codex_home, .. } => Some(codex_home),
        _ => None,
    }?;
    let want = agentdeck_usage::util::expand_home(dir);
    instances
        .iter()
        .find(|i| agentdeck_usage::util::expand_home(Path::new(&i.home)) == want)
        .map(|i| i.id.clone())
}

/// Path or env-var NAME, never a secret value.
fn source_pointer(account: &LegacyAccount) -> String {
    match account {
        LegacyAccount::Claude { config_dir, .. } => config_dir.display().to_string(),
        LegacyAccount::Codex {
            codex_home,
            command,
            ..
        } => format!("{command} CODEX_HOME={}", codex_home.display()),
        LegacyAccount::Deepseek { api_key_env, .. } => api_key_env.clone(),
        LegacyAccount::Openrouter { api_key_env, .. }
        | LegacyAccount::Vercel { api_key_env, .. } => api_key_env.clone(),
        LegacyAccount::Antigravity { token_file, .. } => token_file
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "auto-discovery".to_owned()),
        LegacyAccount::OpencodeGo { db_file, .. } => db_file
            .clone()
            .unwrap_or_else(agentdeck_usage::providers::opencode_go::default_db_file)
            .display()
            .to_string(),
    }
}

fn usage_entry(account: &LegacyAccount, instance: Option<String>) -> anyhow::Result<UsageEntry> {
    let kind = UsageKind::parse(account.provider())?;
    let path_string = |p: &Path| p.to_string_lossy().into_owned();
    let mut entry = UsageEntry {
        id: account.id().to_owned(),
        kind,
        instance,
        display: account.name().to_owned(),
        api_key_env: None,
        db_file: None,
        extra: Default::default(),
    };
    match account {
        LegacyAccount::Deepseek { api_key_env, .. }
        | LegacyAccount::Openrouter { api_key_env, .. }
        | LegacyAccount::Vercel { api_key_env, .. } => {
            entry.api_key_env = Some(api_key_env.clone());
        }
        LegacyAccount::OpencodeGo { db_file, .. } => {
            entry.db_file = db_file.as_ref().map(|p| path_string(p));
        }
        LegacyAccount::Claude { .. }
        | LegacyAccount::Codex { .. }
        | LegacyAccount::Antigravity { .. } => {}
    }
    Ok(entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::SystemTime;

    static N: AtomicU64 = AtomicU64::new(0);

    struct Guard(PathBuf);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Fixture layout: `<dir>/old/{.config/ai-usage,.local/state/ai-usage,homes/*}`,
    /// new agentdeck roots at `<dir>/new/{config,state}` (absolute paths only,
    /// so no HOME/XDG env is read).
    struct Fixture {
        _g: Guard,
        paths: Paths,
        old: OldTree,
        old_root: PathBuf,
    }

    fn fixture(tag: &str) -> Fixture {
        let root = std::env::temp_dir().join(format!(
            "agentdeck-migrate-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&root);
        let old_home = root.join("old");
        let config_base = old_home.join(".config").join("ai-usage");
        let state_base = old_home.join(".local/state").join("ai-usage");
        let homes = old_home.join("homes");
        for d in [&config_base, &state_base, &homes] {
            fs::create_dir_all(d).unwrap();
        }
        for h in ["claude-main", "codex-main", "agy-main"] {
            fs::create_dir_all(homes.join(h)).unwrap();
        }
        let claude_home = homes.join("claude-main");
        let codex_home = homes.join("codex-main");
        let agy_token = homes.join("agy-main").join("acp_token.json");
        fs::write(&agy_token, "{}").unwrap();
        let config_toml = config_base.join("config.toml");
        let toml = format!(
            r#"
refresh_seconds = 120
stale_seconds = 3600

[[accounts]]
provider = "claude"
id = "claude-main"
name = "Claude Main"
config_dir = "{}"

[[accounts]]
provider = "codex"
id = "codex-main"
name = "Codex Main"
codex_home = "{}"
command = "codex"
limit_id = "codex"

[[accounts]]
provider = "claude"
id = "claude-orphan"
name = "Orphan"
config_dir = "/nonexistent/claude-x"

[[accounts]]
provider = "antigravity"
id = "agy-main"
name = "Google AI Pro"
token_file = "{}"

[[accounts]]
provider = "openrouter"
id = "openrouter-main"
name = "OpenRouter"
api_key_env = "AI_USAGE_OPENROUTER_OPENROUTER_MAIN"
base_url = "https://openrouter.ai/api/v1"

[[accounts]]
provider = "deepseek"
id = "deepseek-main"
name = "DeepSeek"
api_key_env = "AI_USAGE_DEEPSEEK_DEEPSEEK_MAIN"
base_url = "https://api.deepseek.com"

[[accounts]]
provider = "opencode-go"
id = "opencode-go"
name = "OpenCode Go"
"#,
            claude_home.display(),
            codex_home.display(),
            agy_token.display(),
        );
        fs::write(&config_toml, toml).unwrap();
        // Fake values only; never real secrets.
        fs::write(
            config_base.join("secrets.env"),
            "AI_USAGE_OPENROUTER_OPENROUTER_MAIN=fake-or-key\nAI_USAGE_DEEPSEEK_DEEPSEEK_MAIN=fake-ds-key\n",
        )
        .unwrap();
        fs::write(
            state_base.join("claude-usage-claude-main.json"),
            r#"{"fetched_at": 1700000000}"#,
        )
        .unwrap();
        fs::write(
            state_base.join("antigravity-usage-agy-main.json"),
            r#"{"updated_at": 1700000000}"#,
        )
        .unwrap();
        fs::write(state_base.join("history.jsonl"), "{}\n").unwrap();
        fs::write(state_base.join("history.jsonl.1"), "{}\n").unwrap();
        fs::write(state_base.join("state.json"), "{}").unwrap();
        fs::write(state_base.join("notes.txt"), "do not copy").unwrap();

        let paths = Paths::new(root.join("new/config"), root.join("new/state"));
        Registry::update(&paths, |reg| {
            for (id, harness, home) in [
                (
                    "claude-main",
                    agentdeck_core::Harness::Claude,
                    claude_home.clone(),
                ),
                (
                    "codex-main",
                    agentdeck_core::Harness::Codex,
                    codex_home.clone(),
                ),
                (
                    "agy-main",
                    agentdeck_core::Harness::Agy,
                    homes.join("agy-main"),
                ),
            ] {
                reg.add(agentdeck_core::registry::AddSpec {
                    id: id.to_owned(),
                    harness,
                    display_name: None,
                    home: home.display().to_string(),
                    binary: None,
                    no_t3: true,
                })?;
            }
            Ok(())
        })
        .unwrap();
        Fixture {
            old: OldTree {
                config_toml,
                secrets_env: config_base.join("secrets.env"),
                state_dir: state_base,
            },
            paths,
            old_root: old_home,
            _g: Guard(root),
        }
    }

    fn hash_tree(dir: &Path) -> u64 {
        let mut entries: Vec<(PathBuf, Vec<u8>)> = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            let Ok(rd) = fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if let Ok(b) = fs::read(&p) {
                    entries.push((p, b));
                }
            }
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let mut h = DefaultHasher::new();
        for (p, b) in entries {
            p.strip_prefix(dir).unwrap().hash(&mut h);
            b.hash(&mut h);
        }
        h.finish()
    }

    #[test]
    fn copies_everything_idempotently_and_leaves_the_old_tree_alone() {
        let f = fixture("full");
        let old_hash = hash_tree(&f.old_root);

        let plan = plan_migration(&f.paths, &f.old).unwrap();
        assert_eq!(plan.rows.len(), 7);
        // IDs are byte-identical to the old config.
        assert_eq!(
            plan.rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
            [
                "claude-main",
                "codex-main",
                "claude-orphan",
                "agy-main",
                "openrouter-main",
                "deepseek-main",
                "opencode-go"
            ]
        );
        let instance = |id: &str| {
            plan.rows
                .iter()
                .find(|r| r.id == id)
                .unwrap()
                .instance
                .clone()
        };
        assert_eq!(instance("claude-main").as_deref(), Some("claude-main"));
        assert_eq!(instance("codex-main").as_deref(), Some("codex-main"));
        assert_eq!(instance("claude-orphan"), None);
        assert_eq!(instance("agy-main").as_deref(), Some("agy-main"));
        assert_eq!(instance("openrouter-main"), None);
        assert!(plan.registry_change.is_some());
        // 1 secrets + 4 state files.
        assert_eq!(plan.file_changes.len(), 5);

        migrate_ai_usage(&f.paths, &f.old, false).unwrap();

        let reg = Registry::load(&f.paths).unwrap();
        assert_eq!(reg.usage.len(), 6);
        assert_eq!(
            reg.usage.iter().map(|u| u.id.clone()).collect::<Vec<_>>(),
            [
                "claude-main",
                "codex-main",
                "agy-main",
                "openrouter-main",
                "deepseek-main",
                "opencode-go"
            ]
        );
        let kinds: Vec<&str> = reg.usage.iter().map(|u| u.kind.as_str()).collect();
        assert_eq!(
            kinds,
            [
                "claude",
                "codex",
                "antigravity",
                "openrouter",
                "deepseek",
                "opencode-go"
            ]
        );

        let secrets = agentdeck_usage::daemon::agentdeck_secrets_file(&f.paths.config_dir);
        assert_eq!(
            fs::read(&secrets).unwrap(),
            fs::read(&f.old.secrets_env).unwrap()
        );
        assert_eq!(
            fs::metadata(&secrets).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let new_usage = f.paths.state_dir.join("usage");
        for name in [
            "claude-usage-claude-main.json",
            "antigravity-usage-agy-main.json",
            "history.jsonl",
            "history.jsonl.1",
        ] {
            assert_eq!(
                fs::read(new_usage.join(name)).unwrap(),
                fs::read(f.old.state_dir.join(name)).unwrap(),
                "{name}"
            );
        }
        assert!(!new_usage.join("state.json").exists());
        assert!(!new_usage.join("notes.txt").exists());

        // Second run: no changes, old tree byte-identical.
        let again = plan_migration(&f.paths, &f.old).unwrap();
        assert!(again.registry_change.is_none());
        assert!(again.file_changes.is_empty());
        assert!(again.kept_newer.is_empty());
        assert_eq!(hash_tree(&f.old_root), old_hash);
    }

    #[test]
    fn dry_run_writes_nothing() {
        let f = fixture("dry");
        migrate_ai_usage(&f.paths, &f.old, true).unwrap();
        // Registry existed from the fixture setup; migration must not add usage.
        assert!(Registry::load(&f.paths).unwrap().usage.is_empty());
        assert!(!f.paths.state_dir.join("usage").exists());
        assert!(!agentdeck_usage::daemon::agentdeck_secrets_file(&f.paths.config_dir).exists());
    }

    #[test]
    fn newer_destinations_are_kept() {
        let f = fixture("newer");
        migrate_ai_usage(&f.paths, &f.old, false).unwrap();
        // Old copy changes under an older mtime: destination is newer, must be kept.
        let dest = f.paths.state_dir.join("usage").join("history.jsonl");
        fs::write(&dest, "{}\n{\"t\":2}\n").unwrap();
        let src = f.old.state_dir.join("history.jsonl");
        let mtime = fs::metadata(&dest).unwrap().modified().unwrap();
        filetime_set_back(&src, mtime);
        let plan = plan_migration(&f.paths, &f.old).unwrap();
        assert!(plan.file_changes.is_empty());
        assert_eq!(plan.kept_newer, vec![dest.clone()]);
        migrate_ai_usage(&f.paths, &f.old, false).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"{}\n{\"t\":2}\n");
    }

    /// Set `src` mtime strictly older than `than` (a full minute back so
    /// coarse filesystem ticks cannot compare equal).
    fn filetime_set_back(src: &Path, than: SystemTime) {
        let older = than
            .checked_sub(std::time::Duration::from_secs(60))
            .unwrap();
        fs::File::options()
            .write(true)
            .open(src)
            .unwrap()
            .set_modified(older)
            .unwrap();
    }
}
