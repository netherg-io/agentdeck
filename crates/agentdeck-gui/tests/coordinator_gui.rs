//! Coordinator page: a thread's goal and checklist plus the open backlog,
//! rendered headless from state written through the core API (EN/RU).

use agentdeck_core::Paths;
use agentdeck_core::backlog::{self, NewEntry};
use agentdeck_core::thread_state::{self, ItemPatch};
use agentdeck_gui::app::{self, Message, Page};

const SNAPSHOTS: &str = "tests/snapshots";

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
