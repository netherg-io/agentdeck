use anyhow::Result;

use crate::model::AccountState;
use crate::providers::t3::{self, T3Source};

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
            provider: "antigravity",
            instance,
            explicit_plan: None,
        },
        stale_seconds,
    )
    .await
}
