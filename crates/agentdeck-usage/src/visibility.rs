//! Panel widget prefs: usage providers hidden from the COSMIC applet and the
//! accounts' display order. Written by the agentdeck GUI, read by the applet;
//! polling and routing ignore it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Prefs {
    #[serde(default)]
    pub hidden: BTreeSet<String>,
    /// Account ids, first shown first; unlisted accounts follow in state order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub order: Vec<String>,
}

impl Prefs {
    /// Sort key for an account id under [`Prefs::order`]; use with a stable sort.
    pub fn rank(&self, id: &str) -> usize {
        self.order
            .iter()
            .position(|o| o == id)
            .unwrap_or(usize::MAX)
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
}
