use std::env;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub fn expand_home(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    if raw == "~" {
        return dirs::home_dir().unwrap_or_else(|| path.to_path_buf());
    }
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    path.to_path_buf()
}

pub fn runtime_dir() -> PathBuf {
    if let Some(dir) = env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("ai-usage");
    }
    let user = env::var("USER").unwrap_or_else(|_| "unknown".to_owned());
    env::temp_dir().join(format!("ai-usage-{user}"))
}

pub fn state_file() -> PathBuf {
    runtime_dir().join("state.json")
}

/// The `~/.local/state/ai-usage` tree: compat copies, and everything the
/// migrator reads.
pub fn legacy_persistent_state_dir() -> PathBuf {
    dirs::state_dir()
        .map(|dir| dir.join("ai-usage"))
        .unwrap_or_else(runtime_dir)
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    atomic_write_mode(path, bytes, None)
}

/// Записывает файл атомарно. `mode` выставляется на временном файле до `rename`,
/// поэтому итоговый файл никогда не существует с более широкими правами.
pub fn atomic_write_mode(path: &Path, bytes: &[u8], mode: Option<u32>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    {
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        if let Some(mode) = mode {
            options.mode(mode);
        }
        let mut file = options
            .open(&tmp)
            .with_context(|| format!("Не удалось создать {}", tmp.display()))?;
        if let Some(mode) = mode {
            // create-time mode проходит через umask, поэтому фиксируем права явно.
            file.set_permissions(fs::Permissions::from_mode(mode))?;
        }
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path).with_context(|| format!("Не удалось заменить {}", path.display()))?;
    Ok(())
}

pub fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() {
        bail!("ID не может быть пустым");
    }
    if !id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        bail!("ID может содержать только латинские буквы, цифры, '-' и '_'");
    }
    Ok(())
}

/// `claude_max_5x` → `Max 5x`, `plus` → `Plus`.
///
/// Тариф приходит от провайдеров машинным идентификатором, а в панели должен
/// читаться человеком. Цифровые части (`5x`, `20x`) остаются как есть —
/// именно они отличают тарифы друг от друга.
pub fn humanize_plan(raw: &str) -> String {
    let trimmed = raw
        .strip_prefix("default_claude_")
        .or_else(|| raw.strip_prefix("default_"))
        .or_else(|| raw.strip_prefix("claude_"))
        .unwrap_or(raw);

    trimmed
        .split(['_', '-'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Shared loader: `path` holds `KEY=value` lines, existing process vars win.
/// Values are never printed.
pub fn load_secrets_env_from(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let raw = fs::read_to_string(path)?;
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=')
            && env::var_os(key.trim()).is_none()
        {
            // Edition 2024 marks set_var unsafe (thread-safety); single-threaded
            // load path, same as before the move.
            unsafe { env::set_var(key.trim(), value.trim()) };
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!(
            "ai-usage-test-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn atomic_write_creates_parent_directories() {
        let dir = temp_dir("atomic-parents");
        let path = dir.join("nested").join("deeper").join("state.json");
        atomic_write(&path, b"{}").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "{}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn atomic_write_leaves_no_temporary_files() {
        let dir = temp_dir("atomic-temp");
        let path = dir.join("state.json");
        atomic_write(&path, b"first").unwrap();
        atomic_write(&path, b"second").unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "second");
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("tmp-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "остались временные файлы: {leftovers:?}"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn secret_file_is_never_world_readable_even_for_an_instant() {
        // Права выставляются на временном файле до rename, поэтому итоговый
        // путь сразу появляется с 0600 — отдельного chmod после записи нет.
        let dir = temp_dir("secret-mode");
        let path = dir.join("secrets.env");
        atomic_write_mode(&path, b"KEY=value\n", Some(0o600)).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "получены права {mode:o}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rewriting_a_secret_keeps_restrictive_mode() {
        let dir = temp_dir("secret-rewrite");
        let path = dir.join("secrets.env");
        atomic_write_mode(&path, b"KEY=one\n", Some(0o600)).unwrap();
        atomic_write_mode(&path, b"KEY=two\n", Some(0o600)).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(fs::read_to_string(&path).unwrap(), "KEY=two\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn plan_names_stay_readable_and_keep_their_multiplier() {
        // Множитель — единственное, что отличает Max 5x от Max 20x.
        assert_eq!(humanize_plan("default_claude_max_5x"), "Max 5x");
        assert_eq!(humanize_plan("default_claude_max_20x"), "Max 20x");
        assert_eq!(humanize_plan("claude_max"), "Max");
        assert_eq!(humanize_plan("plus"), "Plus");
        assert_eq!(humanize_plan("pro"), "Pro");
        assert_eq!(humanize_plan("business"), "Business");
        assert_eq!(humanize_plan("team"), "Team");
    }

    #[test]
    fn an_unknown_plan_is_shown_rather_than_dropped() {
        assert_eq!(humanize_plan("some_future_tier_3x"), "Some Future Tier 3x");
        assert_eq!(humanize_plan(""), "");
    }

    #[test]
    fn accepts_safe_ids() {
        for id in ["claude-main", "codex_work", "a1", "A-B_9"] {
            validate_id(id).unwrap_or_else(|error| panic!("{id} отклонён: {error}"));
        }
    }

    #[test]
    fn rejects_ids_that_could_escape_a_directory() {
        // ID подставляется в имя файла кеша, поэтому '/' и '..' недопустимы.
        for id in ["", "../etc", "a/b", "a b", "café", "a.b"] {
            assert!(validate_id(id).is_err(), "ID '{id}' должен быть отклонён");
        }
    }

    #[test]
    fn shell_quoting_survives_embedded_quotes() {
        assert_eq!(
            shell_single_quote("/usr/bin/ai-usage"),
            "'/usr/bin/ai-usage'"
        );
        assert_eq!(shell_single_quote("it's"), r"'it'\''s'");
        assert_eq!(
            shell_single_quote("/home/o'brien/bin/ai usage"),
            r"'/home/o'\''brien/bin/ai usage'"
        );
    }

    #[test]
    fn expands_only_leading_tilde() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_home(Path::new("~")), home);
        assert_eq!(expand_home(Path::new("~/.codex")), home.join(".codex"));
        // '~' в середине пути — обычный символ.
        assert_eq!(
            expand_home(Path::new("/tmp/~/x")),
            PathBuf::from("/tmp/~/x")
        );
        assert_eq!(
            expand_home(Path::new("/absolute/path")),
            PathBuf::from("/absolute/path")
        );
    }
}
