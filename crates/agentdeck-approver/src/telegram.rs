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
use tokio::time::Instant;

use crate::perimeter::ApprovalRequest;

pub const HARD_DENY_PREFIX: &str = "[HARD-DENY: только человек]";
pub const FINALIZE_APPROVED: &str = "— разрешено ✅";
pub const FINALIZE_DENIED: &str = "— отказано ❌";
pub const FINALIZE_TIMEOUT: &str = "— время вышло ⌛";
const INLINE_PAYLOAD_MAX: usize = 3000;
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
    deadline: Option<Instant>,
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
        self.register_until(id, is_secret, None)
    }

    pub(crate) fn register_until(
        &self,
        id: String,
        is_secret: bool,
        deadline: Option<Instant>,
    ) -> oneshot::Receiver<TelegramDecision> {
        let (tx, rx) = oneshot::channel();
        let mut map = self.inner.lock().expect("pending decisions mutex poisoned");
        map.insert(
            id,
            PendingEntry {
                deadline,
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
        if (entry.is_secret && action == CallbackAction::Approve)
            || entry
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
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
            .find(|(_, e)| {
                e.is_secret
                    && e.message_id == Some(reply_to_message_id)
                    && e.deadline.is_none_or(|deadline| Instant::now() < deadline)
            })
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
    format_request_card(req, hard_deny, timeout_secs, false)
}

fn format_request_card(
    req: &ApprovalRequest,
    hard_deny: Option<&str>,
    timeout_secs: u64,
    attached: bool,
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

    if attached {
        text.push_str("\nПолный текст запроса во вложении.");
    } else {
        match req.kind.as_str() {
            "root" | "approve" | "cp-console" => {
                text.push_str("\n<pre>");
                text.push_str(&escape_html(&req.canonical_payload()));
                text.push_str("</pre>");
            }
            _ => {
                if let Some(body) = &req.text {
                    text.push('\n');
                    text.push_str(&escape_html(body));
                }
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
            text: if accepted {
                ""
            } else {
                "Запрос уже закрыт"
            },
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

    fn api_request(&self, method: &str) -> reqwest::RequestBuilder {
        let url = format!("{}/bot{}/{method}", self.api_base, self.bot_token);
        self.client.post(url)
    }

    async fn call_api(
        &self,
        method: &'static str,
        payload: Value,
        timeout: Duration,
    ) -> Result<Value> {
        send_api(method, self.api_request(method).json(&payload), timeout).await
    }

    async fn send_document(&self, payload: &str, filename: &str, hash: &str) -> Result<i64> {
        let mut boundary = format!("agentdeck-{}", &hash[..32]);
        while payload.contains(&boundary) {
            boundary.push('0');
        }
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"chat_id\"\r\n\r\n{}\r\n\
             --{boundary}\r\nContent-Disposition: form-data; name=\"document\"; filename=\"{filename}\"\r\n\
             Content-Type: application/octet-stream\r\n\r\n{payload}\r\n--{boundary}--\r\n",
            self.owner_id,
        );
        let request = self
            .api_request("sendDocument")
            .header(
                "Content-Type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(body);
        let result = send_api("sendDocument", request, TELEGRAM_CALL_TIMEOUT).await?;
        message_id(&result, "sendDocument")
    }

    async fn edit_with_suffix(&self, message_id: i64, suffix: &str) -> Result<()> {
        let original = self
            .sent_texts
            .lock()
            .expect("sent_texts mutex poisoned")
            .remove(&message_id);
        let mut payload = json!({
            "chat_id": self.owner_id,
            "message_id": message_id,
            "reply_markup": { "inline_keyboard": [] }
        });
        let method = if let Some(original) = original
            .filter(|text| text.encode_utf16().count() + suffix.encode_utf16().count() < 4096)
        {
            payload["text"] = json!(format!("{original}\n{}", escape_html(suffix)));
            payload["parse_mode"] = json!("HTML");
            "editMessageText"
        } else {
            // Remove the keyboard after a failed edit or when the status might exceed Telegram's text limit.
            "editMessageReplyMarkup"
        };
        self.call_api(method, payload, TELEGRAM_CALL_TIMEOUT)
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

#[derive(Debug)]
struct ApiFailure {
    method: &'static str,
    code: Option<i64>,
}

impl fmt::Display for ApiFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Telegram API {} failed", self.method)?;
        if let Some(code) = self.code {
            write!(f, " (error_code={code})")?;
        }
        Ok(())
    }
}

impl std::error::Error for ApiFailure {}

pub(crate) fn retryable_edit_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ApiFailure>()
        .and_then(|e| e.code)
        .is_none_or(|code| code == 408 || code == 429 || (500..600).contains(&code))
}

pub(crate) fn log_api_failure(stage: &str, err: &anyhow::Error) {
    if let Some(failure) = err.downcast_ref::<ApiFailure>() {
        eprintln!("approver {stage} failed: {failure}");
    } else {
        eprintln!("approver {stage} failed");
    }
}

async fn send_api(
    method: &'static str,
    request: reqwest::RequestBuilder,
    timeout: Duration,
) -> Result<Value> {
    let res = request
        .timeout(timeout)
        .send()
        .await
        // The reqwest error URL contains the bot token.
        .map_err(|_| ApiFailure { method, code: None })?;
    let status = res.status();
    let body = res.json::<Value>().await;
    if status.is_success() {
        let body = body
            .as_ref()
            .map_err(|_| ApiFailure { method, code: None })?;
        if body.get("ok").and_then(Value::as_bool) == Some(true) {
            return Ok(body.get("result").cloned().unwrap_or(Value::Null));
        }
    }
    let code = body
        .ok()
        .and_then(|body| body.get("error_code").and_then(Value::as_i64))
        .unwrap_or_else(|| i64::from(status.as_u16()));
    Err(ApiFailure {
        method,
        code: Some(code),
    }
    .into())
}

fn message_id(result: &Value, method: &'static str) -> Result<i64> {
    result
        .get("message_id")
        .and_then(Value::as_i64)
        .ok_or_else(|| ApiFailure { method, code: None }.into())
}

impl TelegramTransport for BotApiTelegram {
    async fn send_request(
        &self,
        id: &str,
        req: &ApprovalRequest,
        hard_deny: Option<&str>,
        timeout_secs: u64,
    ) -> Result<i64> {
        let payload = req.canonical_payload();
        let document_id = if payload.chars().nth(INLINE_PAYLOAD_MAX).is_some() {
            let filename = if matches!(req.kind.as_str(), "root" | "approve") {
                "script.sh"
            } else {
                "request.txt"
            };
            Some(
                self.send_document(&payload, filename, &req.payload_sha256())
                    .await?,
            )
        } else {
            None
        };
        let text = format_request_card(req, hard_deny, timeout_secs, document_id.is_some());
        let inline_keyboard = if req.kind == "secret" {
            json!([[{ "text": "Отказать", "callback_data": format!("deny:{id}") }]])
        } else {
            json!([[
                { "text": "Разрешить", "callback_data": format!("approve:{id}") },
                { "text": "Отказать", "callback_data": format!("deny:{id}") }
            ]])
        };
        let mut card = json!({
            "chat_id": self.owner_id,
            "text": text,
            "parse_mode": "HTML",
            "reply_markup": { "inline_keyboard": inline_keyboard }
        });
        if let Some(id) = document_id {
            card["reply_parameters"] = json!({"message_id": id});
        }
        let result = self
            .call_api("sendMessage", card, TELEGRAM_CALL_TIMEOUT)
            .await?;
        let message_id = message_id(&result, "sendMessage")?;
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

    async fn mock_api(
        responses: Vec<(&'static str, Value)>,
    ) -> (
        BotApiTelegram,
        tokio::task::JoinHandle<Vec<(String, String)>>,
    ) {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut bot = BotApiTelegram::new("fake-token".into(), 12345)
            .unwrap()
            .with_api_base(format!("http://{}", listener.local_addr().unwrap()));
        bot.client = reqwest::Client::builder().no_proxy().build().unwrap();
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, response) in responses {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = BufReader::new(stream);
                let mut first = String::new();
                stream.read_line(&mut first).await.unwrap();
                let mut size = 0;
                loop {
                    let mut line = String::new();
                    stream.read_line(&mut line).await.unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        size = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; size];
                stream.read_exact(&mut body).await.unwrap();
                let response = response.to_string();
                stream.write_all(format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len(),
                ).as_bytes()).await.unwrap();
                requests.push((first, String::from_utf8(body).unwrap()));
            }
            requests
        });
        (bot, task)
    }

    #[tokio::test]
    async fn oversized_script_is_uploaded_in_full_before_the_linked_card() {
        let (bot, api) = mock_api(vec![
            ("200 OK", json!({"ok": true, "result": {"message_id": 51}})),
            ("200 OK", json!({"ok": true, "result": {"message_id": 52}})),
        ])
        .await;
        let script = format!("{}\nprintf 'tail'\n", "x".repeat(3600));
        let req: ApprovalRequest = serde_json::from_value(json!({
            "kind": "root", "script": script,
        }))
        .unwrap();
        assert_eq!(bot.send_request("req-1", &req, None, 90).await.unwrap(), 52);
        let requests = api.await.unwrap();
        assert!(requests[0].0.contains("/sendDocument "));
        assert!(requests[0].1.contains("filename=\"script.sh\""));
        assert!(requests[0].1.contains(&format!("\r\n\r\n{script}\r\n--")));
        assert!(requests[1].0.contains("/sendMessage "));
        let card: Value = serde_json::from_str(&requests[1].1).unwrap();
        assert_eq!(card["reply_parameters"]["message_id"], 51);
        assert_eq!(
            card["reply_markup"]["inline_keyboard"][0][0]["callback_data"],
            "approve:req-1"
        );
    }

    #[tokio::test]
    async fn upload_failure_sends_no_approval_card_and_redacts_api_description() {
        for status in ["200 OK", "400 Bad Request"] {
            let (bot, api) = mock_api(vec![(
                status,
                json!({"ok": false, "error_code": 413, "description": "echoed fake-token"}),
            )])
            .await;
            let req: ApprovalRequest = serde_json::from_value(json!({
                "kind": "root", "script": "x".repeat(3600),
            }))
            .unwrap();
            let err = bot.send_request("req-1", &req, None, 90).await.unwrap_err();
            assert_eq!(
                err.to_string(),
                "Telegram API sendDocument failed (error_code=413)"
            );
            let requests = api.await.unwrap();
            assert_eq!(requests.len(), 1);
            assert!(requests[0].0.contains("/sendDocument "));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn pending_registry_rejects_callbacks_and_secret_replies_at_the_deadline() {
        let pending = PendingDecisions::new();
        let delay = Duration::from_secs(1);
        let deadline = Instant::now() + delay;
        let mut callback = pending.register_until("card".into(), false, Some(deadline));
        let mut secret = pending.register_until("secret".into(), true, Some(deadline));
        pending.attach_message_id("secret", 1);

        tokio::time::advance(delay).await;

        assert!(!pending.resolve_callback("card", CallbackAction::Approve));
        assert!(!pending.resolve_secret_reply(1, "fixture".into()));
        assert!(callback.try_recv().is_err());
        assert!(secret.try_recv().is_err());
    }

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
