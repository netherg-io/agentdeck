//! Direct Telegram Bot API transport using long-polling (`getUpdates`),
//! removing Vercel/Upstash webhook dependencies.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::sync::{oneshot, watch};

use crate::perimeter::ApprovalRequest;

pub const HARD_DENY_PREFIX: &str = "[HARD-DENY: только человек]";
pub const FINALIZE_APPROVED: &str = "— разрешено ✅";
pub const FINALIZE_DENIED: &str = "— отказано ❌";
pub const FINALIZE_TIMEOUT: &str = "— время вышло ⌛";
const TELEGRAM_API_BASE: &str = "https://api.telegram.org";
const TELEGRAM_CALL_TIMEOUT: Duration = Duration::from_secs(15);
const TELEGRAM_POLL_TIMEOUT_SECS: u64 = 50;
const TELEGRAM_POLL_HTTP_TIMEOUT: Duration = Duration::from_secs(65);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelegramDecision {
    Approved { secret_value: Option<String> },
    Denied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallbackAction {
    Approve,
    Deny,
}

pub trait TelegramTransport: Send + Sync {
    fn send_request(
        &self,
        id: &str,
        req: &ApprovalRequest,
        hard_deny: Option<&str>,
        timeout_secs: u64,
    ) -> impl Future<Output = Result<i64>> + Send;

    fn cancel_request(
        &self,
        message_id: i64,
        reason: &str,
    ) -> impl Future<Output = Result<()>> + Send;

    fn finalize_request(
        &self,
        message_id: i64,
        outcome_text: &str,
    ) -> impl Future<Output = Result<()>> + Send;
}

struct PendingEntry {
    tx: oneshot::Sender<TelegramDecision>,
    is_secret: bool,
    message_id: Option<i64>,
}

/// Shared registry of in-flight Telegram approval requests awaiting owner action.
#[derive(Default)]
pub struct PendingDecisions {
    inner: Mutex<HashMap<String, PendingEntry>>,
}

impl PendingDecisions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, id: String, is_secret: bool) -> oneshot::Receiver<TelegramDecision> {
        let (tx, rx) = oneshot::channel();
        let mut map = self.inner.lock().expect("pending decisions mutex poisoned");
        map.insert(
            id,
            PendingEntry {
                tx,
                is_secret,
                message_id: None,
            },
        );
        rx
    }

    pub fn attach_message_id(&self, id: &str, message_id: i64) {
        let mut map = self.inner.lock().expect("pending decisions mutex poisoned");
        if let Some(entry) = map.get_mut(id) {
            entry.message_id = Some(message_id);
        }
    }

    pub fn remove(&self, id: &str) {
        let mut map = self.inner.lock().expect("pending decisions mutex poisoned");
        map.remove(id);
    }

    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("pending decisions mutex poisoned")
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Resolve a pending request from a verified owner callback button click.
    pub fn resolve_callback(&self, id: &str, action: CallbackAction) -> bool {
        let mut map = self.inner.lock().expect("pending decisions mutex poisoned");
        let Some(entry) = map.get(id) else {
            return false;
        };
        if entry.is_secret && action == CallbackAction::Approve {
            // Secret requests can only be fulfilled by an owner reply, never by button click.
            return false;
        }
        let Some(entry) = map.remove(id) else {
            return false;
        };
        let decision = match action {
            CallbackAction::Approve => TelegramDecision::Approved { secret_value: None },
            CallbackAction::Deny => TelegramDecision::Denied,
        };
        entry.tx.send(decision).is_ok()
    }

    /// Resolve a pending `secret` request from a verified owner reply to `reply_to_message_id`.
    pub fn resolve_secret_reply(&self, reply_to_message_id: i64, value: String) -> bool {
        let mut map = self.inner.lock().expect("pending decisions mutex poisoned");
        let matching_id = map
            .iter()
            .find(|(_, e)| e.is_secret && e.message_id == Some(reply_to_message_id))
            .map(|(k, _)| k.clone());
        let Some(id) = matching_id else {
            return false;
        };
        let Some(entry) = map.remove(&id) else {
            return false;
        };
        entry
            .tx
            .send(TelegramDecision::Approved {
                secret_value: Some(value),
            })
            .is_ok()
    }
}

pub fn escape_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Format a Telegram HTML approval card. Prefixes `[HARD-DENY: только человек]`
/// whenever the Hard-Deny Perimeter triggered.
pub fn format_request_message(
    req: &ApprovalRequest,
    hard_deny: Option<&str>,
    timeout_secs: u64,
) -> String {
    let mut text = String::new();
    if let Some(rule) = hard_deny {
        text.push_str(HARD_DENY_PREFIX);
        text.push_str(&format!(" ({})\n", escape_html(rule)));
    }
    let host = escape_html(req.effective_host());
    let header = match req.kind.as_str() {
        "root" | "approve" => format!("🔐 {host}: root"),
        "confirm" => format!("✅ {host}: подтверждение"),
        "secret" => format!("🔑 {host}: секрет"),
        "cp-console" => format!("🖥️ {host}: консоль"),
        other => format!("⚠️ {host}: {}", escape_html(other)),
    };
    text.push_str(&header);
    if let Some(app) = req.app.as_deref().filter(|s| !s.is_empty()) {
        text.push_str(&format!(" [{}]", escape_html(app)));
    }
    text.push_str(&format!(" ({timeout_secs}s)"));

    let summary = if !req.reason.is_empty() {
        req.reason.as_str()
    } else {
        req.title.as_deref().unwrap_or("")
    };
    if !summary.is_empty() {
        text.push('\n');
        text.push_str(&escape_html(summary));
    }

    match req.kind.as_str() {
        "root" | "approve" => {
            if let Some(script) = &req.script {
                let truncated: String = script.chars().take(3000).collect();
                text.push_str("\n<pre>");
                text.push_str(&escape_html(&truncated));
                text.push_str("</pre>");
            }
        }
        "cp-console" => {
            text.push_str("\n<pre>");
            text.push_str(&escape_html(&req.canonical_payload()));
            text.push_str("</pre>");
        }
        _ => {
            if let Some(body) = &req.text {
                let truncated: String = body.chars().take(3000).collect();
                text.push('\n');
                text.push_str(&escape_html(&truncated));
            }
        }
    }
    if req.kind == "secret" {
        text.push_str("\nОтветьте на это сообщение секретом. Ответ будет удалён.");
    }
    text
}

pub fn parse_callback_data(data: &str) -> Option<(CallbackAction, &str)> {
    if let Some(id) = data.strip_prefix("approve:")
        && !id.is_empty()
    {
        return Some((CallbackAction::Approve, id));
    }
    if let Some(id) = data.strip_prefix("deny:")
        && !id.is_empty()
    {
        return Some((CallbackAction::Deny, id));
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateAction {
    AnswerCallback {
        callback_query_id: String,
        text: &'static str,
    },
    DeleteMessage {
        message_id: i64,
    },
}

/// Process one Telegram `Update` object, strictly verifying `from.id == owner_id`.
pub fn handle_update(
    update: &Value,
    owner_id: i64,
    pending: &PendingDecisions,
) -> Option<UpdateAction> {
    if let Some(cb) = update.get("callback_query") {
        let from_id = cb.pointer("/from/id").and_then(Value::as_i64)?;
        if from_id != owner_id {
            return None;
        }
        let callback_query_id = cb.get("id").and_then(Value::as_str)?.to_string();
        let accepted = cb
            .get("data")
            .and_then(Value::as_str)
            .and_then(parse_callback_data)
            .is_some_and(|(action, id)| pending.resolve_callback(id, action));
        return Some(UpdateAction::AnswerCallback {
            callback_query_id,
            text: if accepted { "" } else { "Запрос уже закрыт" },
        });
    }

    if let Some(msg) = update.get("message") {
        let from_id = msg.pointer("/from/id").and_then(Value::as_i64)?;
        let chat_id = msg.pointer("/chat/id").and_then(Value::as_i64)?;
        if from_id != owner_id || chat_id != owner_id {
            return None;
        }
        let reply_to = msg
            .pointer("/reply_to_message/message_id")
            .and_then(Value::as_i64)?;
        let secret_text = msg.get("text").and_then(Value::as_str)?;
        let msg_id = msg.get("message_id").and_then(Value::as_i64)?;
        if pending.resolve_secret_reply(reply_to, secret_text.to_string()) {
            return Some(UpdateAction::DeleteMessage { message_id: msg_id });
        }
    }

    None
}

pub struct BotApiTelegram {
    client: reqwest::Client,
    bot_token: String,
    owner_id: i64,
    api_base: String,
    sent_texts: Mutex<HashMap<i64, String>>,
}

impl fmt::Debug for BotApiTelegram {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BotApiTelegram")
            .field("bot_token", &"[REDACTED]")
            .field("owner_id", &self.owner_id)
            .field("api_base", &self.api_base)
            .finish()
    }
}

impl BotApiTelegram {
    pub fn new(bot_token: String, owner_id: i64) -> Result<Self> {
        if bot_token.trim().is_empty() {
            bail!("TELEGRAM_BOT_TOKEN must not be empty");
        }
        let client = reqwest::Client::builder()
            .build()
            .context("failed to build Telegram HTTP client")?;
        Ok(Self {
            client,
            bot_token,
            owner_id,
            api_base: TELEGRAM_API_BASE.to_string(),
            sent_texts: Mutex::new(HashMap::new()),
        })
    }

    pub fn with_api_base(mut self, api_base: String) -> Self {
        self.api_base = api_base.trim_end_matches('/').to_string();
        self
    }

    pub fn owner_id(&self) -> i64 {
        self.owner_id
    }

    async fn call_api(&self, method: &str, payload: Value, timeout: Duration) -> Result<Value> {
        let url = format!("{}/bot{}/{method}", self.api_base, self.bot_token);
        let res = self
            .client
            .post(&url)
            .timeout(timeout)
            .json(&payload)
            .send()
            .await
            // Never include reqwest's error URL because it contains the bot token.
            .map_err(|_| anyhow::anyhow!("Telegram API {method} request failed"))?;
        if !res.status().is_success() {
            bail!(
                "Telegram API {method} returned HTTP {}",
                res.status().as_u16()
            );
        }
        let body: Value = res
            .json()
            .await
            .map_err(|_| anyhow::anyhow!("Telegram API {method} returned invalid JSON"))?;
        if body.get("ok").and_then(Value::as_bool) != Some(true) {
            let desc = body
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            bail!("Telegram API {method} error: {desc}");
        }
        Ok(body.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn edit_with_suffix(&self, message_id: i64, suffix: &str) -> Result<()> {
        let original = {
            let mut map = self.sent_texts.lock().expect("sent_texts mutex poisoned");
            map.remove(&message_id)
        };
        let new_text = match original {
            Some(orig) => format!("{orig}\n{}", escape_html(suffix)),
            None => escape_html(suffix),
        };
        self.call_api(
            "editMessageText",
            json!({
                "chat_id": self.owner_id,
                "message_id": message_id,
                "text": new_text,
                "parse_mode": "HTML",
                "reply_markup": { "inline_keyboard": [] }
            }),
            TELEGRAM_CALL_TIMEOUT,
        )
        .await?;
        Ok(())
    }

    /// Long-poll `getUpdates` and route verified owner callbacks and secret replies
    /// into `pending` until `shutdown` signals `true`.
    pub async fn poll_updates_loop(
        &self,
        pending: Arc<PendingDecisions>,
        mut shutdown: watch::Receiver<bool>,
    ) {
        let mut offset: i64 = match self
            .call_api(
                "getUpdates",
                json!({
                    "offset": -1,
                    "timeout": 0,
                    "allowed_updates": ["message", "callback_query"]
                }),
                TELEGRAM_CALL_TIMEOUT,
            )
            .await
        {
            Ok(Value::Array(items)) => items
                .last()
                .and_then(|u| u.get("update_id"))
                .and_then(Value::as_i64)
                .map(|id| id + 1)
                .unwrap_or(0),
            _ => 0,
        };

        let mut backoff_secs = 1u64;
        while !*shutdown.borrow() {
            let poll_fut = self.call_api(
                "getUpdates",
                json!({
                    "offset": offset,
                    "timeout": TELEGRAM_POLL_TIMEOUT_SECS,
                    "allowed_updates": ["message", "callback_query"]
                }),
                TELEGRAM_POLL_HTTP_TIMEOUT,
            );

            let result = tokio::select! {
                _ = shutdown.changed() => break,
                res = poll_fut => res,
            };

            match result {
                Ok(Value::Array(updates)) => {
                    backoff_secs = 1;
                    for update in updates {
                        if let Some(uid) = update.get("update_id").and_then(Value::as_i64) {
                            offset = offset.max(uid + 1);
                        }
                        match handle_update(&update, self.owner_id, &pending) {
                            Some(UpdateAction::AnswerCallback {
                                callback_query_id,
                                text,
                            }) => {
                                let _ = self
                                    .call_api(
                                        "answerCallbackQuery",
                                        json!({
                                            "callback_query_id": callback_query_id,
                                            "text": text,
                                        }),
                                        TELEGRAM_CALL_TIMEOUT,
                                    )
                                    .await;
                            }
                            Some(UpdateAction::DeleteMessage { message_id }) => {
                                let _ = self
                                    .call_api(
                                        "deleteMessage",
                                        json!({
                                            "chat_id": self.owner_id,
                                            "message_id": message_id,
                                        }),
                                        TELEGRAM_CALL_TIMEOUT,
                                    )
                                    .await;
                            }
                            None => {}
                        }
                    }
                }
                _ => {
                    tokio::select! {
                        _ = shutdown.changed() => break,
                        _ = tokio::time::sleep(Duration::from_secs(backoff_secs)) => {}
                    }
                    backoff_secs = (backoff_secs * 2).min(60);
                }
            }
        }
    }
}

impl TelegramTransport for BotApiTelegram {
    async fn send_request(
        &self,
        id: &str,
        req: &ApprovalRequest,
        hard_deny: Option<&str>,
        timeout_secs: u64,
    ) -> Result<i64> {
        let text = format_request_message(req, hard_deny, timeout_secs);
        let inline_keyboard = if req.kind == "secret" {
            json!([[{ "text": "Отказать", "callback_data": format!("deny:{id}") }]])
        } else {
            json!([[
                { "text": "Разрешить", "callback_data": format!("approve:{id}") },
                { "text": "Отказать", "callback_data": format!("deny:{id}") }
            ]])
        };
        let result = self
            .call_api(
                "sendMessage",
                json!({
                    "chat_id": self.owner_id,
                    "text": text,
                    "parse_mode": "HTML",
                    "reply_markup": { "inline_keyboard": inline_keyboard }
                }),
                TELEGRAM_CALL_TIMEOUT,
            )
            .await?;
        let message_id = result
            .get("message_id")
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow::anyhow!("Telegram sendMessage response missing message_id"))?;
        self.sent_texts
            .lock()
            .expect("sent_texts mutex poisoned")
            .insert(message_id, text);
        Ok(message_id)
    }

    async fn cancel_request(&self, message_id: i64, reason: &str) -> Result<()> {
        self.edit_with_suffix(message_id, reason).await
    }

    async fn finalize_request(&self, message_id: i64, outcome_text: &str) -> Result<()> {
        self.edit_with_suffix(message_id, outcome_text).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_update_strictly_verifies_owner_id() {
        let pending = PendingDecisions::new();
        let mut rx = pending.register("req-1".into(), false);

        // Non-owner callback must be ignored and leave pending unresolved.
        let intruder = json!({
            "update_id": 1,
            "callback_query": {
                "id": "cb-1",
                "from": {"id": 9999},
                "data": "approve:req-1"
            }
        });
        assert!(handle_update(&intruder, 12345, &pending).is_none());
        assert!(rx.try_recv().is_err());
        assert_eq!(pending.len(), 1);

        // Owner callback resolves immediately.
        let owner = json!({
            "update_id": 2,
            "callback_query": {
                "id": "cb-2",
                "from": {"id": 12345},
                "data": "approve:req-1"
            }
        });
        let ack = handle_update(&owner, 12345, &pending);
        assert_eq!(
            ack,
            Some(UpdateAction::AnswerCallback {
                callback_query_id: "cb-2".into(),
                text: ""
            })
        );
        assert_eq!(
            rx.try_recv().unwrap(),
            TelegramDecision::Approved { secret_value: None }
        );

        // Duplicate click after resolution reports already closed.
        let dup = handle_update(&owner, 12345, &pending);
        assert_eq!(
            dup,
            Some(UpdateAction::AnswerCallback {
                callback_query_id: "cb-2".into(),
                text: "Запрос уже закрыт"
            })
        );
    }

    #[test]
    fn hard_deny_prefix_appears_only_when_hard_denied() {
        let req = ApprovalRequest {
            kind: "root".into(),
            reason: "check logs".into(),
            script: Some("journalctl -n 20".into()),
            text: None,
            title: None,
            target: None,
            line: None,
            host: Some("homeserver".into()),
            app: None,
            thread_id: None,
            timeout_secs: None,
            nonce: None,
        };
        let normal = format_request_message(&req, None, 120);
        assert!(!normal.contains(HARD_DENY_PREFIX));

        let denied = format_request_message(&req, Some("secret_exfiltration"), 120);
        assert!(denied.starts_with(HARD_DENY_PREFIX));
    }
}
