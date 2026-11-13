use anyhow::Result;

use crate::model::AccountState;
use crate::providers::t3::{self, T3Source};

pub const DEFAULT_STALE_SECONDS: i64 = 900;

pub async fn fetch(
    id: &str,
    name: &str,
    plan: Option<String>,
    instance: Option<&str>,
    stale_seconds: i64,
) -> Result<AccountState> {
    t3::fetch(
        T3Source {
            id,
            name,
            provider: "claude",
            instance,
            explicit_plan: plan.as_deref(),
        },
        stale_seconds,
    )
    .await
}

pub fn humanize_age(seconds: i64) -> String {
    let minutes = seconds / 60;
    match minutes {
        ..=0 => "меньше минуты".to_owned(),
        1..=59 => format!("{minutes} мин"),
        _ => format!("{} ч {} мин", minutes / 60, minutes % 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanizes_age_values() {
        assert_eq!(humanize_age(0), "меньше минуты");
        assert_eq!(humanize_age(-5), "меньше минуты");
        assert_eq!(humanize_age(59), "меньше минуты");
        assert_eq!(humanize_age(60), "1 мин");
        assert_eq!(humanize_age(3_540), "59 мин");
        assert_eq!(humanize_age(3_600), "1 ч 0 мин");
        assert_eq!(humanize_age(19_860), "5 ч 31 мин");
    }
}
