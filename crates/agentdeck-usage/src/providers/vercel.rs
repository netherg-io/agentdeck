//! Vercel AI Gateway credits: `GET {base}/credits` → `{"balance", "total_used"}`.

use std::env;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::header::{ACCEPT, AUTHORIZATION};
use serde_json::Value;

use crate::model::{AccountState, BalanceInfo};

const MAX_ERROR_BODY: usize = 200;

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(5))
            .user_agent(concat!("ai-usage/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("failed to build HTTP client")
    })
}

/// The gateway sends amounts as decimal strings; accept numbers too.
fn amount(v: &Value, key: &str) -> Option<f64> {
    match v.get(key)? {
        Value::String(s) => s.trim().parse().ok(),
        other => other.as_f64(),
    }
}

fn state_from(id: &str, name: &str, payload: &Value) -> Result<AccountState> {
    let balance =
        amount(payload, "balance").context("Vercel AI Gateway: no balance in /credits")?;
    let used = amount(payload, "total_used");
    Ok(AccountState {
        id: id.to_owned(),
        name: name.to_owned(),
        provider: "vercel".to_owned(),
        status: if balance > 0.0 { "ok" } else { "depleted" }.to_owned(),
        plan: Some("Credits".to_owned()),
        email: None,
        model: None,
        windows: Vec::new(),
        balances: vec![BalanceInfo {
            currency: "USD".to_owned(),
            total: format!("{balance:.2}"),
            granted: None,
            topped_up: used.map(|u| format!("{:.2}", balance + u)),
        }],
        error: None,
        updated_at: crate::util::unix_now(),
    })
}

pub async fn fetch(
    id: &str,
    name: &str,
    api_key_env: &str,
    base_url: &str,
) -> Result<AccountState> {
    let api_key = env::var(api_key_env).with_context(|| format!("{api_key_env} is not set"))?;
    if api_key.trim().is_empty() {
        bail!("{api_key_env} is empty");
    }
    let res = client()
        .get(format!("{}/credits", base_url.trim_end_matches('/')))
        .header(ACCEPT, "application/json")
        .header(AUTHORIZATION, format!("Bearer {}", api_key.trim()))
        .send()
        .await
        .context("cannot reach Vercel AI Gateway")?;
    let status = res.status();
    if !status.is_success() {
        let body = res.text().await.unwrap_or_default();
        let body: String = body.trim().chars().take(MAX_ERROR_BODY).collect();
        bail!("Vercel AI Gateway returned HTTP {status}: {body}");
    }
    let payload: Value = res
        .json()
        .await
        .context("Vercel AI Gateway returned invalid JSON in /credits")?;
    state_from(id, name, &payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_string_and_number_amounts() {
        let s = state_from(
            "v",
            "Vercel",
            &serde_json::json!({"balance": "95.5", "total_used": 4.5}),
        )
        .unwrap();
        assert_eq!(s.status, "ok");
        assert_eq!(s.balances[0].total, "95.50");
        assert_eq!(s.balances[0].topped_up.as_deref(), Some("100.00"));
        let empty = state_from("v", "Vercel", &serde_json::json!({"balance": 0})).unwrap();
        assert_eq!(empty.status, "depleted");
        assert!(state_from("v", "Vercel", &serde_json::json!({})).is_err());
    }
}
