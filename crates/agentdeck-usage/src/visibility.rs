//! Panel widget visibility: usage providers hidden from the COSMIC applet.
//! Written by the agentdeck GUI, read by the applet; polling and routing ignore it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct VisibilityFile {
    #[serde(default)]
    hidden: Vec<String>,
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

/// Hidden providers; a missing file hides nothing. A malformed file is an error
/// and is never rewritten here, so the user can fix it.
pub fn load(path: &Path) -> Result<BTreeSet<String>, String> {
    match std::fs::read(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeSet::new()),
        Err(e) => Err(format!("{}: {e}", path.display())),
        Ok(bytes) => serde_json::from_slice::<VisibilityFile>(&bytes)
            .map(|file| file.hidden.into_iter().collect())
            .map_err(|e| format!("{}: {e}", path.display())),
    }
}

pub fn save(path: &Path, hidden: &BTreeSet<String>) -> Result<(), String> {
    let file = VisibilityFile {
        hidden: hidden.iter().cloned().collect(),
    };
    let data = serde_json::to_vec_pretty(&file).map_err(|e| e.to_string())?;
    crate::util::atomic_write(path, &data).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_keeps_a_malformed_file_untouched() {
        let dir = std::env::temp_dir().join(format!("agentdeck-visibility-{}", std::process::id()));
        let path = dir.join("ai-usage-visibility.json");
        assert!(load(&path).unwrap().is_empty());
        let hidden: BTreeSet<String> = ["codex".to_owned(), "claude".to_owned()].into();
        save(&path, &hidden).unwrap();
        assert_eq!(load(&path).unwrap(), hidden);
        std::fs::write(&path, b"{not json").unwrap();
        assert!(load(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{not json");
        std::fs::remove_dir_all(&dir).ok();
    }
}
