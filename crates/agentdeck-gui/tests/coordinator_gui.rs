//! Coordinator page: a thread's goal and checklist plus the open backlog,
//! rendered headless from state written through the core API (EN/RU).

use agentdeck_core::Paths;
use agentdeck_core::backlog::{self, NewEntry};
use agentdeck_core::settings::{self, ApproverSettings, CoordinatorPoolEntry, CoordinatorSettings};
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
        ui.find("Reports").expect("reports card");
        assert!(
            ui.find("Backlog blockfield · 2 open").is_err(),
            "backlog belongs on its own tab"
        );
    }
    let _ = app::update(&mut state, Message::SelectPage(Page::Backlog));
    {
        let mut ui = iced_test::simulator(app::view(&state));
        ui.find("Backlog blockfield · 2 open")
            .expect("backlog heading");
        ui.find("Flaky RC on cold VMs").expect("backlog item");
        assert!(
            ui.find("Public server split and archive").is_err(),
            "progress checklist belongs on the Progress tab"
        );
    }
    agentdeck_gui::i18n::set_language("ru");
    {
        let mut ui = iced_test::simulator(app::view(&state));
        ui.find("Бэклог blockfield · открыто 2")
            .expect("RU backlog heading");
    }
    agentdeck_gui::i18n::set_language("en");
    let _ = app::update(&mut state, Message::SelectPage(Page::Coordinator));
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
        ui.find("Stale state").expect("stale badge");
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
        ui.find("Fix-forward \u{2264} 3 \u{B7} reviews \u{2264} 2")
            .expect("mandate caps");
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
        ui.find("Сон по квоте · 2026-11-22T01:00:00Z")
            .expect("RU phase");
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
        "{\"timestamp\":123,\"request_id\":\"r1\",\"host\":\"hosts.example\",\"kind\":\"exec\",\"sha256_payload\":\"x\",\"perimeter\":\"p\",\"outcome\":\"approved\",\"source\":\"PolicyJudge\",\"elapsed_ms\":5}\n",
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
        ui.find("Approves in-scope requests and sends the rest to Telegram.")
            .expect("approver description");
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
        ui.find("Recent decisions").expect("audit card");
        ui.find("1970-01-01 00:02:03 \u{B7} hosts.example")
            .expect("audit row");
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
        ui.find("T3 endpoint").expect("T3 endpoint");
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

/// Drive an owner Grant/Revoke through `app::update` like the shell,
/// refreshing state afterwards, and return the `CoordDone` outcome.
fn run_owner_action(state: &mut app::State, msg: Message) -> Result<(), String> {
    let mut done: Result<(), String> = Err("no CoordDone".to_string());
    for out in drain(app::update(state, msg)) {
        match out {
            Message::CoordDone(r) => done = r,
            other => panic!("unexpected message {other:?}"),
        }
    }
    for out in drain(app::update(state, Message::CoordDone(done.clone()))) {
        let _ = app::update(state, out);
    }
    done
}

/// Safe owner Grant/Revoke: no mandate shows no Grant button and refuses a
/// direct Grant; Intake grants the displayed snapshot; a mandate replaced on
/// disk after the card rendered is rejected; Completed rejects grants; Revoke
/// clears only `owner_granted` while keeping the scope.
#[test]
fn mandate_grant_revoke_guards_scope_and_phase() {
    let _env = lock_env();
    let (paths, dir) = test_env("coord-mandate-guards");
    agentdeck_gui::i18n::set_language("en");

    fn shown() -> RunMandate {
        RunMandate {
            project: "blockfield".into(),
            allowed_hosts: vec!["hosts.example".into()],
            allowed_apps: vec!["deploy".into()],
            allow_releases: false,
            fix_forward_max: 3,
            review_rounds_max: 2,
            notes: "only staging".into(),
            ..RunMandate::default()
        }
    }

    // (a) No mandate: hint only, no Grant button, direct Grant refused.
    thread_state::write(&paths, None, "t-none", Some("goal"), None).unwrap();
    let mut state = app::State::for_tests(paths.clone(), agentdeck_gui::data::load(&paths));
    let _ = app::update(&mut state, Message::SelectPage(Page::Coordinator));
    {
        let mut ui = iced_test::simulator(app::view(&state));
        ui.find("No mandate yet.").expect("no-mandate hint");
        assert!(
            ui.find("Grant mandate").is_err(),
            "no Grant button without a mandate"
        );
    }
    let refused = run_owner_action(
        &mut state,
        Message::CoordMandate {
            thread: "t-none".into(),
            expected: RunMandate::default(),
            grant: true,
        },
    );
    assert!(refused.is_err(), "grant without mandate: {refused:?}");

    // Intake mandate (empty host/app lists read as unrestricted) plus a
    // raced thread and a completed run.
    thread_state::write(&paths, None, "t-full", Some("goal"), None).unwrap();
    thread_state::set_run_meta(
        &paths,
        None,
        "t-full",
        None,
        Some(RunMandate {
            allowed_hosts: Vec::new(),
            allowed_apps: Vec::new(),
            ..shown()
        }),
        None,
        false,
    )
    .unwrap();
    thread_state::write(&paths, None, "t-race", Some("goal"), None).unwrap();
    thread_state::set_run_meta(&paths, None, "t-race", None, Some(shown()), None, false).unwrap();
    thread_state::write(&paths, None, "t-done", Some("goal"), None).unwrap();
    thread_state::set_run_meta(
        &paths,
        None,
        "t-done",
        Some(RunPhase::Completed),
        Some(shown()),
        None,
        false,
    )
    .unwrap();
    let mut state = app::State::for_tests(paths.clone(), agentdeck_gui::data::load(&paths));
    let _ = app::update(&mut state, Message::SelectPage(Page::Coordinator));
    {
        let mut ui = iced_test::simulator(app::view(&state));
        ui.find("Project").expect("project label");
        ui.find("blockfield").expect("project value");
        ui.find("any (unrestricted)")
            .expect("empty scope reads as unrestricted");
        ui.find("Notes").expect("notes label");
        ui.find("only staging").expect("notes shown");
        ui.find("Grant mandate").expect("intake grant button");
    }

    // (c) Intake grant succeeds with the displayed snapshot.
    let expected = RunMandate {
        allowed_hosts: Vec::new(),
        allowed_apps: Vec::new(),
        ..shown()
    };
    let granted = run_owner_action(
        &mut state,
        Message::CoordMandate {
            thread: "t-full".into(),
            expected: expected.clone(),
            grant: true,
        },
    );
    assert!(granted.is_ok(), "intake grant: {granted:?}");
    {
        let mut ui = iced_test::simulator(app::view(&state));
        ui.find("Granted").expect("granted pill");
        ui.find("Revoke mandate").expect("revoke button");
    }

    // (b) A mandate replaced on disk after the card rendered is rejected.
    thread_state::set_run_meta(
        &paths,
        None,
        "t-race",
        None,
        Some(RunMandate {
            allowed_hosts: vec!["other.example".into()],
            ..shown()
        }),
        None,
        false,
    )
    .unwrap();
    let stale = run_owner_action(
        &mut state,
        Message::CoordMandate {
            thread: "t-race".into(),
            expected: shown(),
            grant: true,
        },
    );
    assert!(stale.is_err(), "stale scope granted: {stale:?}");
    assert!(
        !thread_state::read(&paths, None, "t-race")
            .unwrap()
            .unwrap()
            .data
            .mandate
            .unwrap()
            .owner_granted
    );

    // (d) Completed runs reject grants and show no Grant button (core also
    // clears any live grant when a run completes, so Revoke has no target).
    let done = run_owner_action(
        &mut state,
        Message::CoordMandate {
            thread: "t-done".into(),
            expected: shown(),
            grant: true,
        },
    );
    assert!(done.is_err(), "completed grant: {done:?}");
    for dead in ["t-none", "t-full", "t-race"] {
        let _ = std::fs::remove_file(paths.state_dir.join(format!("threads/{dead}.json")));
    }
    let mut solo = app::State::for_tests(paths.clone(), agentdeck_gui::data::load(&paths));
    let _ = app::update(&mut solo, Message::SelectPage(Page::Coordinator));
    {
        let mut ui = iced_test::simulator(app::view(&solo));
        ui.find("Not granted")
            .expect("completed run stays ungranted");
        assert!(
            ui.find("Grant mandate").is_err(),
            "completed run shows no Grant button"
        );
        assert!(
            ui.find("Revoke mandate").is_err(),
            "nothing to revoke on a completed run"
        );
    }

    // (e) Revoke clears only the flag while keeping the displayed scope.
    thread_state::write(&paths, None, "t-rev", Some("goal"), None).unwrap();
    thread_state::set_run_meta(
        &paths,
        None,
        "t-rev",
        None,
        Some(RunMandate {
            owner_granted: true,
            ..expected.clone()
        }),
        None,
        false,
    )
    .unwrap();
    let mut state = app::State::for_tests(paths.clone(), agentdeck_gui::data::load(&paths));
    let revoked = run_owner_action(
        &mut state,
        Message::CoordMandate {
            thread: "t-rev".into(),
            expected,
            grant: false,
        },
    );
    assert!(revoked.is_ok(), "revoke: {revoked:?}");
    let m = thread_state::read(&paths, None, "t-rev")
        .unwrap()
        .unwrap()
        .data
        .mandate
        .unwrap();
    assert!(!m.owner_granted);
    assert_eq!(m.project, "blockfield");
    assert!(m.allowed_hosts.is_empty() && m.allowed_apps.is_empty());
    assert_eq!(m.notes, "only staging");
    assert_eq!((m.fix_forward_max, m.review_rounds_max), (3, 2));
    assert!(!m.allow_releases);
    agentdeck_gui::i18n::set_language("en");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Saving only the timeout keeps deny patterns with a literal comma and
/// paired backslashes byte-for-byte.
#[test]
fn timeout_only_save_preserves_tricky_deny_patterns() {
    let _env = lock_env();
    let (paths, dir) = test_env("approver-deny-roundtrip");
    agentdeck_gui::i18n::set_language("en");
    let tricky = vec!["a,b".to_string(), "\\\\d+".to_string()];
    settings::write_approver(
        &paths,
        &ApproverSettings {
            extra_hard_deny_patterns: tricky.clone(),
            ..ApproverSettings::default()
        },
        false,
    )
    .unwrap();
    let mut sstate = gsettings::State::default();
    let _ = gsettings::update(&mut sstate, paths.clone(), gsettings::load_approver(&paths));
    let before = sstate.approver.deny.clone();
    // Only the autonomous timeout changes; the deny text is untouched.
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
    let back = settings::read_approver(&agentdeck_core::Registry::load(&paths).unwrap()).unwrap();
    assert_eq!(back.extra_hard_deny_patterns, tricky);
    assert_eq!(back.autonomous_timeout_secs, 31);
    assert_eq!(sstate.approver.deny, before, "field keeps its escaped form");
    agentdeck_gui::i18n::set_language("en");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Failed saves keep the draft; a second save while busy is ignored;
/// out-of-range numbers, bad endpoints and duplicate pool entries fail
/// inline without touching disk.
#[test]
fn settings_forms_guard_save_and_keep_drafts() {
    let _env = lock_env();
    let (paths, dir) = test_env("settings-guards");
    agentdeck_gui::i18n::set_language("en");
    settings::write_approver(&paths, &ApproverSettings::default(), false).unwrap();
    settings::write_coordinator(&paths, &CoordinatorSettings::default(), false).unwrap();
    let mut sstate = gsettings::State::default();
    let _ = gsettings::update(&mut sstate, paths.clone(), gsettings::load_approver(&paths));
    let _ = gsettings::update(&mut sstate, paths.clone(), gsettings::load_coord(&paths));

    // (a) A failed validation keeps the draft with an inline error, and a
    // failed Done from an in-flight write does not reload either.
    let _ = gsettings::update(
        &mut sstate,
        paths.clone(),
        gsettings::Msg::Approver(Box::new(gsettings::ApproverMsg::AutoTimeout("0".into()))),
    );
    assert!(
        drain(gsettings::update(
            &mut sstate,
            paths.clone(),
            gsettings::Msg::Approver(Box::new(gsettings::ApproverMsg::Save)),
        ))
        .is_empty(),
        "invalid save dispatches nothing"
    );
    assert_eq!(sstate.approver.auto_timeout, "0", "draft kept");
    assert!(
        sstate.approver.result.as_ref().is_some_and(|(_, e)| *e),
        "inline error shown"
    );
    let _ = gsettings::update(
        &mut sstate,
        paths.clone(),
        gsettings::Msg::Approver(Box::new(gsettings::ApproverMsg::Done(Err("boom".into())))),
    );
    assert_eq!(sstate.approver.auto_timeout, "0", "failed Done keeps draft");
    let _ = gsettings::update(
        &mut sstate,
        paths.clone(),
        gsettings::Msg::Coord(Box::new(gsettings::CoordMsg::Reserve("abc".into()))),
    );
    let _ = gsettings::update(
        &mut sstate,
        paths.clone(),
        gsettings::Msg::Coord(Box::new(gsettings::CoordMsg::Done(Err("boom".into())))),
    );
    assert_eq!(sstate.coord.reserve, "abc", "coord draft kept");

    // (b) A second save while busy is ignored.
    sstate.approver.busy = true;
    assert!(
        drain(gsettings::update(
            &mut sstate,
            paths.clone(),
            gsettings::Msg::Approver(Box::new(gsettings::ApproverMsg::Save)),
        ))
        .is_empty(),
        "busy approver save ignored"
    );
    assert!(sstate.approver.busy, "still busy");
    sstate.approver.busy = false;
    sstate.coord.busy = true;
    assert!(
        drain(gsettings::update(
            &mut sstate,
            paths.clone(),
            gsettings::Msg::Coord(Box::new(gsettings::CoordMsg::Save)),
        ))
        .is_empty(),
        "busy coord save ignored"
    );
    sstate.coord.busy = false;

    // (c) Out-of-bounds numbers and a non-loopback endpoint fail inline.
    for bad in ["0", "9223372036854775808", "18446744073709551615"] {
        let _ = gsettings::update(
            &mut sstate,
            paths.clone(),
            gsettings::Msg::Approver(Box::new(gsettings::ApproverMsg::AutoTimeout(bad.into()))),
        );
        assert!(
            drain(gsettings::update(
                &mut sstate,
                paths.clone(),
                gsettings::Msg::Approver(Box::new(gsettings::ApproverMsg::Save)),
            ))
            .is_empty(),
            "out-of-bounds {bad} dispatches nothing"
        );
        assert!(
            sstate.approver.result.as_ref().is_some_and(|(_, e)| *e),
            "out-of-bounds {bad} shows an error"
        );
    }
    let back = settings::read_approver(&agentdeck_core::Registry::load(&paths).unwrap()).unwrap();
    assert_eq!(
        back.autonomous_timeout_secs,
        ApproverSettings::default().autonomous_timeout_secs,
        "disk untouched by rejected saves"
    );
    let _ = gsettings::update(
        &mut sstate,
        paths.clone(),
        gsettings::Msg::Coord(Box::new(gsettings::CoordMsg::T3Endpoint(
            "http://example.com/".into(),
        ))),
    );
    assert!(
        drain(gsettings::update(
            &mut sstate,
            paths.clone(),
            gsettings::Msg::Coord(Box::new(gsettings::CoordMsg::Save)),
        ))
        .is_empty(),
        "non-loopback endpoint dispatches nothing"
    );
    assert!(
        sstate.coord.result.as_ref().is_some_and(|(_, e)| *e),
        "endpoint error shown"
    );

    // (d) A duplicate pool entry is rejected before saving.
    let dup = || CoordinatorPoolEntry {
        instance_id: "i".into(),
        model: "m".into(),
        options: Default::default(),
        enabled: true,
    };
    sstate.coord.entries = vec![dup(), dup()];
    assert!(
        drain(gsettings::update(
            &mut sstate,
            paths.clone(),
            gsettings::Msg::Coord(Box::new(gsettings::CoordMsg::Save)),
        ))
        .is_empty(),
        "duplicate pool dispatches nothing"
    );
    assert!(
        sstate.coord.result.as_ref().is_some_and(|(_, e)| *e),
        "duplicate pool error shown"
    );
    let back =
        settings::read_coordinator(&agentdeck_core::Registry::load(&paths).unwrap()).unwrap();
    assert!(back.pool.is_empty(), "disk untouched by duplicate pool");
    agentdeck_gui::i18n::set_language("en");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The audit viewer maps real broker outcomes/sources to localized labels,
/// formats UTC timestamps, and tells an unreadable log apart from an empty one.
#[test]
fn audit_viewer_maps_outcomes_and_read_errors() {
    let _env = lock_env();
    let (paths, dir) = test_env("approver-audit-labels");
    agentdeck_gui::i18n::set_language("en");
    settings::write_approver(&paths, &ApproverSettings::default(), false).unwrap();
    let audit_dir = paths.state_dir.join("approver");
    std::fs::create_dir_all(&audit_dir).unwrap();
    std::fs::write(
        audit_dir.join("audit.jsonl"),
        concat!(
            "not json\n",
            "{\"timestamp\":1720000000,\"request_id\":\"r1\",\"host\":\"h1\",\"kind\":\"exec\",\"outcome\":\"approved\",\"source\":\"Telegram\"}\n",
            "\n",
            "{\"timestamp\":1720000060,\"request_id\":\"r2\",\"host\":\"h2\",\"kind\":\"read\",\"outcome\":\"needs_owner\",\"source\":\"Timeout\"}\n",
            "{\"timestamp\":1720000120,\"request_id\":\"r3\",\"host\":\"h3\",\"kind\":\"exec\",\"outcome\":\"denied\",\"source\":\"Denied\"}\n",
            "{\"timestamp\":1720000180,\"request_id\":\"r4\",\"host\":\"h4\",\"kind\":\"exec\",\"outcome\":\"client_cancelled\",\"source\":\"ClientCancelled\"}\n",
            "{\"timestamp\":1720000240,\"request_id\":\"r5\",\"host\":\"h5\",\"kind\":\"exec\",\"outcome\":\"approved\",\"source\":\"PolicyJudge\"}\n",
        ),
    )
    .unwrap();
    let mut sstate = gsettings::State::default();
    let _ = gsettings::update(&mut sstate, paths.clone(), gsettings::load_approver(&paths));
    assert!(sstate.approver.loaded.audit_error.is_none());
    assert_eq!(sstate.approver.loaded.audit.len(), 5, "bad lines skipped");
    assert_eq!(sstate.approver.loaded.audit[0].at, "2024-07-03 09:46:40");
    // The Refresh button re-reads off-thread with the same result.
    for out in drain(gsettings::update(
        &mut sstate,
        paths.clone(),
        gsettings::Msg::Approver(Box::new(gsettings::ApproverMsg::RefreshAudit)),
    )) {
        let _ = gsettings::update(&mut sstate, paths.clone(), out);
    }
    assert_eq!(sstate.approver.loaded.audit.len(), 5);
    {
        let mut ui = iced_test::simulator(gsettings::approver_view(&sstate));
        for text in [
            "Approved",
            "Needs owner",
            "Denied",
            "Cancelled",
            "Policy judge",
            "Telegram",
            "Timeout",
            "Client cancelled",
            "2024-07-03 09:46:40 \u{B7} h1",
        ] {
            ui.find(text)
                .unwrap_or_else(|_| panic!("EN audit shows {text:?}"));
        }
    }
    agentdeck_gui::i18n::set_language("ru");
    {
        let mut ui = iced_test::simulator(gsettings::approver_view(&sstate));
        for text in [
            "Одобрено",
            "Нужен владелец",
            "Отклонено",
            "Отменено",
            "Судья",
            "Telegram",
            "Таймаут",
            "Отменено клиентом",
        ] {
            ui.find(text)
                .unwrap_or_else(|_| panic!("RU audit shows {text:?}"));
        }
    }
    agentdeck_gui::i18n::set_language("en");

    // A missing log reads as empty, not an error.
    std::fs::remove_file(audit_dir.join("audit.jsonl")).unwrap();
    let _ = gsettings::update(&mut sstate, paths.clone(), gsettings::load_approver(&paths));
    assert!(sstate.approver.loaded.audit.is_empty());
    assert!(sstate.approver.loaded.audit_error.is_none());
    {
        let mut ui = iced_test::simulator(gsettings::approver_view(&sstate));
        ui.find("No audit records yet.").expect("empty hint");
    }
    // An unreadable log surfaces the error instead.
    std::fs::create_dir_all(audit_dir.join("audit.jsonl")).unwrap();
    let _ = gsettings::update(&mut sstate, paths.clone(), gsettings::load_approver(&paths));
    assert!(sstate.approver.loaded.audit_error.is_some());
    {
        let mut ui = iced_test::simulator(gsettings::approver_view(&sstate));
        ui.find("Failed to read audit log.").expect("error hint");
    }
    agentdeck_gui::i18n::set_language("en");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn approver_gui_saves_and_preserves_telegram_credentials() {
    use std::os::unix::fs::PermissionsExt;

    let _env = lock_env();
    let (paths, dir) = test_env("approver-creds-gui");
    agentdeck_gui::i18n::set_language("en");

    let mut sstate = gsettings::State::default();
    let _ = gsettings::update(&mut sstate, paths.clone(), gsettings::load_approver(&paths));
    assert!(!sstate.approver.loaded.creds.bot_token_set);
    assert!(sstate.approver.owner_id.is_empty());

    // Enter Telegram bot token + owner ID directly in GUI and save.
    let _ = gsettings::update(
        &mut sstate,
        paths.clone(),
        gsettings::Msg::Approver(Box::new(gsettings::ApproverMsg::BotToken(
            "123456:ABC-gui-token".into(),
        ))),
    );
    let _ = gsettings::update(
        &mut sstate,
        paths.clone(),
        gsettings::Msg::Approver(Box::new(gsettings::ApproverMsg::OwnerId(
            "987654321".into(),
        ))),
    );
    let done = drain(gsettings::update(
        &mut sstate,
        paths.clone(),
        gsettings::Msg::Approver(Box::new(gsettings::ApproverMsg::Save)),
    ));
    let reload = drain(gsettings::update(
        &mut sstate,
        paths.clone(),
        done[0].clone(),
    ));
    for msg in reload {
        let _ = gsettings::update(&mut sstate, paths.clone(), msg);
    }

    assert!(
        sstate.approver.bot_token.is_empty(),
        "token input cleared after save"
    );
    assert!(sstate.approver.loaded.creds.bot_token_set);
    assert_eq!(sstate.approver.owner_id, "987654321");
    assert!(settings::approver_telegram_configured(&paths.config_dir));
    let env_path = paths.config_dir.join("approver.env");
    let mode = std::fs::metadata(&env_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);

    // Unrelated timeout save with blank token field preserves the stored bot token.
    let _ = gsettings::update(
        &mut sstate,
        paths.clone(),
        gsettings::Msg::Approver(Box::new(gsettings::ApproverMsg::AutoTimeout("90".into()))),
    );
    let done = drain(gsettings::update(
        &mut sstate,
        paths.clone(),
        gsettings::Msg::Approver(Box::new(gsettings::ApproverMsg::Save)),
    ));
    let reload = drain(gsettings::update(
        &mut sstate,
        paths.clone(),
        done[0].clone(),
    ));
    for msg in reload {
        let _ = gsettings::update(&mut sstate, paths.clone(), msg);
    }
    let raw = std::fs::read_to_string(&env_path).unwrap();
    assert!(raw.contains("TELEGRAM_BOT_TOKEN=123456:ABC-gui-token"));
    assert!(raw.contains("TELEGRAM_OWNER_ID=987654321"));

    // Invalid owner ID fails inline without touching disk or erasing the draft.
    for bad in ["not-a-number", "0"] {
        let _ = gsettings::update(
            &mut sstate,
            paths.clone(),
            gsettings::Msg::Approver(Box::new(gsettings::ApproverMsg::OwnerId(bad.into()))),
        );
        let follow = drain(gsettings::update(
            &mut sstate,
            paths.clone(),
            gsettings::Msg::Approver(Box::new(gsettings::ApproverMsg::Save)),
        ));
        assert!(
            follow.is_empty(),
            "invalid owner_id {bad} must not spawn save"
        );
        assert!(matches!(sstate.approver.result, Some((_, true))));
        assert_eq!(sstate.approver.owner_id, bad);
    }
    assert_eq!(
        settings::read_approver_credentials(&paths.config_dir).owner_id,
        Some(987654321)
    );

    let _ = std::fs::remove_dir_all(&dir);
}
