//! Watchdog eval suite (Q21): pool selection, checkpoint goldens, the three
//! real Blockfield failures plus the Owner-Inbox quiet case, loopback
//! enforcement, the SQLite observer, and a mock-server T3 controller test.
//!
//! Only temp dirs are touched; no real T3, network, or home directory.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agentdeck_coordinator::checkpoint::{
    CheckpointInput, CheckpointJudge, CheckpointVerdict, HeuristicCheckpointJudge, record_outcome,
};
use agentdeck_coordinator::config::{CoordinatorConfig, CoordinatorPoolEntry};
use agentdeck_coordinator::hygiene::{check_disk_ok, find_stale_worktrees, free_disk_gb};
use agentdeck_coordinator::pool::{
    PoolSelection, QuotaWindowView, parse_epoch_rfc3339, quota_from_t3_cache,
    select_coordinator, should_proactively_rotate,
};
use agentdeck_coordinator::t3_control::{
    HttpT3Controller, ObservedThread, T3ThreadController, is_quota_limit_message,
    observe_thread_sqlite,
};
use agentdeck_coordinator::watchdog::{
    ActionDebounce, RunPhase, RunSnapshot, WatchdogAction, WatchdogThreadInput, decide,
    execute_action,
};

const NOW: i64 = 1_750_000_000;

fn pool2() -> Vec<CoordinatorPoolEntry> {
    vec![
        CoordinatorPoolEntry::new("claude_a", "claude-opus-4-6"),
        CoordinatorPoolEntry::new("codex_b", "gpt-5.4"),
    ]
}

fn cfg_with_pool() -> CoordinatorConfig {
    CoordinatorConfig {
        pool: pool2(),
        ..Default::default()
    }
}

fn quotas(pairs: &[(&str, QuotaWindowView)]) -> HashMap<String, QuotaWindowView> {
    pairs
        .iter()
        .map(|(id, q)| (id.to_string(), q.clone()))
        .collect()
}

fn view(five: f64, weekly: f64) -> QuotaWindowView {
    QuotaWindowView {
        five_hour_remaining_pct: Some(five),
        five_hour_resets_at: None,
        weekly_remaining_pct: Some(weekly),
        weekly_resets_at: None,
        healthy: true,
    }
}

fn run(thread: &str) -> RunSnapshot {
    RunSnapshot {
        thread_id: thread.to_string(),
        phase: RunPhase::Running,
        sleep_until_epoch: None,
        has_unblocked_work: true,
    }
}

fn observed(thread: &str) -> ObservedThread {
    ObservedThread {
        thread_id: thread.to_string(),
        updated_at_epoch: NOW - 3600,
        ..Default::default()
    }
}

// --- pool selection ---------------------------------------------------------

#[test]
fn pool_picks_fullest_5h_window() {
    let cfg = cfg_with_pool();
    let q = quotas(&[("claude_a", view(40.0, 80.0)), ("codex_b", view(75.0, 50.0))]);
    match select_coordinator(&cfg, &q, NOW, None) {
        PoolSelection::Selected { entry, five_hour_pct, .. } => {
            assert_eq!(entry.instance_id, "codex_b");
            assert_eq!(five_hour_pct, 75.0);
        }
        other => panic!("expected Selected, got {other:?}"),
    }
}

#[test]
fn pool_weekly_reserve_blocks_thin_weekly_account() {
    // codex_b is fuller on 5h but its weekly window is at 1%: guarded.
    let cfg = cfg_with_pool();
    let q = quotas(&[("claude_a", view(30.0, 60.0)), ("codex_b", view(90.0, 1.0))]);
    match select_coordinator(&cfg, &q, NOW, None) {
        PoolSelection::Selected { entry, .. } => assert_eq!(entry.instance_id, "claude_a"),
        other => panic!("expected claude_a, got {other:?}"),
    }
}

#[test]
fn pool_one_percent_5h_is_ineligible() {
    let cfg = cfg_with_pool();
    let q = quotas(&[("claude_a", view(1.0, 60.0)), ("codex_b", view(50.0, 60.0))]);
    match select_coordinator(&cfg, &q, NOW, None) {
        PoolSelection::Selected { entry, .. } => assert_eq!(entry.instance_id, "codex_b"),
        other => panic!("expected codex_b, got {other:?}"),
    }
}

#[test]
fn pool_all_exhausted_sleeps_until_earliest_reset_plus_120s() {
    let cfg = cfg_with_pool();
    let mut a = QuotaWindowView::exhausted(Some(NOW + 3000), Some(NOW + 9000));
    a.healthy = true;
    let mut b = QuotaWindowView::exhausted(Some(NOW + 1000), Some(NOW + 8000));
    b.healthy = true;
    let q = quotas(&[("claude_a", a), ("codex_b", b)]);
    match select_coordinator(&cfg, &q, NOW, None) {
        PoolSelection::SleepForQuota {
            wake_at_epoch_secs,
            earliest_reset_epoch_secs,
            next_candidate,
            ..
        } => {
            assert_eq!(earliest_reset_epoch_secs, Some(NOW + 1000));
            assert_eq!(wake_at_epoch_secs, NOW + 1000 + 120);
            assert_eq!(next_candidate.unwrap().instance_id, "codex_b");
        }
        other => panic!("expected SleepForQuota, got {other:?}"),
    }
}

#[test]
fn pool_hysteresis_keeps_current_within_2pct() {
    let cfg = cfg_with_pool();
    let q = quotas(&[("claude_a", view(73.5, 80.0)), ("codex_b", view(75.0, 80.0))]);
    match select_coordinator(&cfg, &q, NOW, Some("claude_a")) {
        PoolSelection::Selected { entry, reason, .. } => {
            assert_eq!(entry.instance_id, "claude_a");
            assert!(reason.contains("prompt cache"), "{reason}");
        }
        other => panic!("expected hysteresis keep, got {other:?}"),
    }
    // Beyond 2% the best entry wins.
    let q = quotas(&[("claude_a", view(70.0, 80.0)), ("codex_b", view(75.0, 80.0))]);
    match select_coordinator(&cfg, &q, NOW, Some("claude_a")) {
        PoolSelection::Selected { entry, .. } => assert_eq!(entry.instance_id, "codex_b"),
        other => panic!("expected codex_b, got {other:?}"),
    }
}

#[test]
fn pool_skips_unhealthy_and_disabled_entries() {
    let mut cfg = cfg_with_pool();
    cfg.pool[0].enabled = false;
    let mut bad = view(99.0, 99.0);
    bad.healthy = false;
    let q = quotas(&[("claude_a", view(99.0, 99.0)), ("codex_b", bad)]);
    // claude_a disabled, codex_b unhealthy: sleep with no known reset.
    match select_coordinator(&cfg, &q, NOW, None) {
        PoolSelection::SleepForQuota { next_candidate, .. } => assert!(next_candidate.is_none()),
        other => panic!("expected SleepForQuota, got {other:?}"),
    }
}

#[test]
fn pool_empty_is_no_pool_configured() {
    let cfg = CoordinatorConfig::default();
    assert_eq!(
        select_coordinator(&cfg, &HashMap::new(), NOW, None),
        PoolSelection::NoPoolConfigured
    );
}

#[test]
fn proactive_rotate_needs_thin_current_and_headroom() {
    let cfg = cfg_with_pool();
    assert!(should_proactively_rotate(&cfg, &view(5.0, 80.0), &view(60.0, 80.0)));
    // Thin is defined by either window: weekly below reserve also qualifies.
    assert!(should_proactively_rotate(&cfg, &view(50.0, 5.0), &view(80.0, 80.0)));
    // Comfortable current: no rotation.
    assert!(!should_proactively_rotate(&cfg, &view(50.0, 80.0), &view(90.0, 80.0)));
    // Less than 15 points of headroom: no flap.
    assert!(!should_proactively_rotate(&cfg, &view(5.0, 80.0), &view(19.0, 80.0)));
}

#[test]
fn rfc3339_parses_t3_shapes() {
    assert_eq!(parse_epoch_rfc3339("2026-11-13T11:00:00Z"), Some(1_794_567_600));
    assert_eq!(parse_epoch_rfc3339("2026-11-13T10:00:00.000Z"), Some(1_794_564_000));
    assert_eq!(
        parse_epoch_rfc3339("2026-11-13T11:00:00+00:00"),
        Some(1_794_567_600)
    );
    assert!(parse_epoch_rfc3339("not a date").is_none());
    assert!(parse_epoch_rfc3339("2026-13-01T00:00:00Z").is_none());
}

// --- checkpoint golden eval (>= 20 scenarios) --------------------------------

fn input(
    runs: u32,
    items: u32,
    milestone: bool,
    stale: bool,
    children: usize,
    events: &[&str],
) -> CheckpointInput {
    CheckpointInput {
        run_count_since_checkpoint: runs,
        item_count_since_checkpoint: items,
        milestone_just_completed: milestone,
        state_stale: stale,
        active_children: children,
        recent_events_tail: events.iter().map(|s| s.to_string()).collect(),
    }
}

#[tokio::test]
async fn checkpoint_golden_eval() {
    let judge = HeuristicCheckpointJudge;
    let cfg = CoordinatorConfig::default();
    // (name, input, expected should_checkpoint, required reason substring)
    let cases: Vec<(&str, CheckpointInput, bool, &str)> = vec![
        ("stale quiet", input(0, 0, false, true, 0, &[]), false, "stale"),
        ("stale milestone", input(0, 0, true, true, 0, &[]), false, "stale"),
        ("stale over threshold", input(200, 5000, false, true, 0, &[]), false, "stale"),
        ("stale with children", input(200, 5000, true, true, 3, &[]), false, "stale"),
        ("stale beats hazard", input(200, 5000, true, true, 0, &["merge conflict"]), false, "stale"),
        ("quiet turn", input(0, 0, false, false, 0, &[]), false, "prompt cache"),
        ("small turn", input(10, 100, false, false, 0, &[]), false, "prompt cache"),
        ("just under both", input(79, 1499, false, false, 0, &[]), false, "prompt cache"),
        ("at run threshold", input(80, 0, false, false, 0, &[]), true, "threshold"),
        ("at item threshold", input(0, 1500, false, false, 0, &[]), true, "threshold"),
        ("well over", input(200, 5000, false, false, 0, &[]), true, "threshold"),
        ("clean milestone", input(3, 12, true, false, 0, &["shipped unit U1"]), true, "milestone"),
        ("over threshold with child", input(200, 5000, false, false, 1, &[]), false, "still running"),
        ("milestone with children", input(0, 0, true, false, 2, &[]), false, "still running"),
        ("merge conflict", input(200, 5000, true, false, 0, &["merge conflict in main"]), false, "merge conflict"),
        ("mid rollback", input(200, 5000, false, false, 0, &["mid-rollback of migration"]), false, "mid-rollback"),
        ("deploy in flight", input(200, 5000, false, false, 0, &["in progress deploy to prod"]), false, "deploy"),
        ("half applied", input(200, 5000, false, false, 0, &["half-applied schema change"]), false, "half-applied"),
        ("rollback failed", input(200, 5000, false, false, 0, &["rollback failed, manual fix needed"]), false, "rollback failed"),
        ("hazard beats quiet-refuse", input(1, 2, false, false, 0, &["merge conflict"]), false, ""),
        ("milestone hazard refused", input(0, 0, true, false, 0, &["half-applied migration"]), false, "half-applied"),
        ("benign tail approved", input(200, 5000, false, false, 0, &["merged PR #12", "tests green"]), true, "threshold"),
        ("milestone clean events", input(5, 30, true, false, 0, &["eval suite green"]), true, "milestone"),
    ];
    assert!(cases.len() >= 20, "eval suite needs >= 20 scenarios");
    for (name, inp, want, reason_part) in cases {
        let v: CheckpointVerdict = judge.judge(&inp, &cfg).await.unwrap();
        assert_eq!(v.should_checkpoint, want, "case {name}: {v:?}");
        if !reason_part.is_empty() {
            assert!(
                v.reason.to_lowercase().contains(reason_part),
                "case {name}: reason {:?} lacks {reason_part:?}",
                v.reason
            );
        }
    }
}

#[test]
fn checkpoint_outcome_log_appends_jsonl() {
    let dir = std::env::temp_dir().join(format!(
        "coord-eval-outcomes-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    let verdict = CheckpointVerdict {
        should_checkpoint: true,
        confidence: 0.9,
        reason: "test".into(),
    };
    record_outcome(&dir, &input(90, 10, false, false, 0, &[]), &verdict).unwrap();
    record_outcome(&dir, &input(0, 0, false, true, 0, &[]), &CheckpointVerdict::refuse("x")).unwrap();
    let text = std::fs::read_to_string(dir.join("coordinator/checkpoint-outcomes.jsonl")).unwrap();
    assert_eq!(text.lines().count(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

// --- watchdog state machine: the real failures -------------------------------

fn tick_input(run: RunSnapshot, observed: ObservedThread) -> WatchdogThreadInput {
    WatchdogThreadInput {
        run,
        observed,
        quotas: HashMap::new(),
        now_epoch_secs: NOW,
        checkpoint_verdict: None,
    }
}

#[test]
fn failure1_weekly_limit_rotates_to_healthy_account_overnight() {
    let cfg = cfg_with_pool();
    let mut r = run("t1");
    r.phase = RunPhase::Running;
    let mut o = observed("t1");
    o.provider_instance_id = Some("claude_a".into());
    o.last_message_text = "You've hit your weekly limit. Try again Sunday.".into();
    o.hit_quota_limit = true;
    let mut inp = tick_input(r, o);
    inp.quotas = quotas(&[
        ("claude_a", QuotaWindowView {
            five_hour_remaining_pct: Some(80.0),
            weekly_remaining_pct: Some(0.0),
            ..QuotaWindowView { healthy: true, ..Default::default() }
        }),
        ("codex_b", view(70.0, 60.0)),
    ]);
    match decide(&cfg, &inp) {
        WatchdogAction::RotateAndWake { to_entry, fresh_context, message, .. } => {
            assert_eq!(to_entry.instance_id, "codex_b");
            assert!(!fresh_context);
            assert!(message.contains("state_read"));
        }
        other => panic!("expected RotateAndWake, got {other:?}"),
    }
}

#[test]
fn failure2_all_exhausted_sleeps_cleanly_until_reset_plus_2m() {
    let cfg = cfg_with_pool();
    let mut r = run("t2");
    r.phase = RunPhase::Running;
    let mut o = observed("t2");
    o.provider_instance_id = Some("claude_a".into());
    o.last_message_text = "You've hit your 5-hour limit".into();
    o.hit_quota_limit = true;
    let mut inp = tick_input(r, o);
    inp.quotas = quotas(&[
        ("claude_a", QuotaWindowView::exhausted(Some(NOW + 5000), None)),
        ("codex_b", QuotaWindowView::exhausted(Some(NOW + 2000), None)),
    ]);
    match decide(&cfg, &inp) {
        WatchdogAction::EnterQuotaSleep { wake_at_epoch_secs, .. } => {
            assert_eq!(wake_at_epoch_secs, NOW + 2000 + 120);
        }
        other => panic!("expected EnterQuotaSleep, got {other:?}"),
    }
}

#[test]
fn sleeping_run_wakes_and_rotates_after_deadline() {
    let cfg = cfg_with_pool();
    let r = RunSnapshot {
        thread_id: "t2b".into(),
        phase: RunPhase::SleepingForQuota,
        sleep_until_epoch: Some(NOW - 1),
        has_unblocked_work: true,
    };
    let mut o = observed("t2b");
    o.provider_instance_id = Some("claude_a".into());
    let mut inp = tick_input(r, o);
    inp.quotas = quotas(&[("claude_a", view(60.0, 60.0)), ("codex_b", view(70.0, 60.0))]);
    match decide(&cfg, &inp) {
        WatchdogAction::RotateAndWake { .. } => {}
        other => panic!("expected RotateAndWake, got {other:?}"),
    }
    // Still inside the sleep window: absolute quiet, zero spam.
    let r = RunSnapshot {
        thread_id: "t2b".into(),
        phase: RunPhase::SleepingForQuota,
        sleep_until_epoch: Some(NOW + 600),
        has_unblocked_work: true,
    };
    let inp = tick_input(r, observed("t2b"));
    assert!(matches!(decide(&cfg, &inp), WatchdogAction::Noop { .. }));
}

#[test]
fn failure3_silent_idle_with_open_work_wakes_after_grace() {
    let cfg = cfg_with_pool();
    let inp = tick_input(run("t3"), observed("t3"));
    match decide(&cfg, &inp) {
        WatchdogAction::Wake { message, fresh_context, .. } => {
            assert!(!fresh_context);
            assert!(message.contains("state_read"));
        }
        other => panic!("expected Wake, got {other:?}"),
    }
    // Inside the grace period: no wake.
    let mut o = observed("t3");
    o.updated_at_epoch = NOW - 10;
    let inp = tick_input(run("t3"), o);
    assert!(matches!(decide(&cfg, &inp), WatchdogAction::Noop { .. }));
}

#[test]
fn owner_inbox_only_run_stays_quiet() {
    let cfg = cfg_with_pool();
    let mut r = run("t4");
    r.has_unblocked_work = false;
    let inp = tick_input(r, observed("t4"));
    match decide(&cfg, &inp) {
        WatchdogAction::Noop { reason } => assert!(reason.contains("Owner Inbox")),
        other => panic!("expected Noop, got {other:?}"),
    }
}

#[test]
fn watchdog_priorities_running_bg_checkpoint() {
    let cfg = cfg_with_pool();
    // Active run beats everything.
    let mut o = observed("p");
    o.is_running = true;
    o.hit_quota_limit = true;
    assert!(matches!(decide(&cfg, &tick_input(run("p"), o)), WatchdogAction::Noop { .. }));
    // Background tasks beat the idle wake.
    let mut o = observed("p");
    o.pending_background_tasks = 2;
    assert!(matches!(decide(&cfg, &tick_input(run("p"), o)), WatchdogAction::Noop { .. }));
    // Paused / finished runs are never touched.
    for phase in [RunPhase::Paused, RunPhase::Finished] {
        let mut r = run("p");
        r.phase = phase;
        assert!(matches!(decide(&cfg, &tick_input(r, observed("p"))), WatchdogAction::Noop { .. }));
    }
    // Approved checkpoint beats the plain wake.
    let mut inp = tick_input(run("p"), observed("p"));
    inp.checkpoint_verdict = Some(CheckpointVerdict {
        should_checkpoint: true,
        confidence: 0.9,
        reason: "clean milestone".into(),
    });
    assert!(matches!(
        decide(&cfg, &inp),
        WatchdogAction::TriggerCheckpoint { .. }
    ));
    // Low-confidence verdict falls through to wake.
    let mut inp = tick_input(run("p"), observed("p"));
    inp.checkpoint_verdict = Some(CheckpointVerdict {
        should_checkpoint: true,
        confidence: 0.5,
        reason: "unsure".into(),
    });
    assert!(matches!(decide(&cfg, &inp), WatchdogAction::Wake { .. }));
}

#[test]
fn proactive_rotation_fires_on_silent_thin_account() {
    let cfg = cfg_with_pool();
    let mut o = observed("t5");
    o.provider_instance_id = Some("claude_a".into());
    let mut inp = tick_input(run("t5"), o);
    inp.quotas = quotas(&[("claude_a", view(5.0, 80.0)), ("codex_b", view(60.0, 80.0))]);
    match decide(&cfg, &inp) {
        WatchdogAction::RotateAndWake { to_entry, .. } => {
            assert_eq!(to_entry.instance_id, "codex_b");
        }
        other => panic!("expected proactive RotateAndWake, got {other:?}"),
    }
}

// --- executor + debounce -----------------------------------------------------

#[derive(Debug, Default)]
struct FakeController {
    configured: Mutex<Vec<(String, String, String)>>,
    messaged: Mutex<Vec<(String, String, bool)>>,
}

impl T3ThreadController for FakeController {
    async fn configure_thread(
        &self,
        thread_id: &str,
        instance_id: &str,
        model: &str,
        _options: &HashMap<String, serde_json::Value>,
    ) -> anyhow::Result<()> {
        self.configured.lock().unwrap().push((thread_id.into(), instance_id.into(), model.into()));
        Ok(())
    }

    async fn send_message(
        &self,
        thread_id: &str,
        message: &str,
        fresh_context: bool,
    ) -> anyhow::Result<()> {
        self.messaged.lock().unwrap().push((thread_id.into(), message.into(), fresh_context));
        Ok(())
    }
}

#[tokio::test]
async fn executor_debounces_double_wake_and_reports_sleeps() {
    let c = FakeController::default();
    let mut debounce = ActionDebounce::new(300);
    let wake = WatchdogAction::Wake {
        thread_id: "t".into(),
        fresh_context: false,
        message: "hi".into(),
        reason: "test".into(),
    };
    assert!(execute_action(&c, &wake, &mut debounce, NOW).await.unwrap());
    // Same tick again: debounced, no second T3 call.
    assert!(!execute_action(&c, &wake, &mut debounce, NOW + 10).await.unwrap());
    assert_eq!(c.messaged.lock().unwrap().len(), 1);
    // After the window: fires again.
    assert!(execute_action(&c, &wake, &mut debounce, NOW + 301).await.unwrap());
    // Noop never touches T3; sleep transitions report true without T3 calls.
    let noop = WatchdogAction::Noop { reason: "x".into() };
    assert!(!execute_action(&c, &noop, &mut debounce, NOW).await.unwrap());
    let sleep = WatchdogAction::EnterQuotaSleep {
        thread_id: "t".into(),
        wake_at_epoch_secs: NOW + 120,
        reason: "x".into(),
    };
    assert!(execute_action(&c, &sleep, &mut debounce, NOW).await.unwrap());
    assert_eq!(c.messaged.lock().unwrap().len(), 2);
}

// --- quota message detection --------------------------------------------------

#[test]
fn quota_limit_messages_cover_all_harnesses() {
    for text in [
        "You've hit your 5-hour limit. Try again at 3pm.",
        "You've hit your weekly limit across all Claude models.",
        "You've been rate limited for the hour",
        "Error 429: too many requests",
        "Antigravity quota exceeded for this account",
        "Codex usage limit reached, resets at midnight",
        "Plan limit hit; upgrade or wait",
    ] {
        assert!(is_quota_limit_message(text), "{text}");
    }
    for text in ["All tests passed", "error: file not found", "limitless potential"] {
        assert!(!is_quota_limit_message(text), "{text}");
    }
}

// --- usage-cache quota view ---------------------------------------------------

fn write_cache(t3: &Path, id: &str, body: &serde_json::Value) {
    std::fs::create_dir_all(t3.join("caches")).unwrap();
    std::fs::write(t3.join(format!("caches/{id}.json")), body.to_string()).unwrap();
}

fn tmpdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "coord-eval-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn quota_view_reads_5h_and_weekly_windows() {
    let dir = tmpdir("cache");
    write_cache(
        &dir,
        "claude_a",
        &serde_json::json!({
            "status": "ready",
            "checkedAt": "2026-11-13T10:00:00.000Z",
            "usageLimits": {"checkedAt": "2026-11-13T10:00:00.000Z", "windows": [
                {"id": "primary", "kind": "session", "label": "Session",
                 "usedPercent": 25.0, "resetsAt": "2026-11-13T15:00:00Z"},
                {"id": "weekly", "kind": "week", "label": "Weekly",
                 "windowDurationMins": 10080, "usedPercent": 40.0,
                 "resetsAt": "2026-11-20T00:00:00Z"},
            ]},
        }),
    );
    let v = quota_from_t3_cache(&dir, "claude_a", "claude-opus-4-6");
    assert!(v.healthy);
    assert_eq!(v.five_hour_remaining_pct, Some(75.0));
    assert_eq!(v.weekly_remaining_pct, Some(60.0));
    assert_eq!(v.five_hour_resets_at, parse_epoch_rfc3339("2026-11-13T15:00:00Z"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn quota_view_respects_antigravity_buckets() {
    let dir = tmpdir("agy");
    write_cache(
        &dir,
        "agy",
        &serde_json::json!({
            "status": "ready",
            "usageLimits": {"windows": [
                {"id": "gemini-week", "label": "Gemini Weekly",
                 "windowDurationMins": 10080, "usedPercent": 10.0},
                {"id": "3p-5h", "label": "Third-party 5h",
                 "windowDurationMins": 300, "usedPercent": 90.0},
            ]},
        }),
    );
    // A Claude model on Antigravity sees the 3p window, not the Gemini one.
    let v = quota_from_t3_cache(&dir, "agy", "claude-sonnet-4-6");
    assert_eq!(v.five_hour_remaining_pct, Some(10.0));
    assert_eq!(v.weekly_remaining_pct, None);
    // A Gemini model sees neither (its buckets are gemini-* labelled).
    let v = quota_from_t3_cache(&dir, "agy", "gemini-3-flash");
    assert_eq!(v.five_hour_remaining_pct, None);
    assert_eq!(v.weekly_remaining_pct, Some(90.0));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn quota_view_missing_cache_is_unknown_healthy_error_is_not() {
    let dir = tmpdir("missing");
    std::fs::create_dir_all(dir.join("caches")).unwrap();
    let v = quota_from_t3_cache(&dir, "ghost", "m");
    assert!(v.healthy);
    assert_eq!(v.five_hour_remaining_pct, None);
    write_cache(&dir, "bad", &serde_json::json!({"status": "error", "message": "auth failed"}));
    let v = quota_from_t3_cache(&dir, "bad", "m");
    assert!(!v.healthy);
    let _ = std::fs::remove_dir_all(&dir);
}

// --- sqlite observer ----------------------------------------------------------

fn fixture_db(t3: &Path) {
    use rusqlite::Connection;
    std::fs::create_dir_all(t3.join("userdata")).unwrap();
    let db = Connection::open(t3.join("userdata/statev2.sqlite")).unwrap();
    db.execute_batch(
        "CREATE TABLE orchestration_v2_projection_threads
           (thread_id TEXT PRIMARY KEY, title TEXT, payload_json TEXT);
         CREATE TABLE orchestration_v2_projection_runs
           (run_id TEXT, provider_instance_id TEXT, provider_thread_id TEXT,
            payload_json TEXT, status TEXT, thread_id TEXT);
         CREATE TABLE orchestration_v2_projection_subagents
           (subagent_id TEXT, provider_instance_id TEXT, payload_json TEXT,
            started_at TEXT, origin TEXT, status TEXT, provider_thread_id TEXT,
            run_id TEXT, child_thread_id TEXT);",
    )
    .unwrap();
    db.execute(
        "INSERT INTO orchestration_v2_projection_threads VALUES (?1, ?2, ?3)",
        rusqlite::params![
            "t1",
            "Coordinator run",
            r#"{"updatedAt": 1749999900}"#,
        ],
    )
    .unwrap();
    db.execute(
        "INSERT INTO orchestration_v2_projection_runs VALUES (?1, ?2, NULL, ?3, ?4, ?5)",
        rusqlite::params![
            "r1",
            "claude_a",
            r#"{"modelSelection": {"model": "claude-opus-4-6"}, "lastMessage": "You've hit your 5-hour limit, try again soon"}"#,
            "running",
            "t1",
        ],
    )
    .unwrap();
    db.execute(
        "INSERT INTO orchestration_v2_projection_subagents VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, NULL)",
        rusqlite::params!["n1", "claude_a", "{}", "2026-01-01T00:00:00Z", "provider_native", "running", "r1"],
    )
    .unwrap();
}

#[test]
fn observer_reads_run_status_quota_tail_and_children() {
    let dir = tmpdir("sqlite");
    fixture_db(&dir);
    let o = observe_thread_sqlite(&dir, "t1").unwrap().expect("thread t1");
    assert_eq!(o.title.as_deref(), Some("Coordinator run"));
    assert!(o.is_running);
    assert_eq!(o.active_run_id.as_deref(), Some("r1"));
    assert_eq!(o.provider_instance_id.as_deref(), Some("claude_a"));
    assert_eq!(o.model.as_deref(), Some("claude-opus-4-6"));
    assert_eq!(o.run_count, 1);
    assert_eq!(o.updated_at_epoch, 1749999900);
    assert!(o.hit_quota_limit, "tail: {}", o.last_message_text);
    assert_eq!(o.pending_background_tasks, 1);
    assert!(observe_thread_sqlite(&dir, "nope").unwrap().is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn observer_absent_db_is_none() {
    let dir = tmpdir("nodb");
    assert!(observe_thread_sqlite(&dir, "t1").unwrap().is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

// --- loopback enforcement + mock T3 server ------------------------------------

#[test]
fn loopback_enforcement_rejects_remote_endpoints() {
    for bad in [
        "https://127.0.0.1:3773/mcp",
        "http://example.com/mcp",
        "http://192.168.1.5:3773/mcp",
        "http://127.0.0.1:3773/mcp?token=x",
        "http://user@127.0.0.1:3773/mcp",
        "http://127.0.0.1:3773/mcp#frag",
        "http://[::1]:3773/mcp\n",
    ] {
        assert!(HttpT3Controller::new(bad, None).is_err(), "{bad}");
    }
    for good in [
        "http://127.0.0.1:3773/mcp",
        "http://localhost:3773/mcp",
        "http://[::1]:3773/mcp",
    ] {
        assert!(HttpT3Controller::new(good, None).is_ok(), "{good}");
    }
}

struct Seen {
    bodies: Mutex<Vec<serde_json::Value>>,
    auths: Mutex<Vec<Option<String>>>,
}

/// Minimal HTTP server: answers every JSON-RPC request with an empty result,
/// recording bodies and authorization headers. Each connection is closed
/// after one response so the client never deadlocks on keep-alive.
fn serve_json_rpc(n_requests: usize) -> (std::net::SocketAddr, Arc<Seen>, std::thread::JoinHandle<()>) {
    let seen = Arc::new(Seen {
        bodies: Mutex::new(Vec::new()),
        auths: Mutex::new(Vec::new()),
    });
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen_clone = seen.clone();
    let handle = std::thread::spawn(move || {
        listener.set_nonblocking(false).ok();
        for _ in 0..n_requests {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).ok();
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            // Read headers first.
            loop {
                let n = stream.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..n]);
                if raw.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let text = String::from_utf8_lossy(&raw).into_owned();
            let len = text
                .lines()
                .find_map(|l| {
                    l.to_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            let header_end = text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(text.len());
            let mut body = raw[header_end.min(raw.len())..].to_vec();
            while body.len() < len {
                let n = stream.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                body.extend_from_slice(&buf[..n]);
            }
            let auth = text
                .lines()
                .find_map(|l| {
                    l.strip_prefix("authorization:")
                        .or_else(|| l.strip_prefix("Authorization:"))
                        .map(|v| v.trim().to_string())
                });
            let req: serde_json::Value = serde_json::from_slice(&body).unwrap();
            seen_clone.bodies.lock().unwrap().push(req.clone());
            seen_clone.auths.lock().unwrap().push(auth);
            let id = req.get("id").cloned().unwrap_or(serde_json::Value::Null);
            let res = format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{}}}}",
                id = id
            );
            let out = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                res.len(),
                res
            );
            stream.write_all(out.as_bytes()).unwrap();
            stream.flush().ok();
        }
    });
    (addr, seen, handle)
}

#[tokio::test]
async fn http_controller_calls_json_rpc_over_loopback() {
    let (addr, seen, handle) = serve_json_rpc(4);
    let endpoint = format!("http://127.0.0.1:{}/mcp", addr.port());
    let dir = tmpdir("token");
    let token_file = dir.join("t3.token");
    std::fs::write(&token_file, "test-token-1\n").unwrap();
    let c = HttpT3Controller::new(&endpoint, Some(&token_file)).unwrap();
    c.configure_thread("t1", "claude_a", "claude-opus-4-6", &HashMap::new())
        .await
        .unwrap();
    c.send_message("t1", "hello", false).await.unwrap();
    handle.join().unwrap();
    let bodies = seen.bodies.lock().unwrap();
    assert_eq!(bodies.len(), 4);
    let calls: Vec<&serde_json::Value> = bodies
        .iter()
        .filter(|b| b.get("method").and_then(|m| m.as_str()) == Some("tools/call"))
        .collect();
    assert_eq!(calls.len(), 2);
    let names: Vec<&str> = calls
        .iter()
        .map(|b| b["params"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["t3_thread_configure", "t3_thread_send"]);
    assert_eq!(calls[1]["params"]["arguments"]["threadId"], "t1");
    assert_eq!(calls[1]["params"]["arguments"]["freshContext"], false);
    for auth in seen.auths.lock().unwrap().iter() {
        assert_eq!(auth.as_deref(), Some("test-token-1"));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// --- hygiene ------------------------------------------------------------------

#[test]
fn disk_check_reports_against_minimum() {
    let dir = tmpdir("disk");
    let free = free_disk_gb(&dir).unwrap();
    assert!(check_disk_ok(&dir, 0).ok);
    assert!(!check_disk_ok(&dir, free + 1_000_000).ok);
    assert!(!check_disk_ok(&PathBuf::from("/nonexistent-coord-path-xyz"), 1).ok);
    let _ = std::fs::remove_dir_all(&dir);
}

fn git_repo(path: &Path) {
    std::process::Command::new("git").arg("init").arg("-b").arg("main").arg(path).output().unwrap();
    std::process::Command::new("git")
        .arg("-C").arg(path)
        .args(["config", "user.email", "t@t"])
        .output().unwrap();
    std::process::Command::new("git")
        .arg("-C").arg(path)
        .args(["config", "user.name", "t"])
        .output().unwrap();
    std::fs::write(path.join("f.txt"), "x").unwrap();
    std::process::Command::new("git").arg("-C").arg(path).args(["add", "."]).output().unwrap();
    std::process::Command::new("git").arg("-C").arg(path).args(["commit", "-m", "init"]).output().unwrap();
    std::process::Command::new("git")
        .arg("-C").arg(path)
        .args(["update-ref", "refs/remotes/origin/main", "HEAD"])
        .output().unwrap();
}

#[test]
fn stale_worktree_discovery_flags_only_clean_merged() {
    if std::process::Command::new("git").arg("--version").output().is_err() {
        return; // no git on PATH: nothing to discover with.
    }
    let dir = tmpdir("wt");
    // Stale: clean, HEAD == origin/main.
    git_repo(&dir.join("old-done"));
    // Dirty: untracked file present.
    git_repo(&dir.join("dirty"));
    std::fs::write(dir.join("dirty/untracked.txt"), "work").unwrap();
    // Not a checkout at all.
    std::fs::create_dir_all(dir.join("plain")).unwrap();
    // Active name: skipped even though stale.
    git_repo(&dir.join("current"));
    let active: HashSet<String> = ["current".to_string()].into_iter().collect();
    let stale = find_stale_worktrees(&dir, &active);
    assert_eq!(stale, vec![dir.join("old-done")]);
    let _ = std::fs::remove_dir_all(&dir);
}
