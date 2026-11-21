//! Configuration, 0600 secret loading, and SSH reverse-tunnel argument builder.

use std::fmt;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use agentdeck_core::settings::{PROVIDER_OPENROUTER, PROVIDER_VERCEL};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const MIN_APPROVAL_TIMEOUT_SECS: u64 = 1;
pub const MAX_APPROVAL_TIMEOUT_SECS: u64 = 7200;
pub const DEFAULT_AUTONOMOUS_TIMEOUT_SECS: u64 = 120;
pub const DEFAULT_INTERACTIVE_TIMEOUT_SECS: u64 = 600;
pub const DEFAULT_JUDGE_CONFIDENCE_THRESHOLD: f64 = 0.85;
pub const SECRETS_FILENAME: &str = "approver.env";

fn default_autonomous_timeout() -> u64 {
    DEFAULT_AUTONOMOUS_TIMEOUT_SECS
}

fn default_interactive_timeout() -> u64 {
    DEFAULT_INTERACTIVE_TIMEOUT_SECS
}

fn default_confidence_threshold() -> f64 {
    DEFAULT_JUDGE_CONFIDENCE_THRESHOLD
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ApproverConfig {
    pub enabled: bool,
    pub socket_path: Option<PathBuf>,
    #[serde(default = "default_autonomous_timeout")]
    pub autonomous_timeout_secs: u64,
    #[serde(default = "default_interactive_timeout")]
    pub interactive_timeout_secs: u64,
    pub judge_provider: Option<String>,
    pub judge_model: Option<String>,
    #[serde(default = "default_confidence_threshold")]
    pub judge_confidence_threshold: f64,
    pub remote_hosts: Vec<String>,
    pub extra_hard_deny_patterns: Vec<String>,
}

impl Default for ApproverConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            socket_path: None,
            autonomous_timeout_secs: DEFAULT_AUTONOMOUS_TIMEOUT_SECS,
            interactive_timeout_secs: DEFAULT_INTERACTIVE_TIMEOUT_SECS,
            judge_provider: None,
            judge_model: None,
            judge_confidence_threshold: DEFAULT_JUDGE_CONFIDENCE_THRESHOLD,
            remote_hosts: Vec::new(),
            extra_hard_deny_patterns: Vec::new(),
        }
    }
}

impl ApproverConfig {
    pub fn validate(&self) -> Result<()> {
        if !(MIN_APPROVAL_TIMEOUT_SECS..=MAX_APPROVAL_TIMEOUT_SECS)
            .contains(&self.autonomous_timeout_secs)
        {
            bail!(
                "autonomous_timeout_secs must be within {MIN_APPROVAL_TIMEOUT_SECS}..={MAX_APPROVAL_TIMEOUT_SECS}, got {}",
                self.autonomous_timeout_secs
            );
        }
        if !(MIN_APPROVAL_TIMEOUT_SECS..=MAX_APPROVAL_TIMEOUT_SECS)
            .contains(&self.interactive_timeout_secs)
        {
            bail!(
                "interactive_timeout_secs must be within {MIN_APPROVAL_TIMEOUT_SECS}..={MAX_APPROVAL_TIMEOUT_SECS}, got {}",
                self.interactive_timeout_secs
            );
        }
        if !self.judge_confidence_threshold.is_finite()
            || !(0.0..=1.0).contains(&self.judge_confidence_threshold)
        {
            bail!(
                "judge_confidence_threshold must be finite and within [0.0, 1.0], got {}",
                self.judge_confidence_threshold
            );
        }
        if let Some(provider) = self.judge_provider.as_deref()
            && provider != PROVIDER_OPENROUTER
            && provider != PROVIDER_VERCEL
        {
            bail!(
                "judge_provider must be {PROVIDER_OPENROUTER:?} or {PROVIDER_VERCEL:?}, got {provider:?}"
            );
        }
        for pat in &self.extra_hard_deny_patterns {
            if pat.is_empty() {
                bail!("extra_hard_deny_patterns must not contain empty patterns");
            }
            regex::Regex::new(&format!("(?i){pat}"))
                .with_context(|| format!("invalid extra_hard_deny_pattern {pat:?}"))?;
        }
        Ok(())
    }

    pub fn resolved_socket_path(&self) -> PathBuf {
        if let Some(path) = &self.socket_path {
            return path.clone();
        }
        if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
            return PathBuf::from(dir).join("agentdeck").join("approver.sock");
        }
        std::env::temp_dir().join("agentdeck-approver.sock")
    }
}

/// Secrets loaded from `config_dir/approver.env` (0600). Debug output is always redacted.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ApproverSecrets {
    pub telegram_bot_token: Option<String>,
    pub telegram_owner_id: Option<i64>,
    pub ai_gateway_api_key: Option<String>,
    pub openrouter_api_key: Option<String>,
}

impl fmt::Debug for ApproverSecrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApproverSecrets")
            .field(
                "telegram_bot_token",
                &self.telegram_bot_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("telegram_owner_id", &self.telegram_owner_id)
            .field(
                "ai_gateway_api_key",
                &self.ai_gateway_api_key.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "openrouter_api_key",
                &self.openrouter_api_key.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

impl ApproverSecrets {
    pub fn secrets_path(config_dir: &Path) -> PathBuf {
        config_dir.join(SECRETS_FILENAME)
    }

    /// Load secrets from `config_dir/approver.env`, tightening permissions to 0600 if wider.
    /// Does not mutate process environment variables; safe to call concurrently in tests.
    pub fn load(config_dir: &Path) -> Result<Self> {
        let path = Self::secrets_path(config_dir);
        if !path.exists() {
            return Ok(Self::default());
        }
        Self::load_from_file(&path)
    }

    /// Read and parse `config_dir/approver.env` without mutating file permissions.
    /// Returns `Ok(None)` when the file does not exist and `Err` when unreadable or malformed.
    pub fn inspect_readonly(config_dir: &Path) -> Result<Option<Self>> {
        let path = Self::secrets_path(config_dir);
        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("failed to read secrets file {}", path.display()));
            }
        };
        Self::parse_env(&raw).map(Some)
    }

    pub fn load_from_file(path: &Path) -> Result<Self> {
        let meta = fs::metadata(path)
            .with_context(|| format!("failed to stat secrets file {}", path.display()))?;
        if meta.permissions().mode() & 0o777 != 0o600 {
            eprintln!(
                "warning: {} must be 0600 (approver secrets); fixing",
                path.display()
            );
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).with_context(|| {
                format!("failed to enforce 0600 permissions on {}", path.display())
            })?;
        }
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read secrets file {}", path.display()))?;
        Self::parse_env(&raw)
    }

    pub fn parse_env(raw: &str) -> Result<Self> {
        let mut out = Self::default();
        for (idx, line) in raw.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let trimmed = trimmed.strip_prefix("export ").unwrap_or(trimmed).trim();
            let Some((key, value)) = trimmed.split_once('=') else {
                bail!("malformed line {} in approver.env (expected KEY=VALUE)", idx + 1);
            };
            let key = key.trim();
            let val = strip_quotes(value.trim());
            if val.is_empty() {
                continue;
            }
            match key {
                "TELEGRAM_BOT_TOKEN" => out.telegram_bot_token = Some(val.to_string()),
                "TELEGRAM_OWNER_ID" => {
                    let id = val.parse::<i64>().with_context(|| {
                        format!("invalid TELEGRAM_OWNER_ID on line {}", idx + 1)
                    })?;
                    out.telegram_owner_id = Some(id);
                }
                "AI_GATEWAY_API_KEY" => out.ai_gateway_api_key = Some(val.to_string()),
                "OPENROUTER_API_KEY" | "JEV_OPENROUTER_API_KEY" => {
                    out.openrouter_api_key = Some(val.to_string());
                }
                _ => {}
            }
        }
        Ok(out)
    }

    pub fn judge_api_key(&self, provider: &str) -> Option<&str> {
        match provider {
            PROVIDER_VERCEL => self.ai_gateway_api_key.as_deref(),
            PROVIDER_OPENROUTER => self.openrouter_api_key.as_deref(),
            _ => None,
        }
    }
}

fn strip_quotes(val: &str) -> &str {
    if val.len() >= 2
        && ((val.starts_with('"') && val.ends_with('"'))
            || (val.starts_with('\'') && val.ends_with('\'')))
    {
        &val[1..val.len() - 1]
    } else {
        val
    }
}

/// Build safe argv for `ssh` reverse Unix-socket forwarding without shell interpolation.
/// Callers pass these directly to `Command::new("ssh").args(...)`.
pub fn ssh_reverse_tunnel_args(
    host: &str,
    local_socket: &Path,
    remote_socket: &str,
) -> Vec<String> {
    vec![
        "-N".to_string(),
        "-T".to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ExitOnForwardFailure=yes".to_string(),
        "-o".to_string(),
        "StreamLocalBindUnlink=yes".to_string(),
        "-o".to_string(),
        "ServerAliveInterval=30".to_string(),
        "-o".to_string(),
        "ServerAliveCountMax=3".to_string(),
        "-R".to_string(),
        format!("{remote_socket}:{}", local_socket.display()),
        "--".to_string(),
        host.to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-approver-cfg-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn default_config_is_valid() {
        let cfg = ApproverConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.autonomous_timeout_secs, 120);
        assert_eq!(cfg.interactive_timeout_secs, 600);
        assert!((cfg.judge_confidence_threshold - 0.85).abs() < f64::EPSILON);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn secrets_load_fixes_mode_and_redacts_debug() {
        let dir = temp_dir("secrets");
        let path = ApproverSecrets::secrets_path(&dir);
        fs::write(
            &path,
            "# comment\nTELEGRAM_BOT_TOKEN=\"tok-secret-123\"\nTELEGRAM_OWNER_ID=4242\nAI_GATEWAY_API_KEY=gw-key\nOPENROUTER_API_KEY='or-key'\n",
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        let secrets = ApproverSecrets::load(&dir).unwrap();
        assert_eq!(secrets.telegram_bot_token.as_deref(), Some("tok-secret-123"));
        assert_eq!(secrets.telegram_owner_id, Some(4242));
        assert_eq!(secrets.judge_api_key(PROVIDER_VERCEL), Some("gw-key"));
        assert_eq!(secrets.judge_api_key(PROVIDER_OPENROUTER), Some("or-key"));

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let dbg = format!("{secrets:?}");
        assert!(!dbg.contains("tok-secret-123"));
        assert!(!dbg.contains("gw-key"));
        assert!(!dbg.contains("or-key"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn inspect_readonly_never_mutates_file_mode_and_distinguishes_missing_from_invalid() {
        let dir = temp_dir("readonly");
        assert_eq!(ApproverSecrets::inspect_readonly(&dir).unwrap(), None);

        let path = ApproverSecrets::secrets_path(&dir);
        fs::write(
            &path,
            "TELEGRAM_BOT_TOKEN=tok-ro\nTELEGRAM_OWNER_ID=99\nOPENROUTER_API_KEY=or-ro\n",
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        let secrets = ApproverSecrets::inspect_readonly(&dir)
            .unwrap()
            .expect("existing secrets file must return Some");
        assert_eq!(secrets.telegram_bot_token.as_deref(), Some("tok-ro"));
        assert_eq!(secrets.telegram_owner_id, Some(99));
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644, "inspect_readonly must not mutate permissions");

        fs::write(&path, "MALFORMED_LINE_WITHOUT_EQUALS\n").unwrap();
        assert!(ApproverSecrets::inspect_readonly(&dir).is_err());
        let mode_after_err = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode_after_err, 0o644);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_enforces_timeout_bounds() {
        for bad in [0, MAX_APPROVAL_TIMEOUT_SECS + 1, u64::MAX] {
            let mut cfg = ApproverConfig {
                autonomous_timeout_secs: bad,
                ..Default::default()
            };
            assert!(cfg.validate().is_err(), "autonomous={bad}");
            cfg.autonomous_timeout_secs = DEFAULT_AUTONOMOUS_TIMEOUT_SECS;
            cfg.interactive_timeout_secs = bad;
            assert!(cfg.validate().is_err(), "interactive={bad}");
        }
    }

    #[test]
    fn ssh_reverse_tunnel_args_are_shell_free_and_safe() {
        let args = ssh_reverse_tunnel_args(
            "homeserver-agent",
            Path::new("/run/user/1000/agentdeck/approver.sock"),
            "/run/agent-approver/broker.sock",
        );
        assert!(args.windows(2).any(|w| w == ["-o", "BatchMode=yes"]));
        assert!(args.windows(2).any(|w| w == ["-o", "ExitOnForwardFailure=yes"]));
        assert!(args.windows(2).any(|w| {
            w == [
                "-R",
                "/run/agent-approver/broker.sock:/run/user/1000/agentdeck/approver.sock",
            ]
        }));
        assert_eq!(&args[args.len() - 2..], &["--", "homeserver-agent"]);
    }
}
