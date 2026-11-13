use std::path::PathBuf;

use anyhow::Result;

use crate::model::AccountState;
use crate::providers::t3::{self, T3Source};

/// Legacy OpenCode DB path kept strictly for `cmd_migrate` until U2.
pub fn default_db_file() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("~/.local/share"))
        .join("opencode")
        .join("opencode.db")
}

pub async fn fetch(
    id: &str,
    name: &str,
    instance: Option<&str>,
    stale_seconds: i64,
) -> Result<AccountState> {
    t3::fetch(
        T3Source {
            id,
            name,
            provider: "opencode-go",
            instance,
            explicit_plan: None,
        },
        stale_seconds,
    )
    .await
}
