//! Coordinator page: a thread's goal and checklist plus the open backlog,
//! rendered headless from state written through the core API (EN/RU).

use agentdeck_core::Paths;
use agentdeck_core::backlog::{self, NewEntry};
use agentdeck_core::settings::{
    self, ApproverSettings, CoordinatorPoolEntry, CoordinatorSettings,
};
use agentdeck_core::thread_state::{
    self, ItemPatch, NewAssumption, NewInboxItem, RunMandate, RunPhase,
};
use agentdeck_gui::app::{self, Message, Page};
use agentdeck_gui::pages::settings as gsettings;

const SNAPSHOTS: &str = "tests/snapshots";

/// `AGENTDECK_T3_DIR` and `HOME` are process-global: tests that point them at
/// fixtures hold this lock for their whole body so parallel tests cannot race.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

// Goldens depend on this machine's fonts and GPU rasterizer, so CI only checks that rendering works.
fn assert_snapshot(snapshot: &iced_test::simulator::Snapshot, stem: &str) {
    if std::env::var_os("CI").is_none() {
        assert!(
            snapshot
                .matches_image(format!("{SNAPSHOTS}/{stem}.png"))
                .expect("snapshot compare/write"),
            "snapshot {stem} changed"
        );
    }
}

fn item(id: &str, title: &str, status: &str, note: &str, links: &[&str]) -> ItemPatch {
    ItemPatch {
        id: id.into(),
        title: Some(title.into()),
        status: Some(status.into()),
        note: (!note.is_empty()).then(|| note.into()),
        links: Some(links.iter().map(|l| l.to_string()).collect()),
    }
}

#[test]
fn coordinator_page_shows_checklist_and_backlog() {
    let _env = lock_env();
    let dir = std::env::temp_dir().join(format!("agentdeck-gui-coord-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("home")).unwrap();
    std::fs::create_dir_all(dir.join("config")).unwrap();
    unsafe {
        std::env::set_var("HOME", dir.join("home"));
        std::env::set_var("AGENTDECK_T3_DIR", dir.join("t3"));
        std::env::set_var("AGENTDECK_PROC_DIR", dir.join("proc"));
        std::env::set_var("XDG_STATE_HOME", dir.join("xdg"));
    }
    std::fs::write(
        dir.join("config/instances.toml"),
        "version = 1\n\n[plugins]\ncoordinator = true\n",
    )
    .unwrap();
    agentdeck_gui::i18n::set_language("en");
    let paths = Paths::new(dir.join("config"), dir.join("state"));
    thread_state::write(
        &paths,
        None,
        "thread-1",
        Some("Ship the #71 scope: merged, released, deployed, verified"),
        None,
    )
    .unwrap();
    thread_state::update_checklist(
        &paths,
        None,
        "thread-1",
        Some("blockfield"),
        &[
            item("cp", "CP v2 cutover", "done", "", &["control-plane#88"]),
            item(
                "62",
                "Public server split and archive",
                "running",
                "RC on the new pair",
                &["gameplay#30"],
            ),
            item(
                "deploy",
                "Redeploy v2.83.3",
                "blocked",
                "waits for gameplay#30",
                &[],
            ),
        ],
        &[],
    )
    .map(|(s, _)| s)
    .unwrap();
    for (title, labels) in [
        ("Flaky RC on cold VMs", vec!["partial"]),
        ("Pin the new pack", vec!["next-release"]),
    ] {
        backlog::add(
            &paths,
            "blockfield",
            NewEntry {
                title: title.into(),
                labels: labels.into_iter().map(Into::into).collect(),
                ..NewEntry::default()
            },
        )
        .unwrap();
    }

    // Pin the save time so the snapshot does not change every run.
    let file = paths.state_dir.join("threads/thread-1.json");
    let mut saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    saved["updated"] = "2026-11-16T09:05:00Z".into();
    std::fs::write(&file, serde_json::to_vec(&saved).unwrap()).unwrap();
    let reports = paths.state_dir.join("coordinator/reports.jsonl");
    let pinned: String = std::fs::read_to_string(&reports)
        .unwrap()
        .lines()
        .map(|l| {
            let mut v: serde_json::Value = serde_json::from_str(l).unwrap();
            v["at"] = "2026-11-16T09:05:00Z".into();
            v.to_string() + "\n"
        })
        .collect();
    std::fs::write(&reports, pinned).unwrap();
    let data = agentdeck_gui::data::load(&paths);
    assert_eq!(data.coordinators.len(), 1);
    assert_eq!(data.backlogs.len(), 1, "{:?}", data.backlogs);
    let mut state = app::State::for_tests(paths, data);
    let _ = app::update(&mut state, Message::SelectPage(Page::Coordinator));
    {
        let mut ui = iced_test::simulator(app::view(&state));
        ui.find("1/3").expect("progress pill");
        ui.find("Public server split and archive")
            .expect("checklist item");
        ui.find("Backlog blockfield · 2 open")
            .expect("backlog heading");
        ui.find("Reports").expect("reports card");
    }
    std::fs::create_dir_all(SNAPSHOTS).unwrap();
    for lang in ["en", "ru"] {
        agentdeck_gui::i18n::set_language(lang);
        let mut ui = iced_test::Simulator::with_size(
            iced::Settings::default(),
            (760.0, 400.0),
            app::view(&state),
        );
        let snapshot = ui
            .snapshot(&agentdeck_gui::style::theme(true))
            .expect("headless snapshot");
        assert_snapshot(&snapshot, &format!("coordinator_{lang}"));
    }
    agentdeck_gui::i18n::set_language("en");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Drain an iced task to its output messages (GUI flows stay headless: any
/// real runtime effect fails the test).
fn drain<T>(task: iced::Task<T>) -> Vec<T> {
    use iced::futures::StreamExt;
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(async {
            let mut out = Vec::new();
            let mut pending = vec![task];
            while let Some(task) = pending.pop() {
                if let Some(mut stream) = iced_test::runtime::task::into_stream(task) {
                    while let Some(action) = stream.next().await {
                        match action {
                            iced_test::runtime::Action::Output(message) => out.push(message),
                            _ => panic!("unexpected GUI runtime effect"),
                        }
                    }
                }
            }
            out
        })
}

fn test_env(tag: &str) -> (agentdeck_core::Paths, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "agentdeck-gui-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("home")).unwrap();
    std::fs::create_dir_all(dir.join("config")).unwrap();
    unsafe {
        std::env::set_var("HOME", dir.join("home"));
        std::env::set_var("AGENTDECK_T3_DIR", dir.join("t3"));
        std::env::set_var("AGENTDECK_PROC_DIR", dir.join("proc"));
        std::env::set_var("XDG_STATE_HOME", dir.join("xdg"));
    }
    std::fs::write(
        dir.join("config/instances.toml"),
        "version = 1\n\n[plugins]\ncoordinator = true\napprover = true\n",
    )
    .unwrap();
    (Paths::new(dir.join("config"), dir.join("state")), dir)
}

/// A Coordinator thread at `SleepingForQuota` with a stale snapshot, an
/// ungranted Run Mandate, one open Owner Inbox item and one Assumption.
/// Renders in both languages, then the owner's Grant and Resolve buttons flip
/// the stored state through the GUI messages.
#[test]
fn coordinator_phase_mandate_inbox_assumptions_and_owner_actions() {
    let _env = lock_env();
    let (paths, dir) = test_env("coord-phase");
    agentdeck_gui::i18n::set_language("en");
    thread_state::write(&paths, None, "thread-9", Some("Ship the release"), None).unwrap();
    let items = [
        item("cp", "CP v2 cutover", "done", "", &[]),
        item("62", "Public server split", "running", "", &[]),
    ];
    thread_state::update_checklist(&paths, None, "thread-9", None, &items, &[]).unwrap();
    // Five quiet updates age the snapshot: the stale badge must show.
    for _ in 0..5 {
        thread_state::update_checklist(&paths, None, "thread-9", None, &items, &[]).unwrap();
    }
    thread_state::inbox_add(
        &paths,
        None,
        "thread-9",
        NewInboxItem {
            id: None,
            title: "Deploy needs an owner call".into(),
            reason: "outside the mandate".into(),
            action: "approve the host".into(),
            blocked_items: vec!["deploy".into()],
        },
    )
    .unwrap();
    thread_state::assumption_add(
        &paths,
        None,
        "thread-9",
        NewAssumption {
            id: None,
            decision: "Pinned model to gpt-5".into(),
            rationale: "cheapest strong model".into(),
            revert_hint: "set model back to opus".into(),
        },
    )
    .unwrap();
    thread_state::set_run_meta(
        &paths,
        None,
        "thread-9",
        Some(RunPhase::SleepingForQuota),
        Some(RunMandate {
            project: "blockfield".into(),
            allowed_hosts: vec!["hosts.example".into()],
            allowed_apps: vec!["deploy".into()],
            allow_releases: false,
            fix_forward_max: 3,
            review_rounds_max: 2,
            owner_granted: false,
            ..RunMandate::default()
        }),
        Some(Some("2026-11-22T01:00:00Z".into())),
        false,
    )
    .unwrap();
    let found = thread_state::read(&paths, None, "thread-9")
        .unwrap()
        .expect("thread saved");
    assert!(found.data.state_stale());
    assert_eq!(found.data.open_inbox_count(), 1);
    assert!(matches!(found.data.phase, RunPhase::SleepingForQuota));

    // Pin the save time so the snapshots do not change every run.
    let file = paths.state_dir.join("threads/thread-9.json");
    let mut saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    saved["updated"] = "2026-11-16T09:05:00Z".into();
    std::fs::write(&file, serde_json::to_vec(&saved).unwrap()).unwrap();

    let data = agentdeck_gui::data::load(&paths);
    assert_eq!(data.coordinators.len(), 1);
    let mut state = app::State::for_tests(paths.clone(), data);
    let _ = app::update(&mut state, Message::SelectPage(Page::Coordinator));
    {
        let mut ui = iced_test::simulator(app::view(&state));
        ui.find("Sleeping for quota · 2026-11-22T01:00:00Z")
            .expect("phase with wake time");
        ui.find("Stale state \u{2014} rewrite the snapshot before checkpointing").expect("stale badge");
        ui.find("Inbox: 1").expect("inbox badge");
        ui.find("Run Mandate").expect("mandate block");
        ui.find("Not granted").expect("mandate state");
        ui.find("Hosts").expect("mandate hosts");
        ui.find("Grant mandate").expect("grant button");
        ui.find("Owner Inbox").expect("inbox block");
        ui.find("Deploy needs an owner call").expect("inbox item");
        ui.find("Mark resolved").expect("resolve button");
        ui.find("Assumptions").expect("assumptions block");
        ui.find("Pinned model to gpt-5").expect("assumption");
        ui.find("Fix-forward \u{2264} 3 \u{B7} reviews \u{2264} 2").expect("mandate caps");
    }
    std::fs::create_dir_all(SNAPSHOTS).unwrap();
    for lang in ["en", "ru"] {
        agentdeck_gui::i18n::set_language(lang);
        let mut ui = iced_test::Simulator::with_size(
            iced::Settings::default(),
            (760.0, 400.0),
            app::view(&state),
        );
        let snapshot = ui
            .snapshot(&agentdeck_gui::style::theme(true))
            .expect("headless snapshot");
        assert_snapshot(&snapshot, &format!("coordinator_phase_{lang}"));
    }
    agentdeck_gui::i18n::set_language("ru");
    {
        let mut ui = iced_test::simulator(app::view(&state));
        ui.find("Сон по квоте · 2026-11-22T01:00:00Z").expect("RU phase");
        ui.find("Мандат запуска").expect("RU mandate");
        ui.find("Выдать мандат").expect("RU grant");
        ui.find("Входящие владельца").expect("RU inbox");
        ui.find("Отметить решённым").expect("RU resolve");
        ui.find("Допущения").expect("RU assumptions");
    }
    agentdeck_gui::i18n::set_language("en");

    // The owner's Grant click seals the mandate; Resolve parks the inbox item.
    let mut ui = iced_test::simulator(app::view(&state));
    ui.click("Grant mandate").expect("grant click");
    for message in ui.into_messages() {
        for out in drain(app::update(&mut state, message)) {
            for out2 in drain(app::update(&mut state, out)) {
                let _ = app::update(&mut state, out2);
            }
        }
    }
    let granted = thread_state::read(&paths, None, "thread-9")
        .unwrap()
        .expect("thread saved");
    assert!(granted.data.mandate.expect("mandate").owner_granted);
    let mut ui = iced_test::simulator(app::view(&state));
    ui.find("Granted").expect("granted pill");
    ui.click("Mark resolved").expect("resolve click");
    for message in ui.into_messages() {
        for out in drain(app::update(&mut state, message)) {
            for out2 in drain(app::update(&mut state, out)) {
                let _ = app::update(&mut state, out2);
            }
        }
    }
    let resolved = thread_state::read(&paths, None, "thread-9")
        .unwrap()
        .expect("thread saved");
    assert_eq!(resolved.data.open_inbox_count(), 0);
    agentdeck_gui::i18n::set_language("en");
    let _ = std::fs::remove_dir_all(&dir);
}

/// PLUGIN_APPROVER shows on the Modules page, and ApproverSettings plus
/// CoordinatorSettings survive a GUI load/edit/save round-trip.
#[test]
fn approver_plugin_and_settings_round_trip() {
    let _env = lock_env();
    let (paths, dir) = test_env("approver-settings");
    agentdeck_gui::i18n::set_language("en");
    settings::write_approver(
        &paths,
        &ApproverSettings {
            autonomous_timeout_secs: 30,
            interactive_timeout_secs: 45,
            judge_provider: Some("openrouter".into()),
            judge_model: Some("x/y".into()),
            judge_confidence_threshold: 0.9,
            remote_hosts: vec!["hosts.example".into()],
            socket_path: None,
            extra_hard_deny_patterns: vec!["secret".into()],
        },
        false,
    )
    .unwrap();
    settings::write_coordinator(
        &paths,
        &CoordinatorSettings {
            pool: vec![CoordinatorPoolEntry {
                instance_id: "inst1".into(),
                model: "model-a".into(),
                options: Default::default(),
                enabled: true,
            }],
            min_weekly_reserve_pct: 15.0,
            proactive_rotate_below_5h_pct: 12.0,
            quota_sleep_buffer_secs: 130,
            idle_wake_grace_secs: 190,
            checkpoint_soft_runs: 70,
            checkpoint_soft_items: 1400,
            checkpoint_confidence_threshold: 0.85,
            min_free_disk_gb: 20,
            t3_endpoint: Some("http://127.0.0.1:9999/".into()),
            t3_token_file: None,
            judge_provider: Some("vercel".into()),
            judge_model: Some("j/model".into()),
        },
        false,
    )
    .unwrap();
    // One audit row for the recent-audit viewer.
    let audit_dir = paths.state_dir.join("approver");
    std::fs::create_dir_all(&audit_dir).unwrap();
    std::fs::write(
        audit_dir.join("audit.jsonl"),
        "{\"timestamp\":123,\"request_id\":\"r1\",\"host\":\"hosts.example\",\"kind\":\"exec\",\"sha256_payload\":\"x\",\"perimeter\":\"p\",\"outcome\":\"allowed\",\"source\":\"PolicyJudge\",\"elapsed_ms\":5}\n",
    )
    .unwrap();

    // GUI state loads what core wrote.
    let mut sstate = gsettings::State::default();
    let _ = gsettings::update(&mut sstate, paths.clone(), gsettings::load_approver(&paths));
    assert_eq!(sstate.approver.auto_timeout, "30");
    assert_eq!(sstate.approver.inter_timeout, "45");
    assert_eq!(sstate.approver.judge, gsettings::JudgeProvider::OpenRouter);
    assert_eq!(sstate.approver.judge_model, "x/y");
    assert_eq!(sstate.approver.confidence, "0.9");
    assert_eq!(sstate.approver.remote_hosts, "hosts.example");
    assert_eq!(sstate.approver.deny, "secret");
    assert_eq!(sstate.approver.loaded.audit.len(), 1);
    let _ = gsettings::update(&mut sstate, paths.clone(), gsettings::load_coord(&paths));
    assert_eq!(sstate.coord.entries.len(), 1);
    assert_eq!(sstate.coord.reserve, "15");
    assert_eq!(sstate.coord.t3_endpoint, "http://127.0.0.1:9999/");

    // Edit through the GUI messages, save, read back from disk.
    let _ = gsettings::update(
        &mut sstate,
        paths.clone(),
        gsettings::Msg::Approver(Box::new(gsettings::ApproverMsg::AutoTimeout("31".into()))),
    );
    for out in drain(gsettings::update(
        &mut sstate,
        paths.clone(),
        gsettings::Msg::Approver(Box::new(gsettings::ApproverMsg::Save)),
    )) {
        for out2 in drain(gsettings::update(&mut sstate, paths.clone(), out)) {
            let _ = gsettings::update(&mut sstate, paths.clone(), out2);
        }
    }
    assert_eq!(
        settings::read_approver(&agentdeck_core::Registry::load(&paths).unwrap())
            .unwrap()
            .autonomous_timeout_secs,
        31
    );
    let _ = gsettings::update(
        &mut sstate,
        paths.clone(),
        gsettings::Msg::Coord(Box::new(gsettings::CoordMsg::Reserve("16".into()))),
    );
    let _ = gsettings::update(
        &mut sstate,
        paths.clone(),
        gsettings::Msg::Coord(Box::new(gsettings::CoordMsg::NewInstance("inst2".into()))),
    );
    let _ = gsettings::update(
        &mut sstate,
        paths.clone(),
        gsettings::Msg::Coord(Box::new(gsettings::CoordMsg::NewModel("model-b".into()))),
    );
    let _ = gsettings::update(
        &mut sstate,
        paths.clone(),
        gsettings::Msg::Coord(Box::new(gsettings::CoordMsg::Add)),
    );
    assert_eq!(sstate.coord.entries.len(), 2);
    for out in drain(gsettings::update(
        &mut sstate,
        paths.clone(),
        gsettings::Msg::Coord(Box::new(gsettings::CoordMsg::Save)),
    )) {
        for out2 in drain(gsettings::update(&mut sstate, paths.clone(), out)) {
            let _ = gsettings::update(&mut sstate, paths.clone(), out2);
        }
    }
    let back =
        settings::read_coordinator(&agentdeck_core::Registry::load(&paths).unwrap()).unwrap();
    assert!((back.min_weekly_reserve_pct - 16.0).abs() < f64::EPSILON);
    assert_eq!(back.pool.len(), 2);

    // Pages render: approver card, both settings forms, audit row.
    let data = agentdeck_gui::data::load(&paths);
    assert!(data.plugins.iter().any(|p| p.name == "approver"));
    let mut state = app::State::for_tests(paths.clone(), data);
    let _ = app::update(&mut state, Message::SelectPage(Page::Modules));
    {
        let mut ui = iced_test::simulator(app::view(&state));
        ui.find("approver").expect("approver module card");
        ui.find("Local approvals: hard-deny perimeter, policy judge, Telegram confirm.").expect("approver description");
    }
    for out in drain(app::update(
        &mut state,
        Message::SelectPage(Page::ApproverSettings),
    )) {
        for out2 in drain(app::update(&mut state, out)) {
            let _ = app::update(&mut state, out2);
        }
    }
    {
        let mut ui = iced_test::simulator(app::view(&state));
        ui.find("Recent audit").expect("audit card");
        ui.find("123 \u{B7} hosts.example").expect("audit row");
        ui.find("Save").expect("approver save");
    }
    for out in drain(app::update(
        &mut state,
        Message::SelectPage(Page::CoordinatorSettings),
    )) {
        for out2 in drain(app::update(&mut state, out)) {
            let _ = app::update(&mut state, out2);
        }
    }
    {
        let mut ui = iced_test::simulator(app::view(&state));
        ui.find("Coordinator Pool").expect("pool editor");
        ui.find("model-a").expect("pool entry");
        ui.find("T3 loopback endpoint").expect("T3 endpoint");
    }
    for lang in ["en", "ru"] {
        agentdeck_gui::i18n::set_language(lang);
        let mut ui = iced_test::Simulator::with_size(
            iced::Settings::default(),
            (760.0, 560.0),
            app::view(&state),
        );
        ui.snapshot(&agentdeck_gui::style::theme(true))
            .expect("headless settings render");
        let mut ui = iced_test::simulator(app::view(&state));
        if lang == "ru" {
            ui.find("Пул координатора").expect("RU pool");
        }
    }
    agentdeck_gui::i18n::set_language("en");
    let _ = std::fs::remove_dir_all(&dir);
}
