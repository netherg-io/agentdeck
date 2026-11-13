pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod deepseek;
pub mod opencode_go;
pub mod openrouter;
pub mod t3;
pub mod vercel;

use crate::config::AccountConfig;
use crate::model::AccountState;

pub async fn fetch(account: AccountConfig, stale_seconds: i64) -> AccountState {
    let id = account.id().to_owned();
    let name = account.name().to_owned();
    let provider = account.provider();

    let result = match account {
        AccountConfig::Claude {
            id,
            name,
            plan,
            instance,
            ..
        } => claude::fetch(&id, &name, plan, instance.as_deref(), stale_seconds).await,
        AccountConfig::Codex {
            id, name, instance, ..
        } => codex::fetch(&id, &name, instance.as_deref(), stale_seconds).await,
        AccountConfig::Deepseek {
            id,
            name,
            api_key_env,
            base_url,
        } => deepseek::fetch(&id, &name, &api_key_env, &base_url).await,
        AccountConfig::Openrouter {
            id,
            name,
            api_key_env,
            base_url,
        } => openrouter::fetch(&id, &name, &api_key_env, &base_url).await,
        AccountConfig::Vercel {
            id,
            name,
            api_key_env,
            base_url,
        } => vercel::fetch(&id, &name, &api_key_env, &base_url).await,
        AccountConfig::Antigravity {
            id, name, instance, ..
        } => antigravity::fetch(&id, &name, instance.as_deref(), stale_seconds).await,
        AccountConfig::OpencodeGo {
            id, name, instance, ..
        } => opencode_go::fetch(&id, &name, instance.as_deref(), stale_seconds).await,
    };

    result.unwrap_or_else(|error| AccountState::error(&id, &name, provider, error.to_string()))
}
