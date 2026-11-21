use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use agentdeck_approver::{
    ApprovalRequest, ApproverConfig, ApproverSecrets, BotApiTelegram, HttpPolicyJudge,
    JudgeVerdict, MandateContext, PendingDecisions, PolicyJudge, UnixBrokerServer, audit_log_path,
    check_perimeter_with_extra,
};
use agentdeck_coordinator::checkpoint::{
    CheckpointInput, CheckpointJudge, HeuristicCheckpointJudge, HttpCheckpointJudge,
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
const MANDATE_REFRESH_SECS: u64 = 2;

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
    let m = st.mandate.as_ref()?;
    Some(MandateContext {
        active_autonomous_run: true,
        owner_granted: m.owner_granted,
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

/// Resolve the active `MandateContext` for a specific request from `thread_state::list(paths)`.
/// Requires an explicit `req.thread_id` so unrelated callers never borrow another run's mandate.
fn resolve_request_mandate(paths: &Paths, req: &ApprovalRequest) -> Option<MandateContext> {
    let tid = req.thread_id.as_deref().map(str::trim).filter(|s| !s.is_empty())?;
    let threads = thread_state::list(paths);
    threads
        .iter()
        .find(|(id, _)| id == tid)
        .and_then(|(id, st)| mandate_from_thread(id, st))
}

/// Snapshot kept in `UnixBrokerServer`'s `RwLock`: exact mandate when 0 or 1
/// `Running` thread exists; when multiple threads run concurrently, union of
/// allowed hosts/apps so `DynamicPolicyJudge` can narrow to the exact thread.
fn resolve_aggregate_mandate(paths: &Paths) -> Option<MandateContext> {
    let active: Vec<MandateContext> = thread_state::list(paths)
        .iter()
        .filter_map(|(id, st)| mandate_from_thread(id, st))
        .collect();
    match active.as_slice() {
        [] => None,
        [single] => Some(single.clone()),
        many => {
            let mut merged = many[0].clone();
            merged.thread_id = None;
            merged.owner_granted = many.iter().any(|m| m.owner_granted);
            merged.allow_releases = many.iter().any(|m| m.allow_releases);
            let any_open_hosts = many.iter().any(|m| m.allowed_hosts.is_empty());
            if any_open_hosts {
                merged.allowed_hosts.clear();
            } else {
                for m in &many[1..] {
                    for h in &m.allowed_hosts {
                        if !merged.allowed_hosts.iter().any(|x| x.eq_ignore_ascii_case(h)) {
                            merged.allowed_hosts.push(h.clone());
                        }
                    }
                }
            }
            let any_open_apps = many.iter().any(|m| m.allowed_apps.is_empty());
            if any_open_apps {
                merged.allowed_apps.clear();
            } else {
                for m in &many[1..] {
                    for a in &m.allowed_apps {
                        if !merged.allowed_apps.iter().any(|x| x.eq_ignore_ascii_case(a)) {
                            merged.allowed_apps.push(a.clone());
                        }
                    }
                }
            }
            Some(merged)
        }
    }
}

struct DynamicPolicyJudge {
    paths: Paths,
    extra_hard_deny_patterns: Vec<String>,
    inner: Option<HttpPolicyJudge>,
}

impl PolicyJudge for DynamicPolicyJudge {
    async fn evaluate(
        &self,
        req: &ApprovalRequest,
        _fallback_mandate: &MandateContext,
    ) -> anyhow::Result<JudgeVerdict> {
        let Some(mandate) = resolve_request_mandate(&self.paths, req) else {
            return Ok(JudgeVerdict {
                allow: false,
                confidence: 0.0,
                reason: "no active Run Mandate matched request thread/host".to_string(),
            });
        };
        if check_perimeter_with_extra(req, Some(&mandate), &self.extra_hard_deny_patterns)
            .is_hard_deny()
        {
            return Ok(JudgeVerdict {
                allow: false,
                confidence: 0.0,
                reason: "blocked by Hard-Deny Perimeter for matched thread mandate".to_string(),
            });
        }
        let Some(inner) = &self.inner else {
            return Ok(JudgeVerdict {
                allow: false,
                confidence: 0.0,
                reason: "policy judge API key not configured".to_string(),
            });
        };
        inner.evaluate(req, &mandate).await
    }
}

async fn run_approver_task(paths: Paths, shutdown: watch::Receiver<bool>) {
    let reg = match Registry::load(&paths) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("agentdeck approver: failed to load registry: {e:#}");
            return;
        }
    };
    if !plugins::plugin_enabled(&reg, plugins::PLUGIN_APPROVER) {
        return;
    }
    let cfg: ApproverConfig = match cmd_approver::approver_config(&reg) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("agentdeck approver: invalid [approver] config (staying dormant): {e:#}");
            return;
        }
    };
    let secrets_path = ApproverSecrets::secrets_path(&paths.config_dir);
    let secrets = match ApproverSecrets::load(&paths.config_dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "agentdeck approver: cannot read {} (staying dormant): {e:#}",
                secrets_path.display()
            );
            return;
        }
    };
    let (Some(bot_token), Some(owner_id)) = (secrets.telegram_bot_token.clone(), secrets.telegram_owner_id) else {
        eprintln!(
            "agentdeck approver: TELEGRAM_BOT_TOKEN / TELEGRAM_OWNER_ID not configured in {}; broker dormant",
            secrets_path.display()
        );
        return;
    };
    let telegram = match BotApiTelegram::new(bot_token, owner_id) {
        Ok(t) => Arc::new(t),
        Err(e) => {
            eprintln!("agentdeck approver: Telegram client init failed: {e:#}");
            return;
        }
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
    let inner_judge = match judge_key {
        Some(key) => {
            match HttpPolicyJudge::new(cfg.judge_provider.clone(), cfg.judge_model.clone(), key) {
                Ok(j) => Some(j),
                Err(e) => {
                    eprintln!("agentdeck approver: Policy Judge disabled ({e:#}); escalating to Telegram");
                    None
                }
            }
        }
        None => {
            eprintln!(
                "agentdeck approver: no Policy Judge API key for {provider}; all approvals will escalate to Telegram"
            );
            None
        }
    };
    let judge = Arc::new(DynamicPolicyJudge {
        paths: paths.clone(),
        extra_hard_deny_patterns: cfg.extra_hard_deny_patterns.clone(),
        inner: inner_judge,
    });
    let mandate_lock = Arc::new(RwLock::new(resolve_aggregate_mandate(&paths)));
    let pending = Arc::new(PendingDecisions::new());
    let socket_path = cfg.resolved_socket_path();
    let audit_path = audit_log_path(&paths.state_dir);

    let server = match UnixBrokerServer::bind(
        socket_path.clone(),
        cfg,
        Arc::clone(&mandate_lock),
        judge,
        Arc::clone(&telegram),
        Arc::clone(&pending),
        Some(audit_path),
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("agentdeck approver: failed to bind broker socket: {e:#}");
            return;
        }
    };
    eprintln!(
        "agentdeck approver: listening on {}",
        socket_path.display()
    );

    let tg_poll = {
        let telegram = Arc::clone(&telegram);
        let pending = Arc::clone(&pending);
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            telegram.poll_updates_loop(pending, shutdown).await;
        })
    };
    let mandate_refresh = {
        let paths = paths.clone();
        let lock = Arc::clone(&mandate_lock);
        let mut shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(MANDATE_REFRESH_SECS));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = shutdown.changed() => break,
                    _ = tick.tick() => {
                        let next = resolve_aggregate_mandate(&paths);
                        if let Ok(mut guard) = lock.write() {
                            *guard = next;
                        }
                    }
                }
            }
        })
    };

    if let Err(e) = server.run(shutdown).await {
        eprintln!("agentdeck approver: broker stopped with error: {e:#}");
    }
    tg_poll.abort();
    mandate_refresh.abort();
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

async fn watchdog_tick_with_controller<C: T3ThreadController>(
    paths: &Paths,
    t3_root: &Path,
    cfg: &agentdeck_coordinator::CoordinatorConfig,
    controller: Option<&C>,
    debounce: &mut ActionDebounce,
    now_epoch_secs: i64,
    logged_no_endpoint: &mut bool,
) {
    let quotas = cmd_coordinator::collect_pool_quotas(cfg, Some(t3_root));
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

        let checkpoint_verdict = if state.phase == thread_state::RunPhase::Running
            && !observed.is_running
            && !observed.hit_quota_limit
            && state.has_unblocked_work()
        {
            let input = CheckpointInput {
                run_count_since_checkpoint: observed.run_count as u32,
                item_count_since_checkpoint: observed.item_count as u32,
                milestone_just_completed: false,
                state_stale: state.state_stale(),
                active_children: observed.pending_background_tasks as usize,
                recent_events_tail: if observed.last_message_text.is_empty() {
                    Vec::new()
                } else {
                    vec![observed.last_message_text.clone()]
                },
            };
            match HttpCheckpointJudge::from_env(cfg) {
                Ok(http) => http.judge(&input, cfg).await.ok(),
                Err(_) => HeuristicCheckpointJudge.judge(&input, cfg).await.ok(),
            }
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
                    match thread_state::set_run_meta(
                        paths,
                        Some(t3_root),
                        thread_id,
                        Some(thread_state::RunPhase::SleepingForQuota),
                        None,
                        Some(Some(wake_iso.clone())),
                        false,
                    ) {
                        Ok(_) => eprintln!(
                            "agentdeck watchdog: {thread_id} entering quota sleep until {wake_iso} ({reason})"
                        ),
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
                match execute_action(ctrl, &action, debounce, now_epoch_secs).await {
                    Ok(true) => {
                        eprintln!(
                            "agentdeck watchdog: executed {} on {thread_id}",
                            action.kind()
                        );
                        if state.phase == thread_state::RunPhase::SleepingForQuota
                            && let Err(e) = thread_state::set_run_meta(
                                paths,
                                Some(t3_root),
                                &thread_id,
                                Some(thread_state::RunPhase::Running),
                                None,
                                Some(None),
                                false,
                            )
                        {
                            eprintln!(
                                "agentdeck watchdog: failed to clear sleep_until for {thread_id}: {e:#}"
                            );
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

async fn run_watchdog_task(paths: Paths, mut shutdown: watch::Receiver<bool>) {
    let mut debounce = ActionDebounce::new(WATCHDOG_DEBOUNCE_SECS);
    let mut logged_no_endpoint = false;
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
        let controller = match cfg.t3_endpoint.as_deref() {
            Some(ep) => match HttpT3Controller::new(ep, cfg.t3_token_file.as_deref()) {
                Ok(c) => Some(c),
                Err(e) => {
                    eprintln!("agentdeck watchdog: invalid t3_endpoint: {e:#}");
                    None
                }
            },
            None => None,
        };
        watchdog_tick_with_controller(
            &paths,
            &t3_root,
            &cfg,
            controller.as_ref(),
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
    use std::sync::Mutex;

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

    #[tokio::test]
    async fn watchdog_tick_enters_quota_sleep_then_wakes_and_clears_sleep_until() {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-daemon-wd-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = Paths::new(dir.join("config"), dir.join("state"));
        let t3_root = dir.join("t3");
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
             );",
        )
        .unwrap();
        db.execute(
            "INSERT INTO orchestration_v2_projection_threads VALUES ('t-wd','Coord','2026-11-21T01:00:00Z','{}','claude_a')",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO orchestration_v2_projection_runs VALUES ('r1','t-wd',1,'failed','2026-11-21T01:00:00Z','2026-11-21T01:05:00Z','{}','claude_a')",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO orchestration_v2_projection_turn_items VALUES ('e1','t-wd','r1',1,'error','failed',?1)",
            [r#"{"failure":{"class":"usage_limit","message":"5-hour limit reached"}}"#],
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

    #[test]
    fn resolve_request_mandate_matches_thread_id_and_rejects_non_running() {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-daemon-mandate-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = Paths::new(dir.join("config"), dir.join("state"));
        thread_state::set_run_meta(
            &paths,
            None,
            "t-run",
            Some(thread_state::RunPhase::Running),
            Some(thread_state::RunMandate {
                project: "blockfield".into(),
                allowed_hosts: vec!["homeserver".into()],
                allowed_apps: vec!["staging".into()],
                ..Default::default()
            }),
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
        };
        assert_eq!(
            resolve_request_mandate(&paths, &mk(Some("t-run")))
                .unwrap()
                .project,
            "blockfield"
        );
        // Explicitly naming a non-running thread must NOT inherit another thread's mandate.
        assert!(resolve_request_mandate(&paths, &mk(Some("t-intake"))).is_none());
        // Omitting thread_id must NOT borrow an active thread's mandate (fail closed).
        assert!(resolve_request_mandate(&paths, &mk(None)).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
