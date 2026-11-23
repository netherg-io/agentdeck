//! Panel widget prefs: usage providers hidden from the COSMIC applet and the
//! accounts' display order. Written by the agentdeck GUI, read by the applet;
//! polling and routing ignore it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Prefs {
    #[serde(default)]
    pub hidden: BTreeSet<String>,
    /// Provider names in panel order (first shown first); unlisted providers
    /// follow in first-seen order. Legacy files with account ids still resolve.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub order: Vec<String>,
}

impl Prefs {
    /// Sort key for a provider or legacy account id under [`Prefs::order`].
    pub fn rank(&self, key: &str) -> usize {
        self.order
            .iter()
            .position(|o| o == key)
            .unwrap_or(usize::MAX)
    }

    /// Sort key for a provider group: matches the provider name directly, or
    /// falls back to the earliest legacy account id belonging to that provider.
    pub fn provider_rank<'a, T>(
        &self,
        provider: &str,
        items: &'a [T],
        id: impl Fn(&T) -> &str,
        item_provider: impl Fn(&'a T) -> &'a str,
    ) -> usize {
        let direct = self.rank(provider);
        if direct != usize::MAX {
            return direct;
        }
        items
            .iter()
            .filter(|item| item_provider(item) == provider)
            .map(|item| self.rank(id(item)))
            .min()
            .unwrap_or(usize::MAX)
    }

    /// Group items by provider in panel order so new instances immediately land
    /// next to their provider group and moving a group moves all its instances.
    pub fn group_by_provider<'a, T>(
        &self,
        items: &'a [T],
        id: impl Fn(&T) -> &str,
        provider: impl Fn(&'a T) -> &'a str,
    ) -> Vec<(&'a str, Vec<&'a T>)> {
        let mut groups: Vec<(&'a str, Vec<&'a T>)> = Vec::new();
        for item in items {
            let p = provider(item);
            if let Some((_, list)) = groups.iter_mut().find(|(k, _)| *k == p) {
                list.push(item);
            } else {
                groups.push((p, vec![item]));
            }
        }
        for (_, list) in &mut groups {
            list.sort_by_key(|item| self.rank(id(item)));
        }
        groups.sort_by_key(|(p, _)| self.provider_rank(p, items, &id, &provider));
        groups
    }
}

/// Same resolution order as `agentdeck_core::Paths::config_dir`.
pub fn path() -> PathBuf {
    if let Some(dir) = std::env::var_os("AGENTDECK_CONFIG_DIR").map(PathBuf::from) {
        return dir.join("ai-usage-visibility.json");
    }
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from) {
        return dir.join("agentdeck").join("ai-usage-visibility.json");
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        return home.join(".config/agentdeck/ai-usage-visibility.json");
    }
    std::env::temp_dir().join("agentdeck-ai-usage-visibility.json")
}

/// A missing file hides nothing and keeps state order. A malformed file is an
/// error and is never rewritten here, so the user can fix it.
pub fn load(path: &Path) -> Result<Prefs, String> {
    match std::fs::read(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Prefs::default()),
        Err(e) => Err(format!("{}: {e}", path.display())),
        Ok(bytes) => {
            serde_json::from_slice::<Prefs>(&bytes).map_err(|e| format!("{}: {e}", path.display()))
        }
    }
}

pub fn save(path: &Path, prefs: &Prefs) -> Result<(), String> {
    let data = serde_json::to_vec_pretty(prefs).map_err(|e| e.to_string())?;
    crate::util::atomic_write(path, &data).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_keeps_a_malformed_file_untouched() {
        let dir = std::env::temp_dir().join(format!("agentdeck-visibility-{}", std::process::id()));
        let path = dir.join("ai-usage-visibility.json");
        assert_eq!(load(&path).unwrap(), Prefs::default());
        let prefs = Prefs {
            hidden: ["codex".to_owned(), "claude".to_owned()].into(),
            order: vec!["b".into(), "a".into()],
        };
        save(&path, &prefs).unwrap();
        assert_eq!(load(&path).unwrap(), prefs);
        assert!((prefs.rank("b"), prefs.rank("a")) < (prefs.rank("a"), prefs.rank("zzz")));
        // Files written before `order` existed still load.
        std::fs::write(&path, br#"{"hidden":["x"]}"#).unwrap();
        assert!(load(&path).unwrap().order.is_empty());
        std::fs::write(&path, b"{not json").unwrap();
        assert!(load(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{not json");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn groups_new_instances_with_their_provider_and_honors_legacy_id_order() {
        let items = [
            ("codex-1", "codex"),
            ("claude-1", "claude"),
            ("agy-1", "antigravity"),
            ("claude-2", "claude"),
            ("agy-2", "antigravity"),
        ];
        // Provider-keyed order moves whole groups and keeps newly added instances in their group.
        let prefs = Prefs {
            order: vec!["antigravity".into(), "claude".into()],
            ..Prefs::default()
        };
        let grouped = prefs.group_by_provider(&items, |x| x.0, |x| x.1);
        let summary: Vec<(&str, Vec<&str>)> = grouped
            .iter()
            .map(|(p, list)| (*p, list.iter().map(|x| x.0).collect()))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("antigravity", vec!["agy-1", "agy-2"]),
                ("claude", vec!["claude-1", "claude-2"]),
                ("codex", vec!["codex-1"]),
            ]
        );

        // Legacy account-id order still ranks groups by their earliest member and pulls new
        // unlisted instances of that provider right into the group.
        let legacy = Prefs {
            order: vec!["agy-2".into(), "claude-1".into(), "codex-1".into()],
            ..Prefs::default()
        };
        let grouped = legacy.group_by_provider(&items, |x| x.0, |x| x.1);
        let summary: Vec<(&str, Vec<&str>)> = grouped
            .iter()
            .map(|(p, list)| (*p, list.iter().map(|x| x.0).collect()))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("antigravity", vec!["agy-2", "agy-1"]),
                ("claude", vec!["claude-1", "claude-2"]),
                ("codex", vec!["codex-1"]),
            ]
        );
    }
}
