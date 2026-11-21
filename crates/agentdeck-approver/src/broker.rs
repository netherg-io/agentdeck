//! Core approval state machine and Unix socket broker server (ADR-0001).
//!
//! Wire-compatible with `approve-client.py`: accepts one JSON request line,
//! emits periodic `{"type":"pending","id":...,"elapsed_secs":...}` keepalives,
//! cancels Telegram buttons immediately on client disconnect before approval,
//! enforces strict pre-execution timeout, and returns Opaque Approval results.

use std::fs;
use std::future::{Future, poll_fn, ready};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::audit::{AuditRecord, AuditSource, append_record, unix_timestamp_secs};
use crate::config::ApproverConfig;
use crate::judge::{PolicyJudge, is_auto_approved};
use crate::perimeter::{
    ApprovalRequest, MandateContext, PerimeterVerdict, check_perimeter_with_extra, hex_lower,
};
use crate::telegram::{
    FINALIZE_APPROVED, FINALIZE_DENIED, FINALIZE_TIMEOUT, PendingDecisions, TelegramDecision,
    TelegramTransport, log_api_failure, retryable_edit_error,
};

pub const MAX_REQUEST_BYTES: usize = 256 * 1024;
pub const DEFAULT_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
const CANCEL_CLIENT_DISCONNECTED: &str = "— отменено (клиент отключился)";
const CANCEL_BROKER_SHUTDOWN: &str = "— отменено (брокер остановлен)";

/// Opaque broker outcome. Deliberately omits whether PolicyJudge or Telegram
/// approved, so wire serialization cannot leak the approval source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrokerOutcome {
    Approved { secret_value: Option<String> },
    Denied,
    NeedsOwner,
    ClientCancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerDecision {
    pub request_id: String,
    pub outcome: BrokerOutcome,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingEvent {
    pub id: String,
    pub elapsed_secs: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct BrokerTimings {
    pub keepalive_interval: Duration,
    /// Optional exact pre-execution timeout override for deterministic tests.
    pub timeout_override: Option<Duration>,
}

impl Default for BrokerTimings {
    fn default() -> Self {
        Self {
            keepalive_interval: DEFAULT_KEEPALIVE_INTERVAL,
            timeout_override: None,
        }
    }
}

pub struct BrokerContext<'a> {
    pub config: &'a ApproverConfig,
    pub mandate: Option<&'a MandateContext>,
    pub pending: &'a PendingDecisions,
    pub audit_path: Option<&'a Path>,
    pub timings: BrokerTimings,
}

pub fn format_wire_pending(event: &PendingEvent) -> String {
    json!({
        "type": "pending",
        "id": event.id,
        "elapsed_secs": event.elapsed_secs,
    })
    .to_string()
}

/// Format the opaque final wire line compatible with `approve-client.py` and
/// `relay.py`. Output is identical whether PolicyJudge or Telegram approved.
pub fn format_wire_result(outcome: &BrokerOutcome, req: &ApprovalRequest) -> String {
    match outcome {
        BrokerOutcome::Approved { secret_value } => {
            let mut obj = serde_json::Map::new();
            obj.insert("type".into(), json!("result"));
            obj.insert("result".into(), json!("approved"));
            if req.kind == "approve" || req.nonce.is_some() {
                if let Some(nonce) = &req.nonce {
                    obj.insert("nonce".into(), json!(nonce));
                }
                obj.insert("sha256".into(), json!(req.payload_sha256()));
            }
            if req.kind == "secret"
                && let Some(val) = secret_value
            {
                obj.insert("value".into(), json!(val));
            }
            serde_json::Value::Object(obj).to_string()
        }
        BrokerOutcome::Denied => json!({
            "type": "result",
            "result": "denied",
        })
        .to_string(),
        BrokerOutcome::NeedsOwner => json!({
            "type": "result",
            "result": "timeout",
            "needs_owner": true,
        })
        .to_string(),
        BrokerOutcome::ClientCancelled => json!({
            "type": "result",
            "result": "error",
            "error": "client disconnected",
        })
        .to_string(),
    }
}

fn generate_request_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let mut entropy = [0u8; 8];
    if fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut entropy))
        .is_ok()
    {
        return hex_lower(&entropy);
    }
    let mut hasher = Sha256::new();
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    hasher.update(unix_timestamp_secs().to_le_bytes());
    let digest = hasher.finalize();
    hex_lower(&digest[..8])
}

fn resolve_timeout(
    req: &ApprovalRequest,
    config: &ApproverConfig,
    autonomous: bool,
    timings: BrokerTimings,
) -> (Duration, u64) {
    if let Some(ov) = timings.timeout_override {
        return (ov, ov.as_secs().max(1));
    }
    let secs = if autonomous {
        req.timeout_secs
            .map(|t| t.clamp(1, config.autonomous_timeout_secs))
            .unwrap_or(config.autonomous_timeout_secs)
    } else {
        req.timeout_secs
            .map(|t| t.clamp(1, 1800))
            .unwrap_or(config.interactive_timeout_secs)
    };
    (Duration::from_secs(secs), secs)
}

#[allow(clippy::too_many_arguments)]
fn record_audit(
    audit_path: Option<&Path>,
    request_id: &str,
    req: &ApprovalRequest,
    perimeter: &PerimeterVerdict,
    judge_allow: Option<bool>,
    judge_confidence: Option<f64>,
    judge_reason: Option<String>,
    outcome: &str,
    source: AuditSource,
    elapsed_ms: u64,
) {
    let Some(path) = audit_path else {
        return;
    };
    let record = AuditRecord {
        timestamp: unix_timestamp_secs(),
        request_id: request_id.to_string(),
        host: req.effective_host().to_string(),
        kind: req.kind.clone(),
        sha256_payload: req.payload_sha256(),
        thread_id: req.thread_id.clone(),
        perimeter: perimeter.audit_label(),
        judge_allow,
        judge_confidence,
        judge_reason,
        outcome: outcome.to_string(),
        source,
        elapsed_ms,
    };
    if let Err(e) = append_record(path, &record) {
        eprintln!("approver audit log write failed: {e:#}");
    }
}

/// Evaluate a request while sending the same keepalives for both approval paths.
pub async fn evaluate_and_wait<J, T, F>(
    req: &ApprovalRequest,
    ctx: &BrokerContext<'_>,
    judge: &J,
    telegram: &Arc<T>,
    client_disconnected: F,
    pending_tx: Option<mpsc::UnboundedSender<PendingEvent>>,
) -> Result<BrokerDecision>
where
    J: PolicyJudge,
    T: TelegramTransport + 'static,
    F: Future<Output = ()> + Send,
{
    req.validate()?;
    let request_id = generate_request_id();
    let started = Instant::now();
    let evaluation = evaluate(
        req,
        ctx,
        judge,
        telegram,
        async {
            tokio::select! {
                _ = client_disconnected => {}
                _ = async {
                    match &pending_tx {
                        Some(tx) => tx.closed().await,
                        None => std::future::pending::<()>().await,
                    }
                } => {}
            }
        },
        &request_id,
        started,
    );
    let Some(tx) = &pending_tx else {
        return evaluation.await;
    };

    let _ = tx.send(PendingEvent {
        id: request_id.clone(),
        elapsed_secs: 0,
    });
    let step = ctx.timings.keepalive_interval.max(Duration::from_millis(5));
    let mut keepalive = tokio::time::interval_at(started + step, step);
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tokio::pin!(evaluation);
    loop {
        tokio::select! {
            biased;
            decision = &mut evaluation => return decision,
            _ = keepalive.tick() => {
                let _ = tx.send(PendingEvent {
                    id: request_id.clone(),
                    elapsed_secs: started.elapsed().as_secs(),
                });
            }
        }
    }
}

async fn disconnect_ready(disconnect: Pin<&mut impl Future<Output = ()>>) -> bool {
    tokio::select! {
        biased;
        () = disconnect => true,
        () = ready(()) => false,
    }
}

async fn evaluate<J, T, F>(
    req: &ApprovalRequest,
    ctx: &BrokerContext<'_>,
    judge: &J,
    telegram: &Arc<T>,
    client_disconnected: F,
    request_id: &str,
    started: Instant,
) -> Result<BrokerDecision>
where
    J: PolicyJudge,
    T: TelegramTransport + 'static,
    F: Future<Output = ()> + Send,
{
    let mandate = ctx.mandate.filter(|m| {
        m.thread_id
            .as_ref()
            .is_none_or(|id| !id.trim().is_empty() && req.thread_id.as_ref() == Some(id))
    });
    let autonomous = mandate.is_some_and(|m| m.active_autonomous_run);
    let perimeter = check_perimeter_with_extra(req, mandate, &ctx.config.extra_hard_deny_patterns);
    let complete = |outcome, source, label, (allow, confidence, reason)| {
        let elapsed_ms = started.elapsed().as_millis() as u64;
        record_audit(
            ctx.audit_path,
            request_id,
            req,
            &perimeter,
            allow,
            confidence,
            reason,
            label,
            source,
            elapsed_ms,
        );
        Ok(BrokerDecision {
            request_id: request_id.into(),
            outcome,
            elapsed_ms,
        })
    };

    tokio::pin!(client_disconnected);
    let mut judge_fields = (None, None, None);
    if let Some(mandate) =
        mandate.filter(|m| autonomous && m.owner_granted && !perimeter.is_hard_deny())
    {
        let result = tokio::select! {
            biased;
            _ = &mut client_disconnected => return complete(
                BrokerOutcome::ClientCancelled, AuditSource::ClientCancelled, "client_cancelled", judge_fields,
            ),
            result = judge.evaluate(req, mandate) => result,
        };
        match result {
            Ok(verdict) => {
                let approved = is_auto_approved(&verdict, ctx.config.judge_confidence_threshold);
                judge_fields = (
                    Some(verdict.allow),
                    verdict.confidence.is_finite().then_some(verdict.confidence),
                    Some(verdict.reason),
                );
                if approved {
                    if disconnect_ready(client_disconnected.as_mut()).await {
                        return complete(
                            BrokerOutcome::ClientCancelled,
                            AuditSource::ClientCancelled,
                            "client_cancelled",
                            judge_fields,
                        );
                    }
                    return complete(
                        BrokerOutcome::Approved { secret_value: None },
                        AuditSource::PolicyJudge,
                        "approved",
                        judge_fields,
                    );
                }
            }
            Err(err) => {
                judge_fields = (Some(false), Some(0.0), Some(format!("judge_error: {err}")))
            }
        }
    }

    let (timeout, display_secs) = resolve_timeout(req, ctx.config, autonomous, ctx.timings);
    let deadline = Instant::now() + timeout;
    let mut rx =
        ctx.pending
            .register_until(request_id.into(), req.kind == "secret", Some(deadline));
    let sender = Arc::clone(telegram);
    let sent_req = req.clone();
    let sent_id = request_id.to_owned();
    let rule = perimeter.rule();
    let mut delivery = Box::pin(async move {
        sender
            .send_request(&sent_id, &sent_req, rule, display_secs)
            .await
    });

    let mut delivery_started = false;
    let mut message_id = None;
    let (outcome, status) = 'wait: {
        let delivered = tokio::select! {
            biased;
            _ = &mut client_disconnected => break 'wait (BrokerOutcome::ClientCancelled, CANCEL_CLIENT_DISCONNECTED),
            _ = tokio::time::sleep_until(deadline) => break 'wait (BrokerOutcome::NeedsOwner, FINALIZE_TIMEOUT),
            result = async {
                delivery_started = true;
                delivery.as_mut().await
            } => result,
        };
        let id = match delivered {
            Ok(id) => id,
            Err(err) => {
                ctx.pending.remove(request_id);
                log_api_failure("delivery", &err);
                bail!("approval service unavailable");
            }
        };
        message_id = Some(id);
        ctx.pending.attach_message_id(request_id, id);

        let decision = tokio::select! {
            biased;
            _ = &mut client_disconnected => break 'wait (BrokerOutcome::ClientCancelled, CANCEL_CLIENT_DISCONNECTED),
            _ = tokio::time::sleep_until(deadline) => break 'wait (BrokerOutcome::NeedsOwner, FINALIZE_TIMEOUT),
            result = &mut rx => result,
        };
        if disconnect_ready(client_disconnected.as_mut()).await {
            break 'wait (BrokerOutcome::ClientCancelled, CANCEL_CLIENT_DISCONNECTED);
        }
        if Instant::now() >= deadline {
            break 'wait (BrokerOutcome::NeedsOwner, FINALIZE_TIMEOUT);
        }
        match decision {
            Ok(TelegramDecision::Approved { secret_value }) => {
                (BrokerOutcome::Approved { secret_value }, FINALIZE_APPROVED)
            }
            Ok(TelegramDecision::Denied) => (BrokerOutcome::Denied, FINALIZE_DENIED),
            Err(_) => (BrokerOutcome::ClientCancelled, CANCEL_BROKER_SHUTDOWN),
        }
    };

    ctx.pending.remove(request_id);
    let cancelled = outcome == BrokerOutcome::ClientCancelled;
    if delivery_started {
        let telegram = Arc::clone(telegram);
        tokio::spawn(async move {
            let id = match message_id {
                Some(id) => id,
                None => match delivery.await {
                    Ok(id) => id,
                    Err(err) => {
                        log_api_failure("delivery", &err);
                        return;
                    }
                },
            };
            let edit = || async {
                if cancelled {
                    telegram.cancel_request(id, status).await
                } else {
                    telegram.finalize_request(id, status).await
                }
            };
            if let Err(err) = edit().await {
                let result = if retryable_edit_error(&err) {
                    edit().await
                } else {
                    Err(err)
                };
                if let Err(err) = result {
                    log_api_failure("card cleanup", &err);
                }
            }
        });
    }

    let (source, label) = match outcome {
        BrokerOutcome::Approved { .. } => (AuditSource::Telegram, "approved"),
        BrokerOutcome::Denied => (AuditSource::Denied, "denied"),
        BrokerOutcome::NeedsOwner => (AuditSource::Timeout, "needs_owner"),
        BrokerOutcome::ClientCancelled => (AuditSource::ClientCancelled, "client_cancelled"),
    };
    complete(outcome, source, label, judge_fields)
}

/// Returns `true` when the peer has fully closed the Unix stream (`POLLHUP`/`POLLERR`),
/// distinguishing full disconnect from a half-close (`shutdown(SHUT_WR)`).
fn is_peer_hup(fd: i32) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: 0,
        revents: 0,
    };
    let rc = unsafe { libc::poll(&mut pfd, 1, 0) };
    rc > 0 && (pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL)) != 0
}

async fn wait_client_disconnect(reader: &tokio::net::unix::OwnedReadHalf) {
    let fd = reader.as_ref().as_raw_fd();
    let wait = async {
        let mut scratch = [0u8; 64];
        loop {
            if reader.readable().await.is_err() {
                return;
            }
            match reader.try_read(&mut scratch) {
                Ok(0) => break,
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(_) => return,
            }
        }

        // SHUT_WR clients can still read. Probe for their eventual full close.
        while !is_peer_hup(fd) {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    };

    tokio::pin!(wait);
    poll_fn(|cx| {
        // An owner decision can wake the broker before the next half-close probe.
        if is_peer_hup(fd) {
            std::task::Poll::Ready(())
        } else {
            wait.as_mut().poll(cx)
        }
    })
    .await;
}

/// Local Unix socket server implementing the line-JSON protocol of `approve-client.py`.
pub struct UnixBrokerServer<J, T> {
    listener: UnixListener,
    socket_path: PathBuf,
    config: ApproverConfig,
    mandate: Arc<RwLock<Option<MandateContext>>>,
    judge: Arc<J>,
    telegram: Arc<T>,
    pending: Arc<PendingDecisions>,
    audit_path: Option<PathBuf>,
    timings: BrokerTimings,
}

impl<J, T> UnixBrokerServer<J, T>
where
    J: PolicyJudge + 'static,
    T: TelegramTransport + 'static,
{
    #[allow(clippy::too_many_arguments)]
    pub fn bind(
        socket_path: PathBuf,
        config: ApproverConfig,
        mandate: Arc<RwLock<Option<MandateContext>>>,
        judge: Arc<J>,
        telegram: Arc<T>,
        pending: Arc<PendingDecisions>,
        audit_path: Option<PathBuf>,
    ) -> Result<Self> {
        if let Some(parent) = socket_path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create socket parent dir {}", parent.display())
            })?;
        }
        match fs::symlink_metadata(&socket_path) {
            Ok(meta) => {
                if !meta.file_type().is_socket() {
                    bail!(
                        "approver socket path is not a socket: {}",
                        socket_path.display()
                    );
                }
                match std::os::unix::net::UnixStream::connect(&socket_path) {
                    Ok(_) => bail!(
                        "approver socket already active at {}",
                        socket_path.display()
                    ),
                    Err(err) if err.kind() == std::io::ErrorKind::ConnectionRefused => {
                        let current = fs::symlink_metadata(&socket_path)?;
                        if !current.file_type().is_socket()
                            || (current.dev(), current.ino()) != (meta.dev(), meta.ino())
                        {
                            bail!("approver socket path changed: {}", socket_path.display());
                        }
                        fs::remove_file(&socket_path)?;
                    }
                    Err(err) => {
                        return Err(err).with_context(|| {
                            format!("failed to probe approver socket {}", socket_path.display())
                        });
                    }
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "failed to inspect approver socket {}",
                        socket_path.display()
                    )
                });
            }
        }
        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("failed to bind Unix socket {}", socket_path.display()))?;
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to set 0600 on {}", socket_path.display()))?;
        Ok(Self {
            listener,
            socket_path,
            config,
            mandate,
            judge,
            telegram,
            pending,
            audit_path,
            timings: BrokerTimings::default(),
        })
    }

    pub fn with_timings(mut self, timings: BrokerTimings) -> Self {
        self.timings = timings;
        self
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub async fn run(self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        let socket_path = self.socket_path.clone();
        let config = Arc::new(self.config);
        let audit_path = Arc::new(self.audit_path);
        let timings = self.timings;

        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                accepted = self.listener.accept() => {
                    let (stream, _) = match accepted {
                        Ok(pair) => pair,
                        Err(e) => {
                            eprintln!("approver socket accept error: {e}");
                            continue;
                        }
                    };
                    let config = Arc::clone(&config);
                    let mandate = Arc::clone(&self.mandate);
                    let judge = Arc::clone(&self.judge);
                    let telegram = Arc::clone(&self.telegram);
                    let pending = Arc::clone(&self.pending);
                    let audit_path = Arc::clone(&audit_path);
                    tokio::spawn(async move {
                        handle_connection(
                            stream,
                            config,
                            mandate,
                            judge,
                            telegram,
                            pending,
                            audit_path,
                            timings,
                        )
                        .await;
                    });
                }
            }
        }

        let _ = fs::remove_file(&socket_path);
        Ok(())
    }
}

async fn write_line(writer: &mut tokio::net::unix::OwnedWriteHalf, line: &str) -> bool {
    let mut bytes = Vec::with_capacity(line.len() + 1);
    bytes.extend_from_slice(line.as_bytes());
    bytes.push(b'\n');
    writer.write_all(&bytes).await.is_ok() && writer.flush().await.is_ok()
}

#[allow(clippy::too_many_arguments)]
async fn handle_connection<J, T>(
    stream: UnixStream,
    config: Arc<ApproverConfig>,
    mandate: Arc<RwLock<Option<MandateContext>>>,
    judge: Arc<J>,
    telegram: Arc<T>,
    pending: Arc<PendingDecisions>,
    audit_path: Arc<Option<PathBuf>>,
    timings: BrokerTimings,
) where
    J: PolicyJudge + 'static,
    T: TelegramTransport + 'static,
{
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let read_res = tokio::time::timeout(Duration::from_secs(15), async {
        let mut buf = Vec::new();
        loop {
            let chunk = reader.fill_buf().await?;
            if chunk.is_empty() {
                break;
            }
            if let Some(nl) = chunk.iter().position(|&b| b == b'\n') {
                buf.extend_from_slice(&chunk[..=nl]);
                reader.consume(nl + 1);
                break;
            }
            buf.extend_from_slice(chunk);
            let len = chunk.len();
            reader.consume(len);
            if buf.len() > MAX_REQUEST_BYTES + 1 {
                break;
            }
        }
        Ok::<Vec<u8>, std::io::Error>(buf)
    })
    .await;

    let raw = match read_res {
        Ok(Ok(buf)) => buf,
        _ => return,
    };
    if raw.is_empty() {
        return;
    }
    if raw.len() > MAX_REQUEST_BYTES + 1 || !raw.ends_with(b"\n") {
        let _ = write_line(
            &mut write_half,
            &json!({"type": "result", "result": "error", "error": "oversize or incomplete request"})
                .to_string(),
        )
        .await;
        return;
    }

    let req: ApprovalRequest = match serde_json::from_slice(&raw) {
        Ok(r) => r,
        Err(_) => {
            let _ = write_line(
                &mut write_half,
                &json!({"type": "result", "result": "error", "error": "malformed request"})
                    .to_string(),
            )
            .await;
            return;
        }
    };
    if let Err(e) = req.validate() {
        let _ = write_line(
            &mut write_half,
            &json!({"type": "result", "result": "error", "error": e.to_string()}).to_string(),
        )
        .await;
        return;
    }

    let read_half = reader.into_inner();
    let (pending_tx, mut pending_rx) = mpsc::unbounded_channel::<PendingEvent>();
    let (write_dead_tx, write_dead_rx) = tokio::sync::oneshot::channel::<()>();

    // Forward keepalive lines to the client; if the client socket breaks, signal disconnect.
    let writer_task = tokio::spawn(async move {
        while let Some(ev) = pending_rx.recv().await {
            if !write_line(&mut write_half, &format_wire_pending(&ev)).await {
                let _ = write_dead_tx.send(());
                return write_half;
            }
        }
        write_half
    });

    let disconnect_fut = async move {
        tokio::select! {
            _ = wait_client_disconnect(&read_half) => {}
            _ = write_dead_rx => {}
        }
    };

    let mandate_snapshot = mandate.read().expect("mandate rwlock poisoned").clone();
    let ctx = BrokerContext {
        config: &config,
        mandate: mandate_snapshot.as_ref(),
        pending: &pending,
        audit_path: audit_path.as_deref(),
        timings,
    };

    let decision = evaluate_and_wait(
        &req,
        &ctx,
        judge.as_ref(),
        &telegram,
        disconnect_fut,
        Some(pending_tx),
    )
    .await;

    // `pending_tx` was moved into `evaluate_and_wait` and is now dropped, so `writer_task` exits.
    let Ok(mut write_half) = writer_task.await else {
        return;
    };

    match decision {
        Ok(d) => {
            if d.outcome != BrokerOutcome::ClientCancelled {
                let line = format_wire_result(&d.outcome, &req);
                let _ = write_line(&mut write_half, &line).await;
            }
        }
        Err(_) => {
            let line = json!({
                "type": "result",
                "result": "error",
                "error": "approval service unavailable",
            })
            .to_string();
            let _ = write_line(&mut write_half, &line).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_full_close_is_detected_between_half_close_probes() {
        let (mut client, broker) = UnixStream::pair().unwrap();
        client.shutdown().await.unwrap();
        let (reader, _writer) = broker.into_split();
        reader.readable().await.unwrap();

        let disconnect = wait_client_disconnect(&reader);
        tokio::pin!(disconnect);
        assert!(!disconnect_ready(disconnect.as_mut()).await);
        drop(client);
        assert!(disconnect_ready(disconnect.as_mut()).await);
    }
}
