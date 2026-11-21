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
    CheckpointInput, CheckpointJudge, CheckpointVerdict, HeuristicCheckpointJudge, precheck,
    record_outcome,
};
use agentdeck_coordinator::config::{CoordinatorConfig, CoordinatorPoolEntry};
use agentdeck_coordinator::hygiene::{check_disk_ok, find_stale_worktrees, free_disk_gb};
use agentdeck_coordinator::pool::{
    PoolKey, PoolSelection, QuotaWindowView, parse_epoch_rfc3339, pool_key,
    quota_from_t3_cache, select_coordinator, should_proactively_rotate,
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

fn quotas(pairs: &[(&str, QuotaWindowView)]) -> HashMap<PoolKey, QuotaWindowView> {
    pairs
        .iter()
        .map(|(id, q)| {
            // Tests use the fixed pool2() models, so the instance id alone
            // identifies the (instance, model) key.
            let model = pool2()
                .into_iter()
                .find(|e| &e.instance_id == id)
                .map(|e| e.model)
                .unwrap_or_else(|| "m".to_string());
            (pool_key(id, &model), q.clone())
        })
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
fn pool_all_exhausted_sleeps_until_earliest_usable_reset_plus_120s() {
    let cfg = cfg_with_pool();
    let mut a = QuotaWindowView::exhausted(Some(NOW + 3000), Some(NOW + 9000));
    a.healthy = true;
    let mut b = QuotaWindowView::exhausted(Some(NOW + 1000), Some(NOW + 8000));
    b.healthy = true;
    let q = quotas(&[("claude_a", a), ("codex_b", b)]);
    match select_coordinator(&cfg, &q, NOW, None) {
        // Each candidate is blocked by BOTH windows, so its usable reset is
        // the later of the two (max): 9000 for claude_a, 8000 for codex_b.
        PoolSelection::SleepForQuota {
            wake_at_epoch_secs,
            earliest_reset_epoch_secs,
            next_candidate,
            ..
        } => {
            assert_eq!(earliest_reset_epoch_secs, Some(NOW + 8000));
            assert_eq!(wake_at_epoch_secs, NOW + 8000 + 120);
            assert_eq!(next_candidate.unwrap().instance_id, "codex_b");
        }
        other => panic!("expected SleepForQuota, got {other:?}"),
    }
}

#[test]
fn pool_sleep_ignores_stale_resets_and_unknown_is_never_now() {
    let cfg = cfg_with_pool();
    // claude_a's reset is ten minutes in the past: stale, never usable.
    let mut a = QuotaWindowView::exhausted(Some(NOW - 600), None);
    a.weekly_remaining_pct = Some(60.0);
    a.healthy = true;
    // codex_b is blocked with no known reset at all: unknown is never now.
    let mut b = QuotaWindowView::exhausted(None, None);
    b.weekly_remaining_pct = Some(60.0);
    b.healthy = true;
    let q = quotas(&[("claude_a", a), ("codex_b", b)]);
    match select_coordinator(&cfg, &q, NOW, None) {
        PoolSelection::SleepForQuota {
            wake_at_epoch_secs,
            earliest_reset_epoch_secs,
            next_candidate,
            ..
        } => {
            assert_eq!(earliest_reset_epoch_secs, None);
            assert_eq!(wake_at_epoch_secs, NOW + 300);
            assert!(next_candidate.is_none());
        }
        other => panic!("expected bounded-refresh SleepForQuota, got {other:?}"),
    }
}

#[test]
fn pool_sleep_uses_max_of_both_blocking_windows() {
    let cfg = cfg_with_pool();
    // Blocked by 5h (reset +1000) and weekly (reset +5000): unusable until +5000.
    let mut a = QuotaWindowView::exhausted(Some(NOW + 1000), Some(NOW + 5000));
    a.healthy = true;
    // Blocked by 5h only (reset +2000), weekly healthy: usable at +2000.
    let mut b = QuotaWindowView::exhausted(Some(NOW + 2000), None);
    b.weekly_remaining_pct = Some(60.0);
    b.healthy = true;
    let q = quotas(&[("claude_a", a), ("codex_b", b)]);
    match select_coordinator(&cfg, &q, NOW, None) {
        PoolSelection::SleepForQuota {
            wake_at_epoch_secs,
            earliest_reset_epoch_secs,
            next_candidate,
            ..
        } => {
            assert_eq!(earliest_reset_epoch_secs, Some(NOW + 2000));
            assert_eq!(wake_at_epoch_secs, NOW + 2000 + 120);
            assert_eq!(next_candidate.unwrap().instance_id, "codex_b");
        }
        other => panic!("expected SleepForQuota, got {other:?}"),
    }
}

#[test]
fn pool_hysteresis_keeps_current_within_2pct() {
    let cfg = cfg_with_pool();
    let q = quotas(&[("claude_a", view(73.5, 80.0)), ("codex_b", view(75.0, 80.0))]);
    match select_coordinator(&cfg, &q, NOW, Some(("claude_a", "claude-opus-4-6"))) {
        PoolSelection::Selected { entry, reason, .. } => {
            assert_eq!(entry.instance_id, "claude_a");
            assert!(reason.contains("prompt cache"), "{reason}");
        }
        other => panic!("expected hysteresis keep, got {other:?}"),
    }
    // Beyond 2% the best entry wins.
    let q = quotas(&[("claude_a", view(70.0, 80.0)), ("codex_b", view(75.0, 80.0))]);
    match select_coordinator(&cfg, &q, NOW, Some(("claude_a", "claude-opus-4-6"))) {
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
fn pool_excludes_entries_without_a_quota_view() {
    let cfg = cfg_with_pool();
    let q = quotas(&[("codex_b", view(80.0, 80.0))]);
    let selection = select_coordinator(&cfg, &q, NOW, Some(("claude_a", "claude-opus-4-6")));
    assert!(
        matches!(selection, PoolSelection::Selected { entry, .. } if entry.instance_id == "codex_b")
    );
    assert!(matches!(
        select_coordinator(&cfg, &HashMap::new(), NOW, None),
        PoolSelection::SleepForQuota {
            next_candidate: None,
            ..
        }
    ));
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
fn proactive_rotate_enforces_weekly_reserve_on_both_sides() {
    let cfg = cfg_with_pool();
    assert!(should_proactively_rotate(
        &cfg,
        &view(90.0, 5.0),
        &view(85.0, 80.0)
    ));
    assert!(!should_proactively_rotate(
        &cfg,
        &view(5.0, 80.0),
        &view(90.0, 5.0)
    ));
}

#[test]
fn quota_sleep_deadline_cannot_overflow_or_wrap() {
    let mut cfg = cfg_with_pool();
    let max = parse_epoch_rfc3339("9999-12-31T23:59:59Z").unwrap();
    for (buffer, reset) in [(u64::MAX, NOW + 600), (120, i64::MAX - 1), (120, max - 1)] {
        cfg.quota_sleep_buffer_secs = buffer;
        let mut blocked = view(0.0, 80.0);
        blocked.five_hour_resets_at = Some(reset);
        let q = quotas(&[("codex_b", blocked), ("claude_a", view(0.0, 80.0))]);
        assert!(matches!(
            select_coordinator(&cfg, &q, NOW, None),
            PoolSelection::SleepForQuota {
                wake_at_epoch_secs, next_candidate: None, ..
            } if wake_at_epoch_secs == NOW + 300
        ));
    }
    assert!(matches!(
        select_coordinator(&cfg, &HashMap::new(), max - 1, None),
        PoolSelection::SleepForQuota {
            wake_at_epoch_secs, ..
        } if wake_at_epoch_secs == max
    ));
}

#[test]
fn rfc3339_parses_t3_shapes() {
    assert_eq!(parse_epoch_rfc3339("2026-11-13T11:00:00Z"), Some(1_794_567_600));
    assert_eq!(parse_epoch_rfc3339("2026-11-13T10:00:00.000Z"), Some(1_794_564_000));
    assert_eq!(
        parse_epoch_rfc3339("2026-11-13T11:00:00+00:00"),
        Some(1_794_567_600)
    );
    assert_eq!(
        parse_epoch_rfc3339("2026-11-13T10:00:00-0100"),
        Some(1_794_567_600)
    );
    assert!(parse_epoch_rfc3339("not a date").is_none());
    assert!(parse_epoch_rfc3339("2026-13-01T00:00:00Z").is_none());
}

#[test]
fn rfc3339_checked_parser_never_panics() {
    // Minutes without seconds: top of the minute, never an index panic.
    let full = parse_epoch_rfc3339("2026-11-21T04:00:00Z").unwrap();
    assert_eq!(parse_epoch_rfc3339("2026-11-21T04:00Z"), Some(full));
    for bad in [
        "garbage!@#",
        "",
        "2026-11-21T04:00",
        "2026-11-21T25:00:00Z",
        "2026-11-21T04:60:00Z",
        "2026-11-21",
        "2026-11-21T04:00:00",
        "2026-11-21T04:00:00+99:99",
        "2026-11-21T04:00:00+€a",
        "2026-11-21T04:00:00+::00::00",
        "2026-11-21T04:00:00:99Z",
        "2026-11-21T04:00:00.badZ",
        "999999999999999999-01-01T00:00:00Z",
        "not a date",
        "2026-13-01T00:00:00Z",
        "2026-00-10T00:00:00Z",
        "2026-02-30T00:00:00Z",
    ] {
        assert!(parse_epoch_rfc3339(bad).is_none(), "{bad}");
    }
}

// --- checkpoint golden eval (>= 20 scenarios) --------------------------------

#[test]
fn checkpoint_precheck_refuses_active_children_even_at_a_milestone() {
    let verdict = precheck(&input(90, 10, true, false, 1, &[]), &cfg_with_pool())
        .expect("active children must be refused before consulting a judge");
    assert!(!verdict.should_checkpoint);
    assert!(verdict.reason.contains("child agents"));
}

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
    o.model = Some("claude-opus-4-6".into());
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
    o.model = Some("claude-opus-4-6".into());
    o.last_message_text = "You've hit your 5-hour limit".into();
    o.hit_quota_limit = true;
    let mut inp = tick_input(r, o);
    // Weekly stays healthy: only the 5h window blocks, so the sleep deadline
    // is the 5h reset + 120s. (0% weekly with no weekly reset would leave no
    // usable future reset at all.)
    let mut clogged = QuotaWindowView::exhausted(Some(NOW + 5000), None);
    clogged.weekly_remaining_pct = Some(60.0);
    let mut spare = QuotaWindowView::exhausted(Some(NOW + 2000), None);
    spare.weekly_remaining_pct = Some(60.0);
    inp.quotas = quotas(&[("claude_a", clogged), ("codex_b", spare)]);
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
    o.model = Some("claude-opus-4-6".into());
    let mut inp = tick_input(run("t5"), o);
    inp.quotas = quotas(&[("claude_a", view(5.0, 80.0)), ("codex_b", view(60.0, 80.0))]);
    match decide(&cfg, &inp) {
        WatchdogAction::RotateAndWake { to_entry, .. } => {
            assert_eq!(to_entry.instance_id, "codex_b");
        }
        other => panic!("expected proactive RotateAndWake, got {other:?}"),
    }
}

#[test]
fn reactive_rotation_never_restarts_stale_failed_account() {
    // The T3 cache is stale: the failed account still shows 95% 5h while the
    // spare shows 80%. The observed failure must win over the cache.
    let cfg = cfg_with_pool();
    let mut r = run("t12");
    r.phase = RunPhase::Running;
    let mut o = observed("t12");
    o.provider_instance_id = Some("claude_a".into());
    o.model = Some("claude-opus-4-6".into());
    o.hit_quota_limit = true;
    let mut inp = tick_input(r, o);
    inp.quotas = quotas(&[("claude_a", view(95.0, 80.0)), ("codex_b", view(80.0, 60.0))]);
    match decide(&cfg, &inp) {
        WatchdogAction::RotateAndWake { to_entry, reason, .. } => {
            assert_eq!(to_entry.instance_id, "codex_b", "{reason}");
            assert!(reason.contains("quota limit"), "{reason}");
        }
        other => panic!("expected RotateAndWake away from the failed account, got {other:?}"),
    }
}

#[test]
fn reactive_rotation_skips_uncached_failed_instance_with_unknown_model() {
    let mut cfg = cfg_with_pool();
    cfg.pool.insert(
        1,
        CoordinatorPoolEntry::new("claude_a", "claude-sonnet-4-6"),
    );
    let mut o = observed("t12");
    o.provider_instance_id = Some("claude_a".into());
    o.hit_quota_limit = true;
    let mut inp = tick_input(run("t12"), o);
    inp.quotas = quotas(&[("codex_b", view(80.0, 80.0))]);
    assert!(matches!(
        decide(&cfg, &inp),
        WatchdogAction::RotateAndWake { to_entry, .. } if to_entry.instance_id == "codex_b"
    ));
}

#[test]
fn sleeping_run_with_invalid_or_missing_deadline_stays_quiet() {
    let cfg = cfg_with_pool();
    for persisted in [None, Some("2026-11-21T04:00:00+€a"), Some("invalid")] {
        let mut run = run("sleeping");
        run.phase = RunPhase::SleepingForQuota;
        run.sleep_until_epoch = persisted.and_then(parse_epoch_rfc3339);
        let mut inp = tick_input(run, observed("sleeping"));
        inp.quotas = quotas(&[("codex_b", view(80.0, 80.0))]);
        assert!(
            matches!(decide(&cfg, &inp), WatchdogAction::Noop { .. }),
            "{persisted:?}"
        );
    }
}

#[test]
fn proactive_rotation_skips_ineligible_alternative() {
    // codex_b has the fullest 5h window but 0% weekly: ineligible, so it can
    // neither win nor hide. With no other entry the thin account wakes plainly.
    let cfg = cfg_with_pool();
    let mut o = observed("t14");
    o.provider_instance_id = Some("claude_a".into());
    o.model = Some("claude-opus-4-6".into());
    let mut inp = tick_input(run("t14"), o);
    inp.quotas = quotas(&[("claude_a", view(5.0, 80.0)), ("codex_b", view(90.0, 0.0))]);
    assert!(matches!(decide(&cfg, &inp), WatchdogAction::Wake { .. }));

    // Three entries: the 5h-fullest is unhealthy, the runner-up is eligible.
    // The unhealthy leader must not hide the healthy runner-up.
    let cfg3 = CoordinatorConfig {
        pool: vec![
            CoordinatorPoolEntry::new("thin", "m-thin"),
            CoordinatorPoolEntry::new("sick", "m-sick"),
            CoordinatorPoolEntry::new("spare", "m-spare"),
        ],
        ..Default::default()
    };
    let thin = view(5.0, 80.0);
    let mut sick = view(95.0, 80.0);
    sick.healthy = false;
    let spare = view(60.0, 80.0);
    let q: HashMap<PoolKey, QuotaWindowView> = [
        (pool_key("thin", "m-thin"), thin),
        (pool_key("sick", "m-sick"), sick),
        (pool_key("spare", "m-spare"), spare),
    ]
    .into_iter()
    .collect();
    let mut o = observed("t14b");
    o.provider_instance_id = Some("thin".into());
    o.model = Some("m-thin".into());
    let mut inp = tick_input(run("t14b"), o);
    inp.quotas = q;
    match decide(&cfg3, &inp) {
        WatchdogAction::RotateAndWake { to_entry, .. } => {
            assert_eq!(to_entry.instance_id, "spare");
        }
        other => panic!("expected rotation to the healthy runner-up, got {other:?}"),
    }
}

#[test]
fn sleeping_run_with_no_work_or_active_run_stays_quiet() {
    let cfg = cfg_with_pool();
    let q = quotas(&[("claude_a", view(60.0, 60.0)), ("codex_b", view(70.0, 60.0))]);
    // Past the deadline with an eligible pool, but the work completed while
    // sleeping: Noop, never a wake.
    let r = RunSnapshot {
        thread_id: "t15".into(),
        phase: RunPhase::SleepingForQuota,
        sleep_until_epoch: Some(NOW - 1),
        has_unblocked_work: false,
    };
    let mut inp = tick_input(r, observed("t15"));
    inp.quotas = q.clone();
    assert!(matches!(decide(&cfg, &inp), WatchdogAction::Noop { .. }));
    // Past the deadline but the thread already resumed: Noop, never a wake.
    let r = RunSnapshot {
        thread_id: "t15".into(),
        phase: RunPhase::SleepingForQuota,
        sleep_until_epoch: Some(NOW - 1),
        has_unblocked_work: true,
    };
    let mut o = observed("t15");
    o.is_running = true;
    let mut inp = tick_input(r, o);
    inp.quotas = q;
    assert!(matches!(decide(&cfg, &inp), WatchdogAction::Noop { .. }));
}

#[test]
fn two_pool_models_on_one_instance_keep_separate_quota_views() {
    // Two entries share one Antigravity instance id with different models: the
    // Claude entry must see the 3p bucket, the Gemini entry the Gemini one.
    let cfg = CoordinatorConfig {
        pool: vec![
            CoordinatorPoolEntry::new("agy", "claude-sonnet-4-6"),
            CoordinatorPoolEntry::new("agy", "gemini-3-flash"),
        ],
        ..Default::default()
    };
    let claude_view = view(95.0, 80.0);
    let gemini_view = view(10.0, 80.0);
    let q: HashMap<PoolKey, QuotaWindowView> = [
        (pool_key("agy", "claude-sonnet-4-6"), claude_view),
        (pool_key("agy", "gemini-3-flash"), gemini_view),
    ]
    .into_iter()
    .collect();
    match select_coordinator(&cfg, &q, NOW, None) {
        PoolSelection::Selected { entry, .. } => {
            assert_eq!(entry.model, "claude-sonnet-4-6");
        }
        other => panic!("expected the healthy Claude entry, got {other:?}"),
    }
    // ... and marking the Claude entry ineligible leaves the Gemini entry.
    let mut claude_bad = view(95.0, 80.0);
    claude_bad.healthy = false;
    let q: HashMap<PoolKey, QuotaWindowView> = [
        (pool_key("agy", "claude-sonnet-4-6"), claude_bad),
        (pool_key("agy", "gemini-3-flash"), view(10.0, 80.0)),
    ]
    .into_iter()
    .collect();
    match select_coordinator(&cfg, &q, NOW, None) {
        PoolSelection::Selected { entry, .. } => {
            assert_eq!(entry.model, "gemini-3-flash");
        }
        other => panic!("expected the Gemini entry, got {other:?}"),
    }
}

// --- executor + debounce -----------------------------------------------------

#[derive(Debug, Default)]
struct FakeController {
    configured: Mutex<Vec<(String, String, String)>>,
    messaged: Mutex<Vec<(String, String, bool)>>,
    fail_send: std::cell::Cell<bool>,
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
        anyhow::ensure!(!self.fail_send.replace(false), "transient T3 failure");
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

#[tokio::test]
async fn executor_retries_failed_rpc_before_starting_debounce() {
    let c = FakeController::default();
    let mut debounce = ActionDebounce::new(300);
    let actions = [
        WatchdogAction::Wake {
            thread_id: "t".into(),
            fresh_context: false,
            message: "hi".into(),
            reason: "test".into(),
        },
        WatchdogAction::RotateAndWake {
            thread_id: "t".into(),
            to_entry: pool2().remove(0),
            fresh_context: false,
            message: "hi".into(),
            reason: "test".into(),
        },
        WatchdogAction::TriggerCheckpoint {
            thread_id: "t".into(),
            target: Some(pool2().remove(0)),
            reason: "test".into(),
        },
    ];
    for action in actions {
        c.fail_send.set(true);
        assert!(
            execute_action(&c, &action, &mut debounce, NOW)
                .await
                .is_err()
        );
        assert!(
            execute_action(&c, &action, &mut debounce, NOW + 1)
                .await
                .unwrap()
        );
        assert!(
            !execute_action(&c, &action, &mut debounce, NOW + 2)
                .await
                .unwrap()
        );
    }
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

fn write_t3_instances(t3: &Path, instances: &[&str]) {
    let providers: serde_json::Map<String, serde_json::Value> = instances
        .iter()
        .map(|id| (id.to_string(), serde_json::json!({"enabled": true})))
        .collect();
    std::fs::create_dir_all(t3.join("userdata")).unwrap();
    std::fs::write(
        t3.join("userdata/settings.json"),
        serde_json::json!({"providerInstances": providers}).to_string(),
    )
    .unwrap();
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
    write_t3_instances(&dir, &["claude_a"]);
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
    write_t3_instances(&dir, &["agy"]);
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
fn quota_view_distinguishes_known_uncached_instances_from_deleted_ones() {
    let dir = tmpdir("missing");
    write_t3_instances(&dir, &["known", "bad"]);
    write_cache(&dir, "deleted", &serde_json::json!({"status": "ready"}));
    let v = quota_from_t3_cache(&dir, "known", "m");
    assert!(v.healthy);
    assert_eq!(v.five_hour_remaining_pct, None);
    assert!(!quota_from_t3_cache(&dir, "deleted", "m").healthy);
    assert!(!quota_from_t3_cache(&dir, "typo", "m").healthy);
    write_cache(&dir, "bad", &serde_json::json!({"status": "error", "message": "auth failed"}));
    let v = quota_from_t3_cache(&dir, "bad", "m");
    assert!(!v.healthy);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn quota_view_only_matches_openai_windows_to_openai_models() {
    let dir = tmpdir("mixed");
    write_t3_instances(&dir, &["mixed"]);
    write_cache(
        &dir,
        "mixed",
        &serde_json::json!({
            "status": "ready",
            "usageLimits": {"windows": [
                {"id": "openai-week", "windowDurationMins": 10080, "usedPercent": 99.0,
                 "resetsAt": "2026-11-21T04:00:00+€a"},
                {"id": "claude-week", "windowDurationMins": 10080, "usedPercent": 20.0}
            ]}
        }),
    );
    let claude = quota_from_t3_cache(&dir, "mixed", "claude-sonnet-4-6");
    assert_eq!(claude.weekly_remaining_pct, Some(80.0));
    let codex = quota_from_t3_cache(&dir, "mixed", "codex-mini");
    assert_eq!(codex.weekly_remaining_pct, Some(1.0));
    assert_eq!(codex.weekly_resets_at, None);
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
    assert!(!o.hit_quota_limit, "a running turn is not a quota failure");
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

#[test]
fn observer_picks_newest_run_limit_recovery_and_attempt_reason() {
    // Real projection schema, two runs on different accounts, inserted
    // out of order: ordinal 2 (failed, on codex_b) lands before ordinal 1
    // (completed, on claude_a). The newest run must win; the thread-level
    // failed attempt's `reason` identifies the quota failure.
    use rusqlite::Connection;
    let dir = tmpdir("sqlite-newest");
    std::fs::create_dir_all(dir.join("userdata")).unwrap();
    let db = Connection::open(dir.join("userdata/statev2.sqlite")).unwrap();
    db.execute_batch(
        "CREATE TABLE orchestration_v2_projection_threads (
           thread_id TEXT PRIMARY KEY, title TEXT, updated_at TEXT,
           payload_json TEXT, provider_instance_id TEXT);
         CREATE TABLE orchestration_v2_projection_runs (
           run_id TEXT PRIMARY KEY, thread_id TEXT, ordinal INTEGER, status TEXT,
           requested_at TEXT, completed_at TEXT, payload_json TEXT,
           provider_instance_id TEXT);
         CREATE TABLE orchestration_v2_projection_turn_items (
           turn_item_id TEXT PRIMARY KEY, thread_id TEXT, run_id TEXT,
           ordinal INTEGER, type TEXT, status TEXT, payload_json TEXT);",
    )
    .unwrap();
    db.execute(
        "INSERT INTO orchestration_v2_projection_threads VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            "t-n",
            "Coord",
            "2026-11-21T02:00:00Z",
            r#"{"modelSelection":{"instanceId":"codex_b","model":"gpt-6.2-sol"}}"#,
            "codex_b",
        ],
    )
    .unwrap();
    // Newest run inserted FIRST so rowid order disagrees with ordinal order.
    db.execute(
        "INSERT INTO orchestration_v2_projection_runs VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        rusqlite::params![
            "r2",
            "t-n",
            2,
            "failed",
            "2026-11-21T02:00:00Z",
            "2026-11-21T02:05:00Z",
            r#"{"modelSelection":{"instanceId":"codex_b","model":"gpt-6.2-sol"}}"#,
            "codex_b",
        ],
    )
    .unwrap();
    db.execute(
        "INSERT INTO orchestration_v2_projection_runs VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        rusqlite::params![
            "r1",
            "t-n",
            1,
            "completed",
            "2026-11-21T01:00:00Z",
            "2026-11-21T01:10:00Z",
            r#"{"modelSelection":{"instanceId":"claude_a","model":"claude-opus-4-6"},"lastMessage":"all done"}"#,
            "claude_a",
        ],
    )
    .unwrap();
    // No failure.class: only the plain `reason` carries the quota signal.
    db.execute(
        "INSERT INTO orchestration_v2_projection_turn_items VALUES ('ti1','t-n','r2',1,'error','failed',?1)",
        [r#"{"title":"Run failed","reason":"API rate limit exceeded, try again at 3pm"}"#],
    )
    .unwrap();
    let o = observe_thread_sqlite(&dir, "t-n").unwrap().expect("thread t-n");
    assert_eq!(o.provider_instance_id.as_deref(), Some("codex_b"));
    assert_eq!(o.model.as_deref(), Some("gpt-6.2-sol"));
    assert_eq!(o.run_count, 2);
    assert!(!o.is_running);
    assert!(o.hit_quota_limit, "tail: {}", o.last_message_text);
    // The attempt's title is the tail and its reason identifies the quota failure.
    assert!(o.last_message_text.contains("Run failed"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn observer_only_reports_quota_from_the_latest_failed_turn() {
    let dir = tmpdir("quota-status");
    fixture_db(&dir);
    let db = rusqlite::Connection::open(dir.join("userdata/statev2.sqlite")).unwrap();
    db.execute_batch(
        "CREATE TABLE orchestration_v2_projection_turn_items
           (thread_id TEXT, run_id TEXT, type TEXT, status TEXT, payload_json TEXT);
         CREATE TABLE orchestration_v2_projection_messages
           (message_id TEXT, thread_id TEXT, role TEXT, updated_at TEXT, payload_json TEXT);",
    )
    .unwrap();
    for (status, recovery_run, assistant_text, error_item, expected) in [
        ("completed", Some("older-run"), None, false, false),
        ("completed", Some("r1"), None, false, false),
        (
            "completed",
            None,
            Some("The rate limit resets at 14:00."),
            false,
            false,
        ),
        ("completed", None, None, true, false),
        ("failed", Some("older-run"), None, false, false),
        ("failed", Some("r1"), None, false, true),
        (
            "interrupted",
            None,
            Some("The rate limit resets at 14:00."),
            false,
            true,
        ),
        ("running", Some("r1"), None, false, false),
    ] {
        let payload = serde_json::json!({"limitRecovery": recovery_run.map(|id| serde_json::json!({"runId": id}))});
        db.execute(
            "UPDATE orchestration_v2_projection_threads SET payload_json=?1",
            [payload.to_string()],
        )
        .unwrap();
        db.execute(
            "UPDATE orchestration_v2_projection_runs SET status=?1, payload_json='{}'",
            [status],
        )
        .unwrap();
        db.execute_batch("DELETE FROM orchestration_v2_projection_messages; DELETE FROM orchestration_v2_projection_turn_items;").unwrap();
        if let Some(assistant_text) = assistant_text {
            db.execute(
                "INSERT INTO orchestration_v2_projection_messages VALUES ('msg', 't1', 'assistant', '1', ?1)",
                [serde_json::json!({"text": assistant_text}).to_string()],
            )
            .unwrap();
        }
        if error_item {
            db.execute(
                "INSERT INTO orchestration_v2_projection_turn_items VALUES ('t1', 'r1', 'error', 'failed', ?1)",
                [r#"{"failure":{"class":"usage_limit"}}"#],
            )
            .unwrap();
        }
        let o = observe_thread_sqlite(&dir, "t1").unwrap().unwrap();
        assert_eq!(
            o.hit_quota_limit, expected,
            "{status}, {recovery_run:?}, {assistant_text:?}, {error_item}"
        );
    }
    drop(db);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn observer_can_read_legacy_run_projection_without_status() {
    let dir = tmpdir("legacy-status");
    fixture_db(&dir);
    let db = rusqlite::Connection::open(dir.join("userdata/statev2.sqlite")).unwrap();
    db.execute_batch("ALTER TABLE orchestration_v2_projection_runs DROP COLUMN status")
        .unwrap();
    let o = observe_thread_sqlite(&dir, "t1").unwrap().unwrap();
    assert_eq!(o.provider_instance_id.as_deref(), Some("claude_a"));
    assert!(o.hit_quota_limit);
    drop(db);
    std::fs::remove_dir_all(dir).unwrap();
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
        // A loopback-looking suffix is a different host.
        "http://127.0.0.1.attacker.com/mcp",
        "http://localhost.attacker.com/mcp",
        // IPv4-mapped IPv6 is not one of the three accepted literals.
        "http://[::ffff:127.0.0.1]:3773/mcp",
        // Credentials hidden in the userinfo part parse as host under a
        // hand-rolled split; the real URL parser must still refuse them.
        "http://127.0.0.1%09@evil.com/mcp",
        "http://:pass@127.0.0.1:3773/mcp",
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
fn serve_json_rpc(
    n_requests: usize,
    hold_sse_open: Option<std::sync::mpsc::Receiver<()>>,
) -> (std::net::SocketAddr, Arc<Seen>, std::thread::JoinHandle<()>) {
    let seen = Arc::new(Seen {
        bodies: Mutex::new(Vec::new()),
        auths: Mutex::new(Vec::new()),
    });
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen_clone = seen.clone();
    let handle = std::thread::spawn(move || {
        listener.set_nonblocking(false).ok();
        for i in 0..n_requests {
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
            if i + 1 == n_requests
                && let Some(release) = hold_sse_open
            {
                stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n").unwrap();
                let events = format!(
                    "data: {{\"id\":99,\"result\":{{}}}}\n\ndata: {{\"jsonrpc\":\"2.0\",\"id\":{id},\r\ndata: \"result\":{{\"text\":\"✓\"}}}}\r\n\n"
                );
                let split = events.find('✓').unwrap() + 1;
                for chunk in [&events.as_bytes()[..split], &events.as_bytes()[split..]] {
                    write!(stream, "{:x}\r\n", chunk.len()).unwrap();
                    stream.write_all(chunk).unwrap();
                    stream.write_all(b"\r\n").unwrap();
                }
                let _ = release.recv_timeout(std::time::Duration::from_secs(5));
                break;
            }
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

struct HandshakeSeen {
    methods: Mutex<Vec<String>>,
    sessions: Mutex<Vec<Option<String>>>,
    tool_calls: Mutex<Vec<serde_json::Value>>,
}

/// Mock Streamable HTTP server: answers `initialize` with an
/// `Mcp-Session-Id` response header and records the session header and tool
/// arguments of every request.
fn serve_handshake() -> (std::net::SocketAddr, Arc<HandshakeSeen>, std::thread::JoinHandle<()>) {
    let seen = Arc::new(HandshakeSeen {
        methods: Mutex::new(Vec::new()),
        sessions: Mutex::new(Vec::new()),
        tool_calls: Mutex::new(Vec::new()),
    });
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen_clone = seen.clone();
    let handle = std::thread::spawn(move || {
        for _ in 0..3 {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).ok();
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
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
            let session = text.lines().find_map(|l| {
                let (k, v) = l.split_once(':')?;
                (k.trim().eq_ignore_ascii_case("mcp-session-id")).then(|| v.trim().to_string())
            });
            let req: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("").to_string();
            if method == "tools/call" {
                seen_clone.tool_calls.lock().unwrap().push(req.clone());
            }
            seen_clone.methods.lock().unwrap().push(method);
            seen_clone.sessions.lock().unwrap().push(session);
            // The notification carries no id.
            let id = req.get("id").cloned().unwrap_or(serde_json::Value::Null);
            let res = format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{}}}}");
            let out = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nmcp-session-id: sess-1\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
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
async fn http_controller_performs_session_handshake() {
    let (addr, seen, handle) = serve_handshake();
    let endpoint = format!("http://127.0.0.1:{}/mcp", addr.port());
    let c = HttpT3Controller::new(&endpoint, None).unwrap();
    let mut options = HashMap::new();
    options.insert("temperature".to_string(), serde_json::json!(0));
    c.configure_thread("t1", "inst-a", "model-x", &options).await.unwrap();
    handle.join().unwrap();
    // Full handshake: initialize, then notifications/initialized, then the call.
    assert_eq!(
        *seen.methods.lock().unwrap(),
        vec!["initialize", "notifications/initialized", "tools/call"],
    );
    // The initialize response's session rides on every later request.
    assert_eq!(
        *seen.sessions.lock().unwrap(),
        vec![None, Some("sess-1".to_string()), Some("sess-1".to_string())],
    );
    // The configure call nests instanceId/model/options under modelSelection.
    let calls = seen.tool_calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    let args = &calls[0]["params"]["arguments"];
    assert_eq!(args["threadId"], "t1");
    assert_eq!(args["modelSelection"]["instanceId"], "inst-a");
    assert_eq!(args["modelSelection"]["model"], "model-x");
    assert_eq!(args["modelSelection"]["options"]["temperature"], 0);
    assert_eq!(calls[0]["params"]["name"], "t3_thread_configure");
}

#[tokio::test]
async fn http_controller_calls_json_rpc_over_loopback() {
    let (addr, seen, handle) = serve_json_rpc(6, None);
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
    // initialize + notifications/initialized + tools/call per operation.
    assert_eq!(bodies.len(), 6);
    let inits = bodies
        .iter()
        .filter(|b| b.get("method").and_then(|m| m.as_str()) == Some("initialize"))
        .count();
    assert_eq!(inits, 2);
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

#[tokio::test]
async fn http_controller_returns_on_matching_sse_event_without_eof() {
    let (release, hold) = std::sync::mpsc::channel();
    let (addr, _, handle) = serve_json_rpc(3, Some(hold));
    let c = HttpT3Controller::new(&format!("http://{addr}/mcp"), None).unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        c.send_message("t1", "hello", false),
    )
    .await;
    let _ = release.send(());
    assert!(result.is_ok(), "the server held the SSE stream open");
    result.unwrap().unwrap();
    handle.join().unwrap();
}

#[tokio::test]
async fn http_controller_bypasses_env_proxy() {
    const CHILD: &str = "AGENTDECK_COORDINATOR_PROXY_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let proxy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = proxy.local_addr().unwrap();
        drop(proxy);
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "http_controller_bypasses_env_proxy"])
            .env(CHILD, "1")
            .env("HTTP_PROXY", format!("http://{addr}"))
            .env_remove("http_proxy")
            .env_remove("ALL_PROXY")
            .env_remove("all_proxy")
            .env_remove("NO_PROXY")
            .env_remove("no_proxy")
            .env_remove("REQUEST_METHOD")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        return;
    }
    let (addr, seen, handle) = serve_json_rpc(3, None);
    let endpoint = format!("http://localhost:{}/mcp", addr.port());
    let c = HttpT3Controller::new(&endpoint, None).unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        c.send_message("t1", "hello", false),
    )
    .await
    .unwrap()
    .unwrap();
    handle.join().unwrap();
    assert_eq!(seen.bodies.lock().unwrap().len(), 3);
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

#[test]
fn stale_worktree_discovery_ignores_plain_subdirectory_of_parent_repo() {
    if std::process::Command::new("git").arg("--version").output().is_err() {
        return; // no git on PATH: nothing to discover with.
    }
    // The scanned dir IS a clean merged checkout: its own plain subdir walks
    // up to the parent repo for every git probe but is not a worktree root.
    let dir = tmpdir("wt-sub");
    git_repo(&dir);
    std::fs::create_dir_all(dir.join("inner")).unwrap();
    std::fs::write(dir.join("inner/notes.txt"), "work").unwrap();
    std::process::Command::new("git").arg("-C").arg(&dir).args(["add", "."]).output().unwrap();
    std::process::Command::new("git").arg("-C").arg(&dir).args(["commit", "-m", "sub"]).output().unwrap();
    std::process::Command::new("git")
        .arg("-C").arg(&dir)
        .args(["update-ref", "refs/remotes/origin/main", "HEAD"])
        .output().unwrap();
    // Only the merged checkouts directly under `dir` are candidates; `inner`
    // is not its own checkout even though git probes succeed inside it.
    let stale = find_stale_worktrees(&dir, &HashSet::new());
    assert!(!stale.contains(&dir.join("inner")), "{stale:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
