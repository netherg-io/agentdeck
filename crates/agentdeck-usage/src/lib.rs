pub mod config;
pub mod daemon;
pub mod history;
pub mod hooks;
pub mod model;
pub mod providers;
pub mod util;
pub mod visibility;

/// Верхняя граница на один provider fetch. Провайдер, вышедший за неё,
/// становится обычным error state и не задерживает остальные аккаунты.
pub const PROVIDER_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);
