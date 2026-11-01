pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod deepseek;
pub mod opencode_go;
pub mod openrouter;
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
            config_dir,
        } => claude::fetch(&id, &name, plan, &config_dir, stale_seconds).await,
        AccountConfig::Codex {
            id,
            name,
            codex_home,
            command,
            limit_id,
        } => codex::fetch(&id, &name, &codex_home, &command, &limit_id).await,
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
            id,
            name,
            token_file,
        } => antigravity::fetch(&id, &name, token_file.as_deref(), stale_seconds).await,
        AccountConfig::OpencodeGo { id, name, db_file } => {
            opencode_go::fetch(&id, &name, db_file.as_deref()).await
        }
    };

    result.unwrap_or_else(|error| AccountState::error(&id, &name, provider, error.to_string()))
}
