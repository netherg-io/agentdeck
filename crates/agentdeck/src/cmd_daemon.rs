use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use agentdeck_approver::{
    ApprovalRequest, ApproverConfig, ApproverSecrets, BotApiTelegram, BrokerTimings,
    HttpPolicyJudge, JudgeVerdict, MandateContext, MandateResolver, PendingDecisions, PolicyJudge,
    TelegramTransport, UnixBrokerServer, audit_log_path,
};
use agentdeck_coordinator::checkpoint::{
    CheckpointInput, CheckpointJudge, HeuristicCheckpointJudge, HttpCheckpointJudge, record_outcome,
};
use agentdeck_coordinator::pool::parse_epoch_rfc3339;
use agentdeck_coordinator::t3_control::{
    HttpT3Controller, T3ThreadController, observe_thread_sqlite,
};
use agentdeck_coordinator::watchdog::{
    ActionDebounce, RunPhase as WdRunPhase, RunSnapshot, WatchdogAction, decide_action,
    execute_action,
};
use agentdeck_core::settings::PROVIDER_OPENROUTER;
use agentdeck_core::{Paths, Registry, plugins, t3, thread_state};
use agentdeck_usage::daemon;
use clap::Args;
use tokio::sync::watch;

use crate::{cmd_approver, cmd_coordinator, sync_watch};

const WATCHDOG_TICK_SECS: u64 = 30;
const WATCHDOG_DEBOUNCE_SECS: u64 = 300;
const APPROVER_SUPERVISE_SECS: u64 = 2;

#[derive(Debug, Args)]
pub struct DaemonArgs {
    /// Also write the old ai-usage state paths (the router and the current
    /// applet still read them). Default on; the migration release removes it.
    #[arg(long, action = clap::ArgAction::Set, default_value_t = true)]
    pub compat_write_ai_usage: bool,
    /// Run one cycle without writing anything.
    #[arg(long)]
    pub dry_run: bool,
}

pub async fn run(paths: &Paths, args: DaemonArgs) -> anyhow::Result<()> {
    let reg = Registry::load(paths)?;
    let accounts = agentdeck_usage::config::accounts_from_registry(&reg.usage)?;
    let defaults = agentdeck_usage::config::Config::default();
    let mut dpaths =
        daemon::DaemonPaths::for_agentdeck(&paths.state_dir, &daemon::agentdeck_runtime_dir());
    if !args.compat_write_ai_usage {
        dpaths.compat_canonical.clear();
        dpaths.compat_runtime.clear();
    }
    if args.dry_run {
        let state = daemon::run_once(&dpaths, &accounts, defaults.stale_seconds, true).await?;
        println!("{}", serde_json::to_string_pretty(&state)?);
        println!("dry-run: state unchanged");
        return Ok(());
    }
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    // Background sync, approver broker, and coordinator watchdog run beside
    // quota polling; failures are logged inside each task and never stop polling.
    let sync_task = tokio::spawn(sync_watch::run(paths.clone()));
    let approver_task = tokio::spawn(run_approver_task(paths.clone(), shutdown_rx.clone()));
    let watchdog_task = tokio::spawn(run_watchdog_task(paths.clone(), shutdown_rx));

    // Sources are re-read every cycle so GUI/CLI edits apply without a restart;
    // a broken registry keeps the last good list instead of stopping polling.
    let mut accounts = accounts;
    let mut poll = Registry::load(paths)
        .map(|reg| agentdeck_core::settings::usage_poll(&reg))
        .unwrap_or((defaults.refresh_seconds, defaults.stale_seconds as u64));
    let result = daemon::run_daemon(&dpaths, daemon::old_service_active, || {
        match Registry::load(paths).and_then(|reg| {
            Ok((
                agentdeck_usage::config::accounts_from_registry(&reg.usage)?,
                agentdeck_core::settings::usage_poll(&reg),
            ))
        }) {
            Ok((fresh, fresh_poll)) => (accounts, poll) = (fresh, fresh_poll),
            Err(e) => eprintln!("usage sources not reloaded, keeping previous: {e:#}"),
        }
        // shortcut: only keys new to the process are picked up; changing an
        // existing key's value still needs a daemon restart.
        if let Err(e) = daemon::load_agentdeck_secrets(&paths.config_dir) {
            eprintln!("usage.env not reloaded: {e:#}");
        }
        Ok((accounts.clone(), poll.0, poll.1 as i64))
    })
    .await;

    let _ = shutdown_tx.send(true);
    sync_task.abort();
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        let _ = approver_task.await;
        let _ = watchdog_task.await;
    })
    .await;
    result
}

// --- Approver broker task (ADR-0001) -----------------------------------------

fn mandate_from_thread(thread_id: &str, st: &thread_state::ThreadState) -> Option<MandateContext> {
    if st.phase != thread_state::RunPhase::Running {
        return None;
    }
    let m = st.mandate.as_ref().filter(|m| m.owner_granted)?;
    Some(MandateContext {
        active_autonomous_run: true,
        owner_granted: true,
        allow_arbitrary_root_scripts: false,
        thread_id: Some(thread_id.to_string()),
        project: if m.project.is_empty() {
            st.project.clone()
        } else {
            m.project.clone()
        },
        allowed_hosts: m.allowed_hosts.clone(),
        allowed_apps: m.allowed_apps.clone(),
        allow_releases: m.allow_releases,
        notes: m.notes.clone(),
    })
}

/// Resolve the active `MandateContext` for one request directly from its exact
/// thread state file. Requires an explicit `req.thread_id`, `phase == Running`,
/// and `owner_granted == true` so unrelated or ungranted callers stay interactive.
fn resolve_request_mandate(paths: &Paths, req: &ApprovalRequest) -> Option<MandateContext> {
    let tid = req
        .thread_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    thread_state::read(paths, None, tid)
        .ok()
        .flatten()
        .and_then(|found| mandate_from_thread(&found.thread, &found.data))
}

fn mandate_resolver(paths: Paths) -> MandateResolver {
    Arc::new(move |req| resolve_request_mandate(&paths, req))
}

/// Defense-in-depth wrapper over an optional Policy Judge: the broker resolves
/// and revalidates the per-request mandate itself, and this guard refuses if
/// `owner_granted` is ever false or no judge API key is configured.
struct DynamicPolicyJudge<J = HttpPolicyJudge> {
    inner: Option<J>,
}

impl<J: PolicyJudge> PolicyJudge for DynamicPolicyJudge<J> {
    async fn evaluate(
        &self,
        req: &ApprovalRequest,
        mandate: &MandateContext,
    ) -> anyhow::Result<JudgeVerdict> {
        if !mandate.active_autonomous_run || !mandate.owner_granted {
            return Ok(JudgeVerdict {
                allow: false,
                confidence: 0.0,
                reason: "mandate_not_granted".to_string(),
            });
        }
        let Some(inner) = &self.inner else {
            return Ok(JudgeVerdict {
                allow: false,
                confidence: 0.0,
                reason: "judge_unconfigured".to_string(),
            });
        };
        inner.evaluate(req, mandate).await
    }
}

#[derive(Default)]
struct TrackedPending {
    cancelled: bool,
    in_flight: HashSet<String>,
    msg_to_id: HashMap<i64, String>,
}

/// Tracks active request IDs sent to Telegram so broker disable/reload can drop
/// every pending oneshot immediately (triggering `CANCEL_BROKER_SHUTDOWN`).
struct TrackedTelegram<T> {
    inner: Arc<T>,
    pending: Arc<PendingDecisions>,
    state: Mutex<TrackedPending>,
}

impl<T> TrackedTelegram<T> {
    fn new(inner: Arc<T>, pending: Arc<PendingDecisions>) -> Self {
        Self {
            inner,
            pending,
            state: Mutex::new(TrackedPending::default()),
        }
    }

    fn cancel_pending(&self) {
        let ids: Vec<String> = {
            let mut st = self.state.lock().expect("tracked telegram mutex poisoned");
            st.cancelled = true;
            st.msg_to_id.clear();
            st.in_flight.drain().collect()
        };
        for id in ids {
            self.pending.remove(&id);
        }
    }
}

impl<T: TelegramTransport> TelegramTransport for TrackedTelegram<T> {
    async fn send_request(
        &self,
        id: &str,
        req: &ApprovalRequest,
        hard_deny: Option<&str>,
        timeout_secs: u64,
    ) -> anyhow::Result<i64> {
        {
            let mut st = self.state.lock().expect("tracked telegram mutex poisoned");
            if st.cancelled {
                drop(st);
                self.pending.remove(id);
                anyhow::bail!("approver broker shutting down");
            }
            st.in_flight.insert(id.to_string());
        }
        let res = self
            .inner
            .send_request(id, req, hard_deny, timeout_secs)
            .await;
        let mut st = self.state.lock().expect("tracked telegram mutex poisoned");
        match &res {
            Ok(msg_id) => {
                if st.cancelled {
                    st.in_flight.remove(id);
                    drop(st);
                    self.pending.remove(id);
                } else {
                    st.msg_to_id.insert(*msg_id, id.to_string());
                }
            }
            Err(_) => {
                st.in_flight.remove(id);
            }
        }
        res
    }

    async fn cancel_request(&self, message_id: i64, reason: &str) -> anyhow::Result<()> {
        {
            let mut st = self.state.lock().expect("tracked telegram mutex poisoned");
            if let Some(id) = st.msg_to_id.remove(&message_id) {
                st.in_flight.remove(&id);
            }
        }
        self.inner.cancel_request(message_id, reason).await
    }

    async fn finalize_request(&self, message_id: i64, outcome_text: &str) -> anyhow::Result<()> {
        {
            let mut st = self.state.lock().expect("tracked telegram mutex poisoned");
            if let Some(id) = st.msg_to_id.remove(&message_id) {
                st.in_flight.remove(&id);
            }
        }
        self.inner.finalize_request(message_id, outcome_text).await
    }
}

struct RunningBroker {
    shutdown_tx: watch::Sender<bool>,
    cancel_pending: Arc<dyn Fn() + Send + Sync>,
    handle: tokio::task::JoinHandle<()>,
}

impl RunningBroker {
    async fn stop(self) {
        let _ = self.shutdown_tx.send(true);
        (self.cancel_pending)();
        let _ = self.handle.await;
    }
}

fn spawn_broker<J, T, P>(
    paths: &Paths,
    cfg: ApproverConfig,
    judge: Arc<J>,
    telegram: Arc<T>,
    timings: BrokerTimings,
    start_poller: P,
) -> anyhow::Result<RunningBroker>
where
    J: PolicyJudge + 'static,
    T: TelegramTransport + 'static,
    P: FnOnce(Arc<PendingDecisions>, watch::Receiver<bool>) -> Option<tokio::task::JoinHandle<()>>,
{
    let pending = Arc::new(PendingDecisions::new());
    let tracked = Arc::new(TrackedTelegram::new(telegram, Arc::clone(&pending)));
    let socket_path = cfg.resolved_socket_path();
    let audit_path = audit_log_path(&paths.state_dir);
    let server = UnixBrokerServer::bind_with_resolver(
        socket_path.clone(),
        cfg,
        mandate_resolver(paths.clone()),
        judge,
        Arc::clone(&tracked),
        Arc::clone(&pending),
        Some(audit_path),
    )?
    .with_timings(timings);
    eprintln!(
        "agentdeck approver: listening on {}",
        socket_path.display()
    );

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let poller = start_poller(Arc::clone(&pending), shutdown_rx.clone());
    let cancel_pending: Arc<dyn Fn() + Send + Sync> = {
        let tracked = Arc::clone(&tracked);
        Arc::new(move || tracked.cancel_pending())
    };
    let handle = tokio::spawn(async move {
        if let Err(e) = server.run(shutdown_rx).await {
            eprintln!("agentdeck approver: broker stopped with error: {e:#}");
        }
        if let Some(task) = poller {
            task.abort();
        }
    });
    Ok(RunningBroker {
        shutdown_tx,
        cancel_pending,
        handle,
    })
}

#[derive(Debug, Clone, PartialEq)]
struct ApproverSpec {
    cfg: ApproverConfig,
    bot_token: String,
    owner_id: i64,
    judge_key: Option<String>,
}

enum SupervisorState<S> {
    Disabled,
    Dormant(String),
    KeepCurrent(String),
    Active(S),
}

fn load_approver_spec(paths: &Paths) -> SupervisorState<ApproverSpec> {
    let reg = match Registry::load(paths) {
        Ok(r) => r,
        Err(e) => {
            return SupervisorState::KeepCurrent(format!(
                "agentdeck approver: failed to load registry: {e:#}"
            ));
        }
    };
    if !plugins::plugin_enabled(&reg, plugins::PLUGIN_APPROVER) {
        return SupervisorState::Disabled;
    }
    let cfg = match cmd_approver::approver_config(&reg) {
        Ok(c) => c,
        Err(e) => {
            return SupervisorState::KeepCurrent(format!(
                "agentdeck approver: invalid [approver] config: {e:#}"
            ));
        }
    };
    let secrets_path = ApproverSecrets::secrets_path(&paths.config_dir);
    let secrets = match ApproverSecrets::inspect_readonly(&paths.config_dir) {
        Ok(Some(s)) => s,
        Ok(None) => {
            return SupervisorState::Dormant(format!(
                "agentdeck approver: TELEGRAM_BOT_TOKEN / TELEGRAM_OWNER_ID not configured in {}; broker dormant",
                secrets_path.display()
            ));
        }
        Err(e) => {
            return SupervisorState::Dormant(format!(
                "agentdeck approver: cannot read {} (staying dormant): {e:#}",
                secrets_path.display()
            ));
        }
    };
    let (Some(bot_token), Some(owner_id)) = (secrets.telegram_bot_token.clone(), secrets.telegram_owner_id) else {
        return SupervisorState::Dormant(format!(
            "agentdeck approver: TELEGRAM_BOT_TOKEN / TELEGRAM_OWNER_ID not configured in {}; broker dormant",
            secrets_path.display()
        ));
    };
    let provider = cfg
        .judge_provider
        .as_deref()
        .unwrap_or(PROVIDER_OPENROUTER);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let judge_key = secrets
        .judge_api_key(provider)
        .map(str::to_string)
        .or_else(|| {
            home.as_deref()
                .and_then(|h| agentdeck_core::imagegen::gateway_key(&reg, h, provider))
        });
    SupervisorState::Active(ApproverSpec {
        cfg,
        bot_token,
        owner_id,
        judge_key,
    })
}

fn spawn_prod_broker(paths: &Paths, spec: &ApproverSpec) -> anyhow::Result<RunningBroker> {
    // Enforce 0600 permissions on broker startup (read-only status/polling never mutates mode).
    let _ = ApproverSecrets::load(&paths.config_dir)?;
    let telegram = Arc::new(BotApiTelegram::new(spec.bot_token.clone(), spec.owner_id)?);
    let provider = spec
        .cfg
        .judge_provider
        .as_deref()
        .unwrap_or(PROVIDER_OPENROUTER);
    let inner_judge = match &spec.judge_key {
        Some(key) => match HttpPolicyJudge::new(
            spec.cfg.judge_provider.clone(),
            spec.cfg.judge_model.clone(),
            key.clone(),
        ) {
            Ok(j) => Some(j),
            Err(e) => {
                eprintln!(
                    "agentdeck approver: Policy Judge disabled ({e:#}); escalating to Telegram"
                );
                None
            }
        },
        None => {
            eprintln!(
                "agentdeck approver: no Policy Judge API key for {provider}; all approvals will escalate to Telegram"
            );
            None
        }
    };
    let judge = Arc::new(DynamicPolicyJudge { inner: inner_judge });
    let poll_tg = Arc::clone(&telegram);
    spawn_broker(
        paths,
        spec.cfg.clone(),
        judge,
        telegram,
        BrokerTimings::default(),
        move |pending, rx| {
            Some(tokio::spawn(async move {
                poll_tg.poll_updates_loop(pending, rx).await;
            }))
        },
    )
}

async fn supervise_approver<S, L, B>(
    paths: Paths,
    poll_interval: Duration,
    mut shutdown: watch::Receiver<bool>,
    mut load_spec: L,
    mut spawn_spec: B,
) where
    S: PartialEq,
    L: FnMut(&Paths) -> SupervisorState<S>,
    B: FnMut(&Paths, &S) -> anyhow::Result<RunningBroker>,
{
    let mut running: Option<RunningBroker> = None;
    let mut active_spec: Option<S> = None;
    let mut last_notice: Option<String> = None;
    let mut tick = tokio::time::interval(poll_interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        if *shutdown.borrow() {
            break;
        }
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = tick.tick() => {}
        }
        match load_spec(&paths) {
            SupervisorState::Disabled => {
                if let Some(broker) = running.take() {
                    eprintln!("agentdeck approver: plugin disabled; stopping broker");
                    broker.stop().await;
                }
                active_spec = None;
                last_notice = None;
            }
            SupervisorState::Dormant(msg) => {
                if let Some(broker) = running.take() {
                    broker.stop().await;
                }
                active_spec = None;
                if last_notice.as_deref() != Some(msg.as_str()) {
                    eprintln!("{msg}");
                    last_notice = Some(msg);
                }
            }
            SupervisorState::KeepCurrent(msg) => {
                if last_notice.as_deref() != Some(msg.as_str()) {
                    eprintln!("{msg}");
                    last_notice = Some(msg);
                }
            }
            SupervisorState::Active(spec) => {
                let needs_restart = active_spec.as_ref() != Some(&spec)
                    || running.as_ref().is_none_or(|b| b.handle.is_finished());
                if needs_restart {
                    if let Some(broker) = running.take() {
                        eprintln!("agentdeck approver: reloading broker with updated config");
                        broker.stop().await;
                    }
                    match spawn_spec(&paths, &spec) {
                        Ok(broker) => {
                            running = Some(broker);
                            active_spec = Some(spec);
                            last_notice = None;
                        }
                        Err(e) => {
                            active_spec = None;
                            let msg = format!("agentdeck approver: failed to start broker: {e:#}");
                            if last_notice.as_deref() != Some(msg.as_str()) {
                                eprintln!("{msg}");
                                last_notice = Some(msg);
                            }
                        }
                    }
                }
            }
        }
    }

    if let Some(broker) = running.take() {
        broker.stop().await;
    }
}

async fn run_approver_task(paths: Paths, shutdown: watch::Receiver<bool>) {
    supervise_approver(
        paths,
        Duration::from_secs(APPROVER_SUPERVISE_SECS),
        shutdown,
        load_approver_spec,
        spawn_prod_broker,
    )
    .await;
}

// --- Coordinator Watchdog task (ADR-0002) ------------------------------------

fn map_phase(p: thread_state::RunPhase) -> WdRunPhase {
    match p {
        thread_state::RunPhase::Running => WdRunPhase::Running,
        thread_state::RunPhase::SleepingForQuota => WdRunPhase::SleepingForQuota,
        thread_state::RunPhase::Intake => WdRunPhase::Paused,
        thread_state::RunPhase::Completed => WdRunPhase::Finished,
    }
}

#[allow(clippy::too_many_arguments)]
async fn watchdog_tick_with_controller<C: T3ThreadController, J: CheckpointJudge>(
    paths: &Paths,
    t3_root: &Path,
    cfg: &agentdeck_coordinator::CoordinatorConfig,
    controller: Option<&C>,
    checkpoint_judge: Option<&J>,
    debounce: &mut ActionDebounce,
    now_epoch_secs: i64,
    logged_no_endpoint: &mut bool,
) {
    let reg = Registry::load(paths).ok();
    let quotas = cmd_coordinator::collect_pool_quotas(cfg, reg.as_ref(), Some(t3_root));
    for (thread_id, state) in thread_state::list(paths) {
        if !matches!(
            state.phase,
            thread_state::RunPhase::Running | thread_state::RunPhase::SleepingForQuota
        ) {
            continue;
        }
        let observed = match observe_thread_sqlite(t3_root, &thread_id) {
            Ok(Some(o)) => o,
            Ok(None) => continue,
            Err(e) => {
                eprintln!("agentdeck watchdog: observe {thread_id} failed: {e:#}");
                continue;
            }
        };
        let snapshot = RunSnapshot {
            thread_id: thread_id.clone(),
            phase: map_phase(state.phase),
            sleep_until_epoch: state.sleep_until.as_deref().and_then(parse_epoch_rfc3339),
            has_unblocked_work: state.has_unblocked_work(),
        };

        let inside_idle_grace = now_epoch_secs.saturating_sub(observed.updated_at_epoch)
            < i64::try_from(cfg.idle_wake_grace_secs).unwrap_or(i64::MAX);
        let checkpoint_verdict = if state.phase == thread_state::RunPhase::Running
            && !observed.is_running
            && !observed.hit_quota_limit
            && observed.pending_background_tasks == 0
            && !inside_idle_grace
            && state.has_unblocked_work()
        {
            let input = CheckpointInput {
                run_count_since_checkpoint: u32::try_from(
                    observed.run_count.saturating_sub(state.checkpoint_run_count),
                )
                .unwrap_or(u32::MAX),
                item_count_since_checkpoint: u32::try_from(
                    observed
                        .item_count
                        .saturating_sub(state.checkpoint_item_count),
                )
                .unwrap_or(u32::MAX),
                milestone_just_completed: false,
                state_stale: state.state_stale(),
                active_children: observed.pending_background_tasks as usize,
                recent_events_tail: if observed.last_message_text.is_empty() {
                    Vec::new()
                } else {
                    vec![observed.last_message_text.clone()]
                },
            };
            let verdict = match checkpoint_judge {
                Some(j) => j.judge(&input, cfg).await.ok(),
                None => HeuristicCheckpointJudge.judge(&input, cfg).await.ok(),
            };
            if let Some(ref v) = verdict {
                let _ = record_outcome(&paths.state_dir, &input, v);
            }
            verdict
        } else {
            None
        };

        let action = decide_action(
            cfg,
            &snapshot,
            &observed,
            &quotas,
            now_epoch_secs,
            checkpoint_verdict.as_ref(),
        );
        match &action {
            WatchdogAction::Noop { .. } => {}
            WatchdogAction::EnterQuotaSleep {
                thread_id,
                wake_at_epoch_secs,
                reason,
            } => {
                let wake_iso = cmd_coordinator::format_utc_rfc3339(*wake_at_epoch_secs);
                if state.phase != thread_state::RunPhase::SleepingForQuota
                    || state.sleep_until.as_deref() != Some(&wake_iso)
                {
                    match thread_state::transition_phase_if(
                        paths,
                        thread_id,
                        state.phase,
                        state.sleep_until.as_deref(),
                        thread_state::RunPhase::SleepingForQuota,
                        Some(Some(wake_iso.clone())),
                    ) {
                        Ok(Some(_)) => eprintln!(
                            "agentdeck watchdog: {thread_id} entering quota sleep until {wake_iso} ({reason})"
                        ),
                        Ok(None) => {}
                        Err(e) => eprintln!(
                            "agentdeck watchdog: failed to record quota sleep for {thread_id}: {e:#}"
                        ),
                    }
                }
            }
            WatchdogAction::RotateAndWake { .. }
            | WatchdogAction::Wake { .. }
            | WatchdogAction::TriggerCheckpoint { .. } => {
                let Some(ctrl) = controller else {
                    if !*logged_no_endpoint {
                        eprintln!(
                            "agentdeck watchdog: action {:?} ready for {thread_id}, but [coordinator] t3_endpoint is not configured",
                            action.kind()
                        );
                        *logged_no_endpoint = true;
                    }
                    continue;
                };
                // Recheck current state before dispatching to T3 so a concurrent
                // Completed/Intake transition is never woken needlessly.
                let still_eligible = thread_state::read(paths, Some(t3_root), &thread_id)
                    .ok()
                    .flatten()
                    .is_some_and(|f| {
                        f.inherited_from.is_none()
                            && f.data.phase == state.phase
                            && f.data.sleep_until == state.sleep_until
                            && f.data.has_unblocked_work()
                    });
                if !still_eligible {
                    continue;
                }
                match execute_action(ctrl, &action, debounce, now_epoch_secs).await {
                    Ok(true) => {
                        eprintln!(
                            "agentdeck watchdog: executed {} on {thread_id}",
                            action.kind()
                        );
                        if state.phase == thread_state::RunPhase::SleepingForQuota {
                            match thread_state::transition_phase_if(
                                paths,
                                &thread_id,
                                thread_state::RunPhase::SleepingForQuota,
                                state.sleep_until.as_deref(),
                                thread_state::RunPhase::Running,
                                Some(None),
                            ) {
                                Ok(Some(_)) | Ok(None) => {}
                                Err(e) => eprintln!(
                                    "agentdeck watchdog: failed to clear sleep_until for {thread_id}: {e:#}"
                                ),
                            }
                        }
                    }
                    Ok(false) => {}
                    Err(e) => eprintln!(
                        "agentdeck watchdog: {} on {thread_id} failed: {e:#}",
                        action.kind()
                    ),
                }
            }
        }
    }
}

#[derive(PartialEq, Eq)]
struct ControllerKey {
    endpoint: String,
    token_file: Option<PathBuf>,
    token_mtime: Option<SystemTime>,
}

struct CachedController {
    key: ControllerKey,
    controller: HttpT3Controller,
}

#[derive(PartialEq, Eq)]
struct JudgeKey {
    provider: Option<String>,
    model: Option<String>,
}

struct CachedCheckpointJudge {
    key: JudgeKey,
    judge: HttpCheckpointJudge,
}

async fn run_watchdog_task(paths: Paths, mut shutdown: watch::Receiver<bool>) {
    let mut debounce = ActionDebounce::new(WATCHDOG_DEBOUNCE_SECS);
    let mut logged_no_endpoint = false;
    let mut cached_ctrl: Option<CachedController> = None;
    let mut cached_judge: Option<CachedCheckpointJudge> = None;
    let mut tick = tokio::time::interval(Duration::from_secs(WATCHDOG_TICK_SECS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = tick.tick() => {}
        }
        let reg = match Registry::load(&paths) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("agentdeck watchdog: registry load failed: {e:#}");
                continue;
            }
        };
        if !plugins::plugin_enabled(&reg, plugins::PLUGIN_COORDINATOR) {
            continue;
        }
        let cfg = match cmd_coordinator::coordinator_config(&reg) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("agentdeck watchdog: invalid [coordinator] config: {e:#}");
                continue;
            }
        };
        if cfg.pool.is_empty() && cfg.t3_endpoint.is_none() {
            continue;
        }
        let Ok(t3_root) = t3::t3_root() else {
            continue;
        };
        match cfg.t3_endpoint.as_deref() {
            Some(ep) => {
                let token_mtime = cfg
                    .t3_token_file
                    .as_deref()
                    .and_then(|p| std::fs::metadata(p).ok()?.modified().ok());
                let key = ControllerKey {
                    endpoint: ep.to_string(),
                    token_file: cfg.t3_token_file.clone(),
                    token_mtime,
                };
                if cached_ctrl.as_ref().is_none_or(|c| c.key != key) {
                    match HttpT3Controller::new(ep, cfg.t3_token_file.as_deref()) {
                        Ok(controller) => cached_ctrl = Some(CachedController { key, controller }),
                        Err(e) => {
                            eprintln!("agentdeck watchdog: invalid t3_endpoint: {e:#}");
                            cached_ctrl = None;
                        }
                    }
                }
            }
            None => cached_ctrl = None,
        }
        let judge_key = JudgeKey {
            provider: cfg.judge_provider.clone(),
            model: cfg.judge_model.clone(),
        };
        if cached_judge.as_ref().is_none_or(|j| j.key != judge_key) {
            cached_judge = HttpCheckpointJudge::from_env(&cfg)
                .ok()
                .map(|judge| CachedCheckpointJudge {
                    key: judge_key,
                    judge,
                });
        }
        watchdog_tick_with_controller(
            &paths,
            &t3_root,
            &cfg,
            cached_ctrl.as_ref().map(|c| &c.controller),
            cached_judge.as_ref().map(|j| &j.judge),
            &mut debounce,
            cmd_coordinator::now_epoch_secs(),
            &mut logged_no_endpoint,
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};

    use agentdeck_approver::{AuditSource, read_recent, telegram::CallbackAction};
    use agentdeck_coordinator::checkpoint::CheckpointVerdict;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;
    use tokio::sync::oneshot;

    #[derive(Default)]
    struct MockController {
        configured: Mutex<Vec<(String, String, String)>>,
        sent: Mutex<Vec<(String, String, bool)>>,
    }

    impl T3ThreadController for MockController {
        async fn configure_thread(
            &self,
            thread_id: &str,
            instance_id: &str,
            model: &str,
            _options: &HashMap<String, serde_json::Value>,
        ) -> anyhow::Result<()> {
            self.configured.lock().unwrap().push((
                thread_id.to_string(),
                instance_id.to_string(),
                model.to_string(),
            ));
            Ok(())
        }

        async fn send_message(
            &self,
            thread_id: &str,
            message: &str,
            fresh_context: bool,
        ) -> anyhow::Result<()> {
            self.sent.lock().unwrap().push((
                thread_id.to_string(),
                message.to_string(),
                fresh_context,
            ));
            Ok(())
        }
    }

    #[derive(Debug, Clone)]
    struct SentCard {
        thread_id: Option<String>,
        hard_deny: Option<String>,
        timeout_secs: u64,
    }

    struct MockTelegram {
        next_msg_id: AtomicI64,
        pending: Mutex<Option<Arc<PendingDecisions>>>,
        auto_action: Mutex<Option<CallbackAction>>,
        sent: Mutex<Vec<SentCard>>,
        cancelled: Mutex<Vec<(i64, String)>>,
        sent_count: watch::Sender<usize>,
        cancel_count: watch::Sender<usize>,
    }

    impl MockTelegram {
        fn new(auto_action: Option<CallbackAction>) -> Self {
            Self {
                next_msg_id: AtomicI64::new(100),
                pending: Mutex::new(None),
                auto_action: Mutex::new(auto_action),
                sent: Mutex::new(Vec::new()),
                cancelled: Mutex::new(Vec::new()),
                sent_count: watch::channel(0).0,
                cancel_count: watch::channel(0).0,
            }
        }

        fn attach_pending(&self, pending: Arc<PendingDecisions>) {
            *self.pending.lock().unwrap() = Some(pending);
        }

        fn set_auto_action(&self, action: Option<CallbackAction>) {
            *self.auto_action.lock().unwrap() = action;
        }
    }

    impl TelegramTransport for MockTelegram {
        async fn send_request(
            &self,
            id: &str,
            req: &ApprovalRequest,
            hard_deny: Option<&str>,
            timeout_secs: u64,
        ) -> anyhow::Result<i64> {
            let msg_id = self.next_msg_id.fetch_add(1, Ordering::SeqCst);
            let n = {
                let mut sent = self.sent.lock().unwrap();
                sent.push(SentCard {
                    thread_id: req.thread_id.clone(),
                    hard_deny: hard_deny.map(str::to_string),
                    timeout_secs,
                });
                sent.len()
            };
            let _ = self.sent_count.send(n);
            if let Some(action) = *self.auto_action.lock().unwrap()
                && let Some(pending) = self.pending.lock().unwrap().clone()
            {
                pending.resolve_callback(id, action);
            }
            Ok(msg_id)
        }

        async fn cancel_request(&self, message_id: i64, reason: &str) -> anyhow::Result<()> {
            let n = {
                let mut cancelled = self.cancelled.lock().unwrap();
                cancelled.push((message_id, reason.to_string()));
                cancelled.len()
            };
            let _ = self.cancel_count.send(n);
            Ok(())
        }

        async fn finalize_request(&self, _message_id: i64, _outcome_text: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    struct MockAllowJudge {
        calls: Arc<AtomicUsize>,
    }

    impl PolicyJudge for MockAllowJudge {
        async fn evaluate(
            &self,
            _req: &ApprovalRequest,
            _mandate: &MandateContext,
        ) -> anyhow::Result<JudgeVerdict> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(JudgeVerdict {
                allow: true,
                confidence: 0.99,
                reason: "mock_allow".to_string(),
            })
        }
    }

    struct BarrierJudge {
        entered: Mutex<Option<oneshot::Sender<()>>>,
        release: tokio::sync::Mutex<Option<oneshot::Receiver<()>>>,
    }

    impl PolicyJudge for BarrierJudge {
        async fn evaluate(
            &self,
            _req: &ApprovalRequest,
            _mandate: &MandateContext,
        ) -> anyhow::Result<JudgeVerdict> {
            if let Some(tx) = self.entered.lock().unwrap().take() {
                let _ = tx.send(());
            }
            let rx = self.release.lock().await.take();
            if let Some(rx) = rx {
                let _ = rx.await;
            }
            Ok(JudgeVerdict {
                allow: true,
                confidence: 0.99,
                reason: "mock_allow".to_string(),
            })
        }
    }

    async fn send_broker_request(socket: &Path, req: serde_json::Value) -> Option<serde_json::Value> {
        let stream = UnixStream::connect(socket).await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        let mut line = req.to_string().into_bytes();
        line.push(b'\n');
        write_half.write_all(&line).await.unwrap();
        write_half.flush().await.unwrap();
        let mut lines = BufReader::new(read_half).lines();
        while let Some(raw) = lines.next_line().await.unwrap() {
            let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
            if v.get("type").and_then(serde_json::Value::as_str) == Some("result") {
                drop(write_half);
                return Some(v);
            }
        }
        None
    }

    fn seed_wd_db(t3_root: &Path, thread_id: &str, runs: &[(i64, &str, &str)], items: &[(&str, &str, &str, &str)], subagent_status: Option<&str>) {
        std::fs::create_dir_all(t3_root.join("userdata")).unwrap();
        std::fs::create_dir_all(t3_root.join("caches")).unwrap();
        let db = rusqlite::Connection::open(t3_root.join("userdata/statev2.sqlite")).unwrap();
        db.execute_batch(
            "CREATE TABLE orchestration_v2_projection_threads (
               thread_id TEXT PRIMARY KEY, title TEXT, updated_at TEXT, payload_json TEXT, provider_instance_id TEXT
             );
             CREATE TABLE orchestration_v2_projection_runs (
               run_id TEXT PRIMARY KEY, thread_id TEXT, ordinal INTEGER, status TEXT,
               requested_at TEXT, completed_at TEXT, payload_json TEXT, provider_instance_id TEXT
             );
             CREATE TABLE orchestration_v2_projection_turn_items (
               turn_item_id TEXT PRIMARY KEY, thread_id TEXT, run_id TEXT, ordinal INTEGER,
               type TEXT, status TEXT, payload_json TEXT
             );
             CREATE TABLE orchestration_v2_projection_thread_item_counts (
               thread_id TEXT PRIMARY KEY, item_count INTEGER
             );
             CREATE TABLE orchestration_v2_projection_subagents (
               subagent_id TEXT PRIMARY KEY, run_id TEXT, status TEXT
             );",
        )
        .unwrap();
        db.execute(
            "INSERT INTO orchestration_v2_projection_threads VALUES (?1,'Coord','2026-11-21T01:00:00Z','{}','claude_a')",
            [thread_id],
        )
        .unwrap();
        db.execute(
            "INSERT INTO orchestration_v2_projection_thread_item_counts VALUES (?1, ?2)",
            rusqlite::params![thread_id, items.len() as i64],
        )
        .unwrap();
        for (ord, status, completed_at) in runs {
            db.execute(
                "INSERT INTO orchestration_v2_projection_runs VALUES (?1,?2,?3,?4,'2026-11-21T01:00:00Z',?5,'{}','claude_a')",
                rusqlite::params![format!("r{ord}"), thread_id, ord, status, completed_at],
            )
            .unwrap();
        }
        for (idx, (run_id, kind, status, payload)) in items.iter().enumerate() {
            db.execute(
                "INSERT INTO orchestration_v2_projection_turn_items VALUES (?1,?2,?3,?4,?5,?6,?7)",
                rusqlite::params![format!("i{idx}"), thread_id, run_id, idx as i64 + 1, kind, status, payload],
            )
            .unwrap();
        }
        if let Some(st) = subagent_status
            && let Some((ord, _, _)) = runs.last()
        {
            db.execute(
                "INSERT INTO orchestration_v2_projection_subagents VALUES ('sub-1', ?1, ?2)",
                rusqlite::params![format!("r{ord}"), st],
            )
            .unwrap();
        }
    }

    #[tokio::test]
    async fn watchdog_tick_enters_quota_sleep_then_wakes_and_clears_sleep_until() {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-daemon-wd-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = Paths::new(dir.join("config"), dir.join("state"));
        let t3_root = dir.join("t3");
        seed_wd_db(
            &t3_root,
            "t-wd",
            &[(1, "failed", "2026-11-21T01:05:00Z")],
            &[(
                "r1",
                "error",
                "failed",
                r#"{"failure":{"class":"usage_limit","message":"5-hour limit reached"}}"#,
            )],
            None,
        );
        std::fs::write(
            t3_root.join("userdata/settings.json"),
            r#"{"providerInstances":{"claude_a":{"enabled":true}}}"#,
        )
        .unwrap();

        // Seed thread_state as Running with open checklist item.
        thread_state::write(&paths, Some(&t3_root), "t-wd", Some("Goal: ship"), None).unwrap();
        thread_state::update_checklist(
            &paths,
            Some(&t3_root),
            "t-wd",
            Some("proj"),
            &[thread_state::ItemPatch {
                id: "u1".into(),
                title: Some("Unit 1".into()),
                status: Some("todo".into()),
                links: None,
                note: None,
            }],
            &[],
        )
        .unwrap();
        thread_state::set_run_meta(
            &paths,
            Some(&t3_root),
            "t-wd",
            Some(thread_state::RunPhase::Running),
            None,
            None,
            false,
        )
        .unwrap();

        let now = parse_epoch_rfc3339("2026-11-21T02:00:00Z").unwrap();
        // Pool exhausted: resets at 03:00:00Z -> sleep until 03:02:00Z.
        std::fs::write(
            t3_root.join("caches/claude_a.json"),
            r#"{"status":"ready","usageLimits":{"windows":[{"id":"5h","windowDurationMins":300,"usedPercent":100.0,"resetsAt":"2026-11-21T03:00:00Z"}]}}"#,
        )
        .unwrap();
        let cfg = agentdeck_coordinator::CoordinatorConfig {
            pool: vec![agentdeck_coordinator::CoordinatorPoolEntry::new(
                "claude_a",
                "claude-opus-4-6",
            )],
            ..Default::default()
        };
        let ctrl = MockController::default();
        let mut debounce = ActionDebounce::new(300);
        let mut logged = false;

        watchdog_tick_with_controller(
            &paths,
            &t3_root,
            &cfg,
            Some(&ctrl),
            Option::<&HeuristicCheckpointJudge>::None,
            &mut debounce,
            now,
            &mut logged,
        )
        .await;
        let st = thread_state::read(&paths, Some(&t3_root), "t-wd")
            .unwrap()
            .unwrap()
            .data;
        assert_eq!(st.phase, thread_state::RunPhase::SleepingForQuota);
        assert_eq!(st.sleep_until.as_deref(), Some("2026-11-21T03:02:00Z"));
        assert!(ctrl.sent.lock().unwrap().is_empty());

        // After reset + buffer, quota recovers: watchdog wakes thread and clears sleep_until.
        std::fs::write(
            t3_root.join("caches/claude_a.json"),
            r#"{"status":"ready","usageLimits":{"windows":[{"id":"5h","windowDurationMins":300,"usedPercent":0.0}]}}"#,
        )
        .unwrap();
        let after_wake = parse_epoch_rfc3339("2026-11-21T03:05:00Z").unwrap();
        watchdog_tick_with_controller(
            &paths,
            &t3_root,
            &cfg,
            Some(&ctrl),
            Option::<&HeuristicCheckpointJudge>::None,
            &mut debounce,
            after_wake,
            &mut logged,
        )
        .await;
        let st2 = thread_state::read(&paths, Some(&t3_root), "t-wd")
            .unwrap()
            .unwrap()
            .data;
        assert_eq!(st2.phase, thread_state::RunPhase::Running);
        assert_eq!(st2.sleep_until, None);
        assert_eq!(ctrl.configured.lock().unwrap().len(), 1);
        assert_eq!(ctrl.sent.lock().unwrap().len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn watchdog_cas_does_not_overwrite_completed_during_inflight_wake() {
        struct CompletingController {
            paths: Paths,
            t3_root: PathBuf,
            completed_once: AtomicBool,
        }

        impl T3ThreadController for CompletingController {
            async fn configure_thread(
                &self,
                _thread_id: &str,
                _instance_id: &str,
                _model: &str,
                _options: &HashMap<String, serde_json::Value>,
            ) -> anyhow::Result<()> {
                Ok(())
            }

            async fn send_message(
                &self,
                thread_id: &str,
                _message: &str,
                _fresh_context: bool,
            ) -> anyhow::Result<()> {
                if !self.completed_once.swap(true, Ordering::SeqCst) {
                    thread_state::set_run_meta(
                        &self.paths,
                        Some(&self.t3_root),
                        thread_id,
                        Some(thread_state::RunPhase::Completed),
                        None,
                        Some(None),
                        false,
                    )?;
                }
                Ok(())
            }
        }

        let dir = std::env::temp_dir().join(format!(
            "agentdeck-daemon-wd-cas-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = Paths::new(dir.join("config"), dir.join("state"));
        let t3_root = dir.join("t3");
        seed_wd_db(
            &t3_root,
            "t-cas",
            &[(1, "failed", "2026-11-21T01:05:00Z")],
            &[],
            None,
        );
        std::fs::write(
            t3_root.join("userdata/settings.json"),
            r#"{"providerInstances":{"claude_a":{"enabled":true}}}"#,
        )
        .unwrap();
        std::fs::write(
            t3_root.join("caches/claude_a.json"),
            r#"{"status":"ready","usageLimits":{"windows":[{"id":"5h","windowDurationMins":300,"usedPercent":0.0}]}}"#,
        )
        .unwrap();
        thread_state::write(&paths, Some(&t3_root), "t-cas", Some("Goal: finish"), None).unwrap();
        thread_state::update_checklist(
            &paths,
            Some(&t3_root),
            "t-cas",
            Some("proj"),
            &[thread_state::ItemPatch {
                id: "u1".into(),
                title: Some("Unit 1".into()),
                status: Some("todo".into()),
                links: None,
                note: None,
            }],
            &[],
        )
        .unwrap();
        thread_state::set_run_meta(
            &paths,
            Some(&t3_root),
            "t-cas",
            Some(thread_state::RunPhase::SleepingForQuota),
            None,
            Some(Some("2026-11-21T03:02:00Z".into())),
            false,
        )
        .unwrap();

        let cfg = agentdeck_coordinator::CoordinatorConfig {
            pool: vec![agentdeck_coordinator::CoordinatorPoolEntry::new(
                "claude_a",
                "claude-opus-4-6",
            )],
            ..Default::default()
        };
        let ctrl = CompletingController {
            paths: paths.clone(),
            t3_root: t3_root.clone(),
            completed_once: AtomicBool::new(false),
        };
        let mut debounce = ActionDebounce::new(300);
        let mut logged = false;
        let now = parse_epoch_rfc3339("2026-11-21T03:05:00Z").unwrap();

        watchdog_tick_with_controller(
            &paths,
            &t3_root,
            &cfg,
            Some(&ctrl),
            Option::<&HeuristicCheckpointJudge>::None,
            &mut debounce,
            now,
            &mut logged,
        )
        .await;

        assert!(ctrl.completed_once.load(Ordering::SeqCst));
        let st = thread_state::read(&paths, Some(&t3_root), "t-cas")
            .unwrap()
            .unwrap()
            .data;
        assert_eq!(
            st.phase,
            thread_state::RunPhase::Completed,
            "in-flight wake must never overwrite Completed back to Running"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn watchdog_checkpoint_skips_active_children_and_grace_uses_deltas_and_records_outcome() {
        struct RecordingCheckpointJudge {
            inputs: Mutex<Vec<CheckpointInput>>,
        }

        impl CheckpointJudge for RecordingCheckpointJudge {
            async fn judge(
                &self,
                input: &CheckpointInput,
                cfg: &agentdeck_coordinator::CoordinatorConfig,
            ) -> anyhow::Result<CheckpointVerdict> {
                self.inputs.lock().unwrap().push(input.clone());
                HeuristicCheckpointJudge.judge(input, cfg).await
            }
        }

        let dir = std::env::temp_dir().join(format!(
            "agentdeck-daemon-wd-chk-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = Paths::new(dir.join("config"), dir.join("state"));
        let t3_root = dir.join("t3");

        // 8 cumulative runs and 12 cumulative items; last checkpoint recorded at 6 runs / 9 items.
        // Lifetime totals (8/12) exceed soft limits (5/10), while deltas (2/3) stay below them.
        let runs: Vec<(i64, &str, &str)> = (1..=8)
            .map(|n| (n, "completed", "2026-11-21T01:05:00Z"))
            .collect();
        let items: Vec<(&str, &str, &str, &str)> = (0..12)
            .map(|_| ("r8", "assistant_message", "completed", r#"{"text":"ok"}"#))
            .collect();
        seed_wd_db(&t3_root, "t-chk", &runs, &items, Some("running"));
        std::fs::write(
            t3_root.join("userdata/settings.json"),
            r#"{"providerInstances":{"claude_a":{"enabled":true}}}"#,
        )
        .unwrap();
        std::fs::write(
            t3_root.join("caches/claude_a.json"),
            r#"{"status":"ready","usageLimits":{"windows":[{"id":"5h","windowDurationMins":300,"usedPercent":10.0}]}}"#,
        )
        .unwrap();

        thread_state::update_checklist(
            &paths,
            Some(&t3_root),
            "t-chk",
            Some("proj"),
            &[thread_state::ItemPatch {
                id: "u1".into(),
                title: Some("Unit 1".into()),
                status: Some("todo".into()),
                links: None,
                note: None,
            }],
            &[],
        )
        .unwrap();
        thread_state::apply_state_update(
            &paths,
            Some(&t3_root),
            "t-chk",
            thread_state::StateUpdate {
                state: Some("Goal: ship\nnext: u1"),
                phase: Some(thread_state::RunPhase::Running),
                record_checkpoint: true,
                checkpoint_run_count: Some(6),
                checkpoint_item_count: Some(9),
                ..Default::default()
            },
        )
        .unwrap();

        let cfg = agentdeck_coordinator::CoordinatorConfig {
            pool: vec![agentdeck_coordinator::CoordinatorPoolEntry::new(
                "claude_a",
                "claude-opus-4-6",
            )],
            idle_wake_grace_secs: 180,
            checkpoint_soft_runs: 5,
            checkpoint_soft_items: 10,
            ..Default::default()
        };
        let judge = RecordingCheckpointJudge {
            inputs: Mutex::new(Vec::new()),
        };
        let ctrl = MockController::default();
        let mut debounce = ActionDebounce::new(300);
        let mut logged = false;

        // 1. Active subagent present: checkpoint judge must be skipped even past grace.
        let past_grace = parse_epoch_rfc3339("2026-11-21T01:10:00Z").unwrap();
        watchdog_tick_with_controller(
            &paths,
            &t3_root,
            &cfg,
            Some(&ctrl),
            Some(&judge),
            &mut debounce,
            past_grace,
            &mut logged,
        )
        .await;
        assert!(judge.inputs.lock().unwrap().is_empty());

        // 2. Subagent finished, but inside idle_wake_grace_secs: still skipped.
        let db = rusqlite::Connection::open(t3_root.join("userdata/statev2.sqlite")).unwrap();
        db.execute(
            "UPDATE orchestration_v2_projection_subagents SET status='completed'",
            [],
        )
        .unwrap();
        drop(db);
        let inside_grace = parse_epoch_rfc3339("2026-11-21T01:06:00Z").unwrap();
        watchdog_tick_with_controller(
            &paths,
            &t3_root,
            &cfg,
            Some(&ctrl),
            Some(&judge),
            &mut debounce,
            inside_grace,
            &mut logged,
        )
        .await;
        assert!(judge.inputs.lock().unwrap().is_empty());

        // 3. Past grace with 0 active children: judge runs ONCE with deltas (2/3) and logs outcome.
        watchdog_tick_with_controller(
            &paths,
            &t3_root,
            &cfg,
            Some(&ctrl),
            Some(&judge),
            &mut debounce,
            past_grace,
            &mut logged,
        )
        .await;
        let seen = judge.inputs.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].run_count_since_checkpoint, 2);
        assert_eq!(seen[0].item_count_since_checkpoint, 3);
        let outcomes = std::fs::read_to_string(
            paths.state_dir.join("coordinator/checkpoint-outcomes.jsonl"),
        )
        .unwrap();
        assert!(outcomes.contains("\"runs\":2"), "{outcomes}");
        assert!(outcomes.contains("\"items\":3"), "{outcomes}");
        assert!(outcomes.contains("\"should_checkpoint\":false"), "{outcomes}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_request_mandate_matches_thread_id_and_rejects_non_running() {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-daemon-mandate-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = Paths::new(dir.join("config"), dir.join("state"));
        let granted_mandate = thread_state::RunMandate {
            project: "blockfield".into(),
            allowed_hosts: vec!["homeserver".into()],
            allowed_apps: vec!["staging".into()],
            ..Default::default()
        };
        thread_state::set_run_meta(
            &paths,
            None,
            "t-run",
            Some(thread_state::RunPhase::Running),
            Some(granted_mandate.clone()),
            None,
            false,
        )
        .unwrap();
        thread_state::grant_mandate_if_matches(&paths, "t-run", &granted_mandate).unwrap();
        thread_state::set_run_meta(
            &paths,
            None,
            "t-ungranted",
            Some(thread_state::RunPhase::Running),
            Some(granted_mandate),
            None,
            false,
        )
        .unwrap();
        thread_state::set_run_meta(
            &paths,
            None,
            "t-intake",
            Some(thread_state::RunPhase::Intake),
            Some(thread_state::RunMandate {
                project: "other".into(),
                ..Default::default()
            }),
            None,
            false,
        )
        .unwrap();

        let mk = |tid: Option<&str>| ApprovalRequest {
            kind: "root".into(),
            reason: "check".into(),
            script: Some("docker ps".into()),
            text: None,
            title: None,
            target: None,
            line: None,
            host: Some("homeserver".into()),
            app: None,
            thread_id: tid.map(str::to_string),
            timeout_secs: None,
            nonce: None,
            trusted_peer_host: None,
        };
        assert_eq!(
            resolve_request_mandate(&paths, &mk(Some("t-run")))
                .unwrap()
                .project,
            "blockfield"
        );
        // Running thread without owner grant must NOT resolve as an active mandate.
        assert!(resolve_request_mandate(&paths, &mk(Some("t-ungranted"))).is_none());
        // Explicitly naming a non-running thread must NOT inherit another thread's mandate.
        assert!(resolve_request_mandate(&paths, &mk(Some("t-intake"))).is_none());
        // Omitting thread_id must NOT borrow an active thread's mandate (fail closed).
        assert!(resolve_request_mandate(&paths, &mk(None)).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn broker_isolates_concurrent_threads_and_interactive_requests() {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-daemon-iso-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = Paths::new(dir.join("config"), dir.join("state"));

        // Thread A: granted on local/staging.
        let mandate_a = thread_state::RunMandate {
            project: "proj-a".into(),
            allowed_hosts: vec!["local".into()],
            allowed_apps: vec!["staging".into()],
            ..Default::default()
        };
        thread_state::set_run_meta(
            &paths,
            None,
            "thread-a",
            Some(thread_state::RunPhase::Running),
            Some(mandate_a.clone()),
            None,
            false,
        )
        .unwrap();
        thread_state::grant_mandate_if_matches(&paths, "thread-a", &mandate_a).unwrap();

        // Thread B: Running with wide open mandate, NOT owner_granted.
        thread_state::set_run_meta(
            &paths,
            None,
            "thread-b",
            Some(thread_state::RunPhase::Running),
            Some(thread_state::RunMandate {
                project: "proj-b".into(),
                notes: "approve all".into(),
                ..Default::default()
            }),
            None,
            false,
        )
        .unwrap();

        let sock = PathBuf::from(format!("/tmp/ad-iso-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let cfg = ApproverConfig {
            enabled: true,
            socket_path: Some(sock.clone()),
            autonomous_timeout_secs: 45,
            interactive_timeout_secs: 300,
            ..Default::default()
        };
        let judge_calls = Arc::new(AtomicUsize::new(0));
        let judge = Arc::new(DynamicPolicyJudge {
            inner: Some(MockAllowJudge {
                calls: Arc::clone(&judge_calls),
            }),
        });
        let tg = Arc::new(MockTelegram::new(Some(CallbackAction::Deny)));
        let tg_ref = Arc::clone(&tg);
        let broker = spawn_broker(
            &paths,
            cfg,
            judge,
            Arc::clone(&tg),
            BrokerTimings::default(),
            move |pending, _| {
                tg_ref.attach_pending(pending);
                None
            },
        )
        .unwrap();

        // 1. Thread A in-scope -> auto-approved by Policy Judge, zero Telegram sends.
        let r1 = send_broker_request(
            &sock,
            serde_json::json!({
                "kind": "confirm", "title": "Deploy", "text": "staging deploy",
                "host": "local", "app": "staging", "thread_id": "thread-a"
            }),
        )
        .await
        .unwrap();
        assert_eq!(r1["result"], "approved");
        assert_eq!(judge_calls.load(Ordering::SeqCst), 1);
        assert!(tg.sent.lock().unwrap().is_empty());

        // 2. Thread B (ungranted) -> NEVER reaches Policy Judge, gets interactive timeout (300s).
        let r2 = send_broker_request(
            &sock,
            serde_json::json!({
                "kind": "confirm", "title": "Sneak", "text": "deploy",
                "host": "local", "app": "staging", "thread_id": "thread-b"
            }),
        )
        .await
        .unwrap();
        assert_eq!(r2["result"], "denied");
        assert_eq!(judge_calls.load(Ordering::SeqCst), 1);

        // 3. Interactive (thread_id == None) -> interactive timeout (300s), no cross-thread hard-deny.
        let r3 = send_broker_request(
            &sock,
            serde_json::json!({
                "kind": "confirm", "title": "Interactive", "text": "other app",
                "host": "local", "app": "unrelated-app"
            }),
        )
        .await
        .unwrap();
        assert_eq!(r3["result"], "denied");
        assert_eq!(judge_calls.load(Ordering::SeqCst), 1);

        // 4. Thread A out-of-scope app -> [HARD-DENY] out_of_scope_app, autonomous timeout (45s).
        let r4 = send_broker_request(
            &sock,
            serde_json::json!({
                "kind": "confirm", "title": "Prod", "text": "prod deploy",
                "host": "local", "app": "prod", "thread_id": "thread-a"
            }),
        )
        .await
        .unwrap();
        assert_eq!(r4["result"], "denied");
        assert_eq!(judge_calls.load(Ordering::SeqCst), 1);

        let cards = tg.sent.lock().unwrap().clone();
        assert_eq!(cards.len(), 3);
        assert_eq!(cards[0].thread_id.as_deref(), Some("thread-b"));
        assert_eq!(cards[0].hard_deny, None);
        assert_eq!(cards[0].timeout_secs, 300);
        assert_eq!(cards[1].thread_id, None);
        assert_eq!(cards[1].hard_deny, None);
        assert_eq!(cards[1].timeout_secs, 300);
        assert_eq!(cards[2].thread_id.as_deref(), Some("thread-a"));
        assert_eq!(cards[2].hard_deny.as_deref(), Some("out_of_scope_app"));
        assert_eq!(cards[2].timeout_secs, 45);

        let records = read_recent(&audit_log_path(&paths.state_dir), 10).unwrap();
        assert_eq!(records.len(), 4);
        assert_eq!(records[0].source, AuditSource::PolicyJudge);
        assert_eq!(records[3].perimeter, "hard_deny:out_of_scope_app");

        broker.stop().await;
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn broker_revoking_mandate_mid_flight_escalates_to_telegram() {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-daemon-revoke-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = Paths::new(dir.join("config"), dir.join("state"));

        let mandate = thread_state::RunMandate {
            project: "proj-a".into(),
            allowed_hosts: vec!["local".into()],
            allowed_apps: vec!["staging".into()],
            ..Default::default()
        };
        thread_state::set_run_meta(
            &paths,
            None,
            "thread-rev",
            Some(thread_state::RunPhase::Running),
            Some(mandate.clone()),
            None,
            false,
        )
        .unwrap();
        thread_state::grant_mandate_if_matches(&paths, "thread-rev", &mandate).unwrap();

        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let judge = Arc::new(DynamicPolicyJudge {
            inner: Some(BarrierJudge {
                entered: Mutex::new(Some(entered_tx)),
                release: tokio::sync::Mutex::new(Some(release_rx)),
            }),
        });
        let sock = PathBuf::from(format!("/tmp/ad-rev-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let cfg = ApproverConfig {
            enabled: true,
            socket_path: Some(sock.clone()),
            ..Default::default()
        };
        let tg = Arc::new(MockTelegram::new(Some(CallbackAction::Deny)));
        let tg_ref = Arc::clone(&tg);
        let broker = spawn_broker(
            &paths,
            cfg,
            judge,
            Arc::clone(&tg),
            BrokerTimings::default(),
            move |pending, _| {
                tg_ref.attach_pending(pending);
                None
            },
        )
        .unwrap();

        let sock_clone = sock.clone();
        let req_task = tokio::spawn(async move {
            send_broker_request(
                &sock_clone,
                serde_json::json!({
                    "kind": "confirm", "title": "Deploy", "text": "staging deploy",
                    "host": "local", "app": "staging", "thread_id": "thread-rev"
                }),
            )
            .await
        });

        // Wait until Policy Judge is actively awaiting, then revoke the mandate mid-flight.
        tokio::time::timeout(Duration::from_secs(2), entered_rx)
            .await
            .unwrap()
            .unwrap();
        thread_state::revoke_mandate(&paths, "thread-rev").unwrap();
        let _ = release_tx.send(());

        let result = tokio::time::timeout(Duration::from_secs(2), req_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(result["result"], "denied");
        assert_eq!(tg.sent.lock().unwrap().len(), 1);
        let records = read_recent(&audit_log_path(&paths.state_dir), 5).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].source, AuditSource::Denied);
        assert_eq!(
            records[0].judge_reason.as_deref(),
            Some("judge_mandate_changed")
        );

        broker.stop().await;
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn approver_supervisor_reloads_stricter_deny_rule_and_cancels_on_disable() {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-daemon-sup-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = Paths::new(dir.join("config"), dir.join("state"));
        let sock = PathBuf::from(format!("/tmp/ad-sup-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);

        plugins::set_plugin_state(&paths, plugins::PLUGIN_APPROVER, true, false).unwrap();
        agentdeck_core::settings::update_approver(&paths, |cfg| {
            cfg.socket_path = Some(sock.clone());
            Ok(())
        })
        .unwrap();

        let tg = Arc::new(MockTelegram::new(Some(CallbackAction::Approve)));
        let generation = Arc::new(AtomicUsize::new(0));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let sup_task = {
            let paths = paths.clone();
            let tg = Arc::clone(&tg);
            let generation = Arc::clone(&generation);
            tokio::spawn(async move {
                supervise_approver(
                    paths,
                    Duration::from_millis(20),
                    shutdown_rx,
                    |p| match Registry::load(p) {
                        Ok(r) if !plugins::plugin_enabled(&r, plugins::PLUGIN_APPROVER) => {
                            SupervisorState::Disabled
                        }
                        Ok(r) => match cmd_approver::approver_config(&r) {
                            Ok(c) => SupervisorState::Active(c),
                            Err(e) => SupervisorState::KeepCurrent(format!("{e:#}")),
                        },
                        Err(e) => SupervisorState::KeepCurrent(format!("{e:#}")),
                    },
                    move |p, cfg| {
                        let tg_ref = Arc::clone(&tg);
                        let broker = spawn_broker(
                            p,
                            cfg.clone(),
                            Arc::new(DynamicPolicyJudge::<MockAllowJudge> { inner: None }),
                            Arc::clone(&tg),
                            BrokerTimings::default(),
                            move |pending, _| {
                                tg_ref.attach_pending(pending);
                                None
                            },
                        )?;
                        generation.fetch_add(1, Ordering::SeqCst);
                        Ok(broker)
                    },
                )
                .await;
            })
        };

        let wait_for_gen = |want: usize| {
            let generation = Arc::clone(&generation);
            async move {
                for _ in 0..100 {
                    if generation.load(Ordering::SeqCst) >= want {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                panic!("timed out waiting for supervisor generation {want}");
            }
        };

        // 1. Initial start: request without matching deny pattern has hard_deny == None.
        wait_for_gen(1).await;
        let r1 = send_broker_request(
            &sock,
            serde_json::json!({
                "kind": "confirm", "title": "Widget", "text": "deploy forbidden-widget"
            }),
        )
        .await
        .unwrap();
        assert_eq!(r1["result"], "approved");
        assert_eq!(tg.sent.lock().unwrap().last().unwrap().hard_deny, None);

        // 2. Add a stricter extra_hard_deny_pattern: supervisor reloads broker immediately.
        agentdeck_core::settings::update_approver(&paths, |cfg| {
            cfg.extra_hard_deny_patterns
                .push("forbidden-widget".to_string());
            Ok(())
        })
        .unwrap();
        wait_for_gen(2).await;
        let r2 = send_broker_request(
            &sock,
            serde_json::json!({
                "kind": "confirm", "title": "Widget", "text": "deploy forbidden-widget"
            }),
        )
        .await
        .unwrap();
        assert_eq!(r2["result"], "approved");
        assert_eq!(
            tg.sent.lock().unwrap().last().unwrap().hard_deny.as_deref(),
            Some("extra_hard_deny_pattern")
        );

        // 3. Hold next decision pending in Telegram, then disable plugin: pending decision
        //    must be cancelled with CANCEL_BROKER_SHUTDOWN and socket must stop accepting.
        let sent_before = *tg.sent_count.borrow();
        let mut sent_rx = tg.sent_count.subscribe();
        let mut cancel_rx = tg.cancel_count.subscribe();
        tg.set_auto_action(None);
        let sock_clone = sock.clone();
        let pending_client = tokio::spawn(async move {
            send_broker_request(
                &sock_clone,
                serde_json::json!({
                    "kind": "confirm", "title": "Pending", "text": "wait for owner"
                }),
            )
            .await
        });
        tokio::time::timeout(
            Duration::from_secs(2),
            sent_rx.wait_for(|&n| n > sent_before),
        )
        .await
        .unwrap()
        .unwrap();

        plugins::set_plugin_state(&paths, plugins::PLUGIN_APPROVER, false, false).unwrap();

        tokio::time::timeout(Duration::from_secs(2), cancel_rx.wait_for(|&n| n >= 1))
            .await
            .unwrap()
            .unwrap();
        let cancelled = tg.cancelled.lock().unwrap().clone();
        assert!(
            cancelled
                .iter()
                .any(|(_, reason)| reason.contains("брокер остановлен")),
            "expected CANCEL_BROKER_SHUTDOWN, got {cancelled:?}"
        );
        let client_out = tokio::time::timeout(Duration::from_secs(2), pending_client)
            .await
            .unwrap()
            .unwrap();
        assert!(
            client_out.is_none(),
            "disabled broker must not approve pending client: {client_out:?}"
        );
        assert!(
            UnixStream::connect(&sock).await.is_err(),
            "disabled broker socket must not accept new connections"
        );

        let _ = shutdown_tx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(2), sup_task).await;
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
