//! Local approval broker implementing ADR-0001:
//! - Unix socket broker wire-compatible with `approve-client.py`
//! - Code-enforced Hard-Deny Perimeter
//! - Policy Judge (OpenRouter / Vercel AI Gateway) for Autonomous Runs
//! - Direct Telegram Bot API long-polling (`getUpdates`)
//! - Opaque Approval with strict pre-execution timeout and disconnect cancel
//! - Append-only JSONL audit log and golden/adversarial evaluation suite

pub mod audit;
pub mod broker;
pub mod config;
pub mod eval;
pub mod judge;
pub mod perimeter;
pub mod telegram;

pub use audit::{
    AuditRecord, AuditSource, append_record, audit_log_path, read_recent, sanitize_judge_reason,
};
pub use broker::{
    BrokerContext, BrokerDecision, BrokerOutcome, BrokerTimings, MandateResolver, PendingEvent,
    UnixBrokerServer, evaluate_and_wait, format_wire_result,
};
pub use config::{
    ApproverConfig, ApproverSecrets, MAX_APPROVAL_TIMEOUT_SECS, MIN_APPROVAL_TIMEOUT_SECS,
    ssh_reverse_tunnel_args,
};
pub use judge::{HttpPolicyJudge, JudgeError, JudgeVerdict, PolicyJudge, is_auto_approved};
pub use perimeter::{
    ApprovalRequest, MandateContext, PerimeterVerdict, check_perimeter,
    check_perimeter_with_config, check_perimeter_with_extra, is_local_host,
};
pub use telegram::{
    BotApiTelegram, FINALIZE_APPROVED, FINALIZE_DENIED, FINALIZE_TIMEOUT, PendingDecisions,
    TelegramDecision, TelegramTransport,
};
