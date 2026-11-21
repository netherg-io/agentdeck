//! Code-enforced Hard-Deny Perimeter (ADR-0001).
//!
//! No Run Mandate or Policy Judge prompt may override these checks. Any request
//! that trips the perimeter skips the Policy Judge and always escalates to human
//! confirmation in Telegram.

use std::sync::OnceLock;

use anyhow::{Result, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::ApproverConfig;

const CP_CONSOLE_LINE_MAX: usize = 1000;

/// Returns `true` for local workstation hosts that do not cross a remote relay boundary.
pub fn is_local_host(host: &str) -> bool {
    let h = host.trim();
    h.eq_ignore_ascii_case("local")
        || h.eq_ignore_ascii_case("localhost")
        || h == "127.0.0.1"
        || h == "::1"
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub kind: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, alias = "timeout", skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
    /// Authenticated remote peer host set in-process by a trusted transport.
    /// Never deserialized from untrusted wire JSON.
    #[serde(skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub trusted_peer_host: Option<String>,
}

impl ApprovalRequest {
    pub fn effective_host(&self) -> &str {
        self.host
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("local")
    }

    pub fn validate(&self) -> Result<()> {
        match self.kind.as_str() {
            "root" | "approve" => {
                let Some(script) = self.script.as_deref() else {
                    bail!("missing script for kind {:?}", self.kind);
                };
                if script.contains('\0') {
                    bail!("script contains NUL byte");
                }
                if self.kind == "approve" && self.nonce.as_deref().is_none_or(|n| n.is_empty()) {
                    bail!("missing nonce for kind \"approve\"");
                }
            }
            "confirm" | "secret" => {
                if self.title.is_none() && self.text.is_none() && self.reason.is_empty() {
                    bail!("missing title/text for kind {:?}", self.kind);
                }
            }
            "cp-console" => {
                let target = self.target.as_deref().unwrap_or("");
                if target.is_empty() || target.len() > 64 {
                    bail!("missing or invalid cp-console target");
                }
                let line = self.line.as_deref().unwrap_or("");
                if line.is_empty()
                    || line.len() > CP_CONSOLE_LINE_MAX
                    || line.chars().any(char::is_control)
                {
                    bail!("missing or invalid cp-console line");
                }
            }
            other => bail!("unknown request kind {other:?}"),
        }
        Ok(())
    }

    /// Canonical payload bytes hashed for approval verification and audit log.
    /// Matches `relay.py` and `approver.py` SHA-256 computation for `root`,
    /// `approve`, and `cp-console`.
    pub fn canonical_payload(&self) -> String {
        match self.kind.as_str() {
            "root" | "approve" => self.script.clone().unwrap_or_default(),
            "cp-console" => format!(
                "CP console [{}]:\n{}",
                self.target.as_deref().unwrap_or(""),
                self.line.as_deref().unwrap_or("")
            ),
            _ => {
                let title = self.title.as_deref().unwrap_or(&self.reason);
                let body = self.text.as_deref().unwrap_or("");
                format!("{title}\n{body}")
            }
        }
    }

    pub fn payload_sha256(&self) -> String {
        let digest = Sha256::digest(self.canonical_payload().as_bytes());
        hex_lower(&digest)
    }

    /// Executable / action surface scanned by perimeter regex rules.
    pub fn inspectable_surface(&self) -> String {
        let mut parts = Vec::with_capacity(4);
        if let Some(script) = &self.script {
            parts.push(script.as_str());
        }
        if let Some(text) = &self.text {
            parts.push(text.as_str());
        }
        if let Some(line) = &self.line {
            parts.push(line.as_str());
        }
        parts.join("\n")
    }
}

pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MandateContext {
    pub active_autonomous_run: bool,
    /// Set in memory by the caller after authenticating the owner grant.
    #[serde(skip_deserializing)]
    pub owner_granted: bool,
    #[serde(default)]
    pub allow_arbitrary_root_scripts: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    #[serde(default)]
    pub allowed_apps: Vec<String>,
    #[serde(default)]
    pub allow_releases: bool,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PerimeterVerdict {
    Clear,
    HardDeny { rule: &'static str, detail: String },
}

impl PerimeterVerdict {
    pub fn is_hard_deny(&self) -> bool {
        matches!(self, Self::HardDeny { .. })
    }

    pub fn rule(&self) -> Option<&'static str> {
        match self {
            Self::Clear => None,
            Self::HardDeny { rule, .. } => Some(rule),
        }
    }

    pub fn audit_label(&self) -> String {
        match self {
            Self::Clear => "clear".to_string(),
            Self::HardDeny { rule, .. } => format!("hard_deny:{rule}"),
        }
    }
}

struct CompiledRule {
    rule: &'static str,
    label: &'static str,
    regex: Regex,
}

fn compiled_rules() -> &'static [CompiledRule] {
    static RULES: OnceLock<Vec<CompiledRule>> = OnceLock::new();
    RULES.get_or_init(|| {
        let defs: &[(&'static str, &'static str, &str)] = &[
            // 2. Secret extraction / exfiltration
            (
                "secret_exfiltration",
                "agentdeck usage.env access",
                r"(?i)\busage\.env\b",
            ),
            (
                "secret_exfiltration",
                "agentdeck approver.env access",
                r"(?i)\bapprover\.env\b",
            ),
            (
                "secret_exfiltration",
                "agentdeck instances.toml access",
                r"(?i)\binstances\.toml\b",
            ),
            (
                "secret_exfiltration",
                "ssh directory access",
                r#"(?i)(?:^|[\s'"=/~$])\.ssh(?:/|\b)"#,
            ),
            (
                "secret_exfiltration",
                "ssh private key access",
                r"(?i)\bid_(?:rsa|ed25519|ecdsa|dsa)\b",
            ),
            (
                "secret_exfiltration",
                "system shadow file access",
                r"(?i)/etc/shadow\b",
            ),
            (
                "secret_exfiltration",
                "aws credentials directory access",
                r#"(?i)(?:^|[\s'"=/~$])\.aws(?:/|\b)"#,
            ),
            (
                "secret_exfiltration",
                "gh cli config access",
                r#"(?i)(?:^|[\s'"=/~$])\.config/gh(?:/|\b)"#,
            ),
            (
                "secret_exfiltration",
                "dotenv file access",
                r#"(?i)(?:^|[\s'"=/])\.env(?:\.[A-Za-z0-9_.-]+)?(?:$|[\s'"|;&>)])"#,
            ),
            (
                "secret_exfiltration",
                "printenv environment dump",
                r"\bprintenv\b",
            ),
            (
                "secret_exfiltration",
                "standalone env dump",
                r"(?m)(?:^|[;&|(`]\s*|\$\(\s*)(?:/usr/bin/|/bin/)?env(?:\s+(?:-[0u]|--null))*\s*(?:$|[|;&>)`#])",
            ),
            (
                "secret_exfiltration",
                "proc environ read",
                r"(?i)/proc/[^\s/]+/environ\b",
            ),
            (
                "secret_exfiltration",
                "encoded secret path dump",
                r"(?i)\b(?:base64|xxd|hexdump|od)\b[^\n;|&]*(?:secret|token|credential|private|\.pem\b|\.key\b|api[_-]?key)",
            ),
            // 3. Security / approver / OS tampering
            (
                "security_tampering",
                "agent-approver tampering",
                r"(?i)\bagent-approver\b",
            ),
            (
                "security_tampering",
                "agentdeck tampering",
                r"(?i)\bagentdeck\b",
            ),
            (
                "security_tampering",
                "sudoers modification",
                r"(?i)/etc/sudoers(?:\.d)?\b",
            ),
            (
                "security_tampering",
                "visudo invocation",
                r"\bvisudo\b",
            ),
            (
                "security_tampering",
                "sshd_config modification",
                r"(?i)\bsshd_config\b",
            ),
            (
                "security_tampering",
                "authorized_keys modification",
                r"(?i)\bauthorized_keys\b",
            ),
            (
                "security_tampering",
                "firewall iptables tampering",
                r"\bip6?tables\b",
            ),
            (
                "security_tampering",
                "firewall nftables tampering",
                r"\bnft\b",
            ),
            (
                "security_tampering",
                "firewall-cmd tampering",
                r"\bfirewall-cmd\b",
            ),
            (
                "security_tampering",
                "rpm-ostree OS layer change",
                r"\brpm-ostree\b",
            ),
            (
                "security_tampering",
                "account password change",
                r"\b(?:ch)?passwd\b",
            ),
            (
                "security_tampering",
                "account modification",
                r"\busermod\b",
            ),
            (
                "security_tampering",
                "world-writable root chmod",
                r"\bchmod\b[^\n;|&]*\b0?777\b[^\n;|&]*\s+/",
            ),
            (
                "security_tampering",
                "chown to root",
                r"\bchown\b[^\n;|&]*\broot(?:[:.]\S*)?\b",
            ),
            // 4. Unbacked destructive production data operations
            (
                "destructive_prod_data",
                "recursive root deletion",
                r"\brm\b[^\n;|&]*(?:-[^\s]*r[^\s]*f|-[^\s]*f[^\s]*r|-[a-zA-Z]*r[a-zA-Z]*\s+-[a-zA-Z]*f|-[a-zA-Z]*f[a-zA-Z]*\s+-[a-zA-Z]*r|--recursive[^\n;|&]*--force|--force[^\n;|&]*--recursive)[^\n;|&]*\s+/+\*?(?:\s|$|[;&|])",
            ),
            (
                "destructive_prod_data",
                "rm --no-preserve-root",
                r"--no-preserve-root\b",
            ),
            (
                "destructive_prod_data",
                "docker volume removal",
                r"\bdocker\s+volume\s+(?:rm|remove|prune)\b",
            ),
            (
                "destructive_prod_data",
                "docker system prune with volumes",
                r"\bdocker\s+system\s+prune\b[^\n;|&]*--volumes\b",
            ),
            (
                "destructive_prod_data",
                "SQL DROP DATABASE",
                r"(?i)\bDROP\s+DATABASE\b",
            ),
            (
                "destructive_prod_data",
                "SQL DROP TABLE",
                r"(?i)\bDROP\s+TABLE\b",
            ),
            (
                "destructive_prod_data",
                "SQL TRUNCATE TABLE",
                r"(?i)\bTRUNCATE\s+TABLE\b",
            ),
            (
                "destructive_prod_data",
                "filesystem format mkfs",
                r"\bmkfs(?:\.[A-Za-z0-9_+-]+)?\b",
            ),
            (
                "destructive_prod_data",
                "raw block device write via dd",
                r"\bdd\b[^\n;|&]*\bof=/dev/(?:sd[a-z]|nvme|vd[a-z]|hd[a-z]|xvd[a-z]|mmcblk|dm-|md\d|loop\d|mapper/)",
            ),
            (
                "shell_obfuscation",
                "shell substitution or eval",
                r"\$\(|`|[<>]\(|\beval\b",
            ),
            (
                "shell_obfuscation",
                "decoder invocation",
                r"\b(?:base64[^\n;|&]*\s(?:--decode|-[A-Za-z]*d[A-Za-z]*)|xxd[^\n;|&]*\s-[A-Za-z]*r[A-Za-z]*)\b",
            ),
            (
                "shell_obfuscation",
                "inline interpreter invocation",
                r"\b(?:(?:python[0-9.]*|bash|(?:da|a|z|k)?sh)[^\n;|&]*\s(?:--command|-[A-Za-z]*c[A-Za-z]*)|(?:perl|ruby|node(?:js)?)[^\n;|&]*\s(?:--eval|--print|-[A-Za-z]*[eEp][A-Za-z]*))\b",
            ),
        ];
        defs.iter()
            .map(|(rule, label, pat)| CompiledRule {
                rule,
                label,
                regex: Regex::new(pat).expect("static hard-deny regex must compile"),
            })
            .collect()
    })
}

/// Evaluate the non-overridable Hard-Deny Perimeter for `req` under `mandate`.
pub fn check_perimeter(
    req: &ApprovalRequest,
    mandate: Option<&MandateContext>,
) -> PerimeterVerdict {
    check_perimeter_inner(req, mandate, &[], None)
}

/// Evaluate the Hard-Deny Perimeter plus any configured `extra_hard_deny_patterns`.
pub fn check_perimeter_with_extra(
    req: &ApprovalRequest,
    mandate: Option<&MandateContext>,
    extra_patterns: &[String],
) -> PerimeterVerdict {
    check_perimeter_inner(req, mandate, extra_patterns, None)
}

/// Evaluate the Hard-Deny Perimeter enforcing global `config.remote_hosts` and
/// `config.extra_hard_deny_patterns`.
pub fn check_perimeter_with_config(
    req: &ApprovalRequest,
    mandate: Option<&MandateContext>,
    config: &ApproverConfig,
) -> PerimeterVerdict {
    check_perimeter_inner(
        req,
        mandate,
        &config.extra_hard_deny_patterns,
        Some(&config.remote_hosts),
    )
}

fn check_perimeter_inner(
    req: &ApprovalRequest,
    mandate: Option<&MandateContext>,
    extra_patterns: &[String],
    remote_hosts: Option<&[String]>,
) -> PerimeterVerdict {
    // 1. Secret entry always requires the human owner.
    if req.kind == "secret" {
        return PerimeterVerdict::HardDeny {
            rule: "secret_kind",
            detail: "secret requests always require human entry in Telegram".to_string(),
        };
    }

    // Scan the original and a conservative normalization of shell word quoting.
    let surface = req.inspectable_surface();
    let joined = surface.replace("\\\n", "");
    let normalized = joined.replace(['\'', '"', '\\'], "");
    let surfaces = [&surface, &joined, &normalized];

    // 2-4. Built-in code-enforced patterns.
    for compiled in compiled_rules() {
        if surfaces.iter().any(|text| compiled.regex.is_match(text)) {
            return PerimeterVerdict::HardDeny {
                rule: compiled.rule,
                detail: compiled.label.to_string(),
            };
        }
    }

    // Global configured remote host allowlist (empty list denies all non-local hosts).
    if let Some(allowed_remote) = remote_hosts {
        let effective_host = req.effective_host();
        if !is_local_host(effective_host)
            && !allowed_remote
                .iter()
                .any(|h| h.trim().eq_ignore_ascii_case(effective_host))
        {
            return PerimeterVerdict::HardDeny {
                rule: "remote_host_not_allowed",
                detail: format!(
                    "remote host {effective_host:?} is outside configured remote_hosts"
                ),
            };
        }
    }

    // 5. Out-of-scope host or app relative to active mandate.
    if let Some(m) = mandate {
        let host = req.host.as_deref().unwrap_or_default().trim();
        if !m.allowed_hosts.is_empty()
            && (host.is_empty() || !m.allowed_hosts.iter().any(|h| h.eq_ignore_ascii_case(host)))
        {
            return PerimeterVerdict::HardDeny {
                rule: "out_of_scope_host",
                detail: format!("host {host:?} is outside mandate allowed_hosts"),
            };
        }
        let app = req.app.as_deref().unwrap_or_default().trim();
        if !m.allowed_apps.is_empty()
            && (app.is_empty() || !m.allowed_apps.iter().any(|a| a.eq_ignore_ascii_case(app)))
        {
            return PerimeterVerdict::HardDeny {
                rule: "out_of_scope_app",
                detail: format!("app {app:?} is outside mandate allowed_apps"),
            };
        }
    }

    // User-configured extra hard-deny patterns (fail-closed on bad regex).
    for pat in extra_patterns {
        match Regex::new(&format!("(?i){pat}")) {
            Ok(re) => {
                if surfaces.iter().any(|text| re.is_match(text)) {
                    return PerimeterVerdict::HardDeny {
                        rule: "extra_hard_deny_pattern",
                        detail: format!("matched extra hard-deny pattern {pat:?}"),
                    };
                }
            }
            Err(e) => {
                return PerimeterVerdict::HardDeny {
                    rule: "invalid_extra_hard_deny_pattern",
                    detail: format!("invalid extra pattern {pat:?}: {e}"),
                };
            }
        }
    }

    if matches!(req.kind.as_str(), "root" | "approve") {
        if normalized.contains('$') {
            return PerimeterVerdict::HardDeny {
                rule: "shell_obfuscation",
                detail: "shell expansion requires human confirmation".into(),
            };
        }
        if mandate.is_some_and(|m| !m.allow_arbitrary_root_scripts) {
            return PerimeterVerdict::HardDeny {
                rule: "arbitrary_root_script",
                detail: "arbitrary root scripts require human confirmation".into(),
            };
        }
    }

    PerimeterVerdict::Clear
}
