//! G2 headless tests: add-wizard snapshots (EN/RU), the embedded login panel
//! driven by a fake script through the real PTY path, and the remove
//! confirmation. Never opens a window; never touches real homes.

use std::path::{Path, PathBuf};

use agentdeck_core::{Paths, Registry};
use agentdeck_gui::app::{self, Message, Page};

const SNAPSHOTS: &str = "tests/snapshots";

fn fixture_paths(tag: &str) -> (Paths, PathBuf, tempfile_guard::Guard) {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "agentdeck-gui-add-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    unsafe {
        std::env::set_var("XDG_STATE_HOME", dir.join("xdg"));
    }
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let paths = Paths::new(dir.join("config"), dir.join("state"));
    (paths, dir.clone(), tempfile_guard::Guard(dir))
}

mod tempfile_guard {
    use std::path::PathBuf;
    pub struct Guard(pub PathBuf);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// `AGENTDECK_T3_DIR` and `HOME` are process-global: hold the lock while set.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Pin every real-machine source to empty temp paths: with the real
    // ~/.t3 (a multi-GB state DB) and /proc, each data::load took minutes.
    let empty = std::env::temp_dir().join(format!("agentdeck-gui-empty-{}", std::process::id()));
    std::fs::create_dir_all(&empty).unwrap();
    unsafe {
        std::env::set_var("AGENTDECK_T3_DIR", empty.join("t3"));
        std::env::set_var("AGENTDECK_PROC_DIR", &empty);
        std::env::set_var("HOME", &empty);
    }
    guard
}

fn fake_login_script(dir: &Path) -> PathBuf {
    let script = dir.join("fake-login.sh");
    std::fs::write(
        &script,
        "#!/bin/bash\n\
         printf 'visit https://example.com/login?x=7 now\\r\\n'\n\
         printf 'Paste code here if prompted > '\n\
         IFS= read -r line\n\
         printf 'done\\r\\n'\n\
         exit 0\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    script
}

fn update(state: &mut app::State, message: Message) {
    let _ = app::update(state, message);
}

#[test]
fn wizard_snapshots_en_ru() {
    let _env = lock_env();
    let (paths, _dir, _guard) = fixture_paths("shots");
    let data = agentdeck_gui::data::load(&paths);
    let mut state = app::State::for_tests(paths, data);
    update(&mut state, Message::SelectPage(Page::AddInstance));
    assert!(matches!(state.page(), Page::AddInstance));
    update(
        &mut state,
        Message::Add(agentdeck_gui::pages::add_instance::AddMsg::Id(
            "second".into(),
        )),
    );
    update(
        &mut state,
        Message::Add(agentdeck_gui::pages::add_instance::AddMsg::PreviewReady(
            Ok(vec![
                "write /tmp/fake/instances.toml:".into(),
                "+[[instance]] id = \"second\"".into(),
            ]),
        )),
    );
    std::fs::create_dir_all(SNAPSHOTS).unwrap();
    for (lang, stem) in [("en", "add_en"), ("ru", "add_ru")] {
        agentdeck_gui::i18n::set_language(lang);
        // Smaller height keeps the golden under 100 KB.
        let mut ui = iced_test::Simulator::with_size(
            iced::Settings::default(),
            (900.0, 520.0),
            app::view(&state),
        );
        let snapshot = ui
            .snapshot(&agentdeck_gui::style::theme(true))
            .expect("headless snapshot");
        // Goldens depend on this machine's fonts and GPU rasterizer.
        if std::env::var_os("CI").is_none() {
            assert!(
                snapshot
                    .matches_image(format!("{SNAPSHOTS}/{stem}.png"))
                    .expect("snapshot compare/write"),
                "snapshot {stem} changed"
            );
        }
        let bytes = std::fs::metadata(format!("{SNAPSHOTS}/{stem}-wgpu.png"))
            .map(|m| m.len())
            .or_else(|_| std::fs::metadata(format!("{SNAPSHOTS}/{stem}.png")).map(|m| m.len()))
            .expect("golden png");
        assert!(bytes < 100 * 1024, "golden {stem} must stay under 100 KB");
    }
}

#[test]
fn login_panel_runs_fake_script_and_sends_code() {
    use agentdeck_gui::pages::add_instance::AddMsg;
    let _env = lock_env();
    agentdeck_gui::i18n::set_language("en");
    let (paths, dir, _guard) = fixture_paths("login");
    let script = fake_login_script(&dir);
    let home = dir.join("second-home");
    Registry::update(&paths, |r| {
        r.add(agentdeck_core::registry::AddSpec {
            id: "second".into(),
            harness: agentdeck_core::Harness::Claude,
            display_name: None,
            home: home.display().to_string(),
            binary: Some(script.display().to_string()),
            no_t3: false,
        })?;
        Ok(())
    })
    .unwrap();
    let data = agentdeck_gui::data::load(&paths);
    let mut state = app::State::for_tests(paths, data);
    update(&mut state, Message::SelectPage(Page::AddInstance));
    update(&mut state, Message::Add(AddMsg::Id("second".into())));
    // Skip the real apply: the registry row already exists with the fake binary.
    update(
        &mut state,
        Message::Add(AddMsg::ApplyDone(Ok(vec!["added second".into()]))),
    );
    update(&mut state, Message::Add(AddMsg::LoginStart));
    // Let the PTY child print its URL and prompt, then drain via the poll.
    std::thread::sleep(std::time::Duration::from_secs(1));
    update(&mut state, Message::Add(AddMsg::LoginPoll));
    let mut ui = iced_test::simulator(app::view(&state));
    ui.find("https://example.com/login?x=7")
        .expect("login URL in panel");
    ui.find("Send").expect("code Send button");
    // Type the pasted code and send it; the script then exits 0.
    ui.click("Pasted code").expect("code input");
    for message in ui.into_messages() {
        update(&mut state, message);
    }
    let mut ui = iced_test::simulator(app::view(&state));
    ui.typewrite("test-code-7");
    for message in ui.into_messages() {
        update(&mut state, message);
    }
    update(&mut state, Message::Add(AddMsg::LoginSend));
    std::thread::sleep(std::time::Duration::from_secs(1));
    update(&mut state, Message::Add(AddMsg::LoginPoll));
    let mut ui = iced_test::simulator(app::view(&state));
    ui.find("Login process exited 0.")
        .expect("login success line");
    agentdeck_gui::i18n::set_language("en");
}

#[test]
fn remove_confirm_shows_and_cancels() {
    let _env = lock_env();
    agentdeck_gui::i18n::set_language("en");
    let (paths, _dir, _guard) = fixture_paths("remove");
    Registry::update(&paths, |r| {
        r.add(agentdeck_core::registry::AddSpec {
            id: "gone".into(),
            harness: agentdeck_core::Harness::Claude,
            display_name: None,
            home: "/fake/gone".into(),
            binary: None,
            no_t3: false,
        })?;
        r.add_pool(agentdeck_core::pool::PoolEntry {
            instance: "gone".into(),
            model: "m1".into(),
            options: Default::default(),
            enabled: false,
        })?;
        r.add_usage(agentdeck_core::usage::UsageEntry {
            id: "gone-quota".into(),
            kind: agentdeck_core::usage::UsageKind::Claude,
            instance: Some("gone".into()),
            display: String::new(),
            api_key_env: None,
            db_file: None,
            extra: Default::default(),
        })?;
        Ok(())
    })
    .unwrap();
    let data = agentdeck_gui::data::load(&paths);
    let mut state = app::State::for_tests(paths, data);
    update(&mut state, Message::RemoveAsk("gone".into()));
    {
        let mut ui = iced_test::simulator(app::view(&state));
        ui.find("The login dir is kept.").expect("keep-home note");
        // The pool entry goes with the instance; the dialog names it.
        let pool_line: String = i18n_embed_fl::fl!(
            agentdeck_gui::i18n::loader(),
            "remove-pool",
            models = "m1".to_string()
        );
        ui.find(pool_line.as_str()).expect("pool line");
        let usage_line: String = i18n_embed_fl::fl!(
            agentdeck_gui::i18n::loader(),
            "remove-usage",
            sources = "gone-quota".to_string()
        );
        ui.find(usage_line.as_str()).expect("usage line");
        ui.click("Keep").expect("cancel button");
        for message in ui.into_messages() {
            update(&mut state, message);
        }
    }
    {
        let mut ui = iced_test::simulator(app::view(&state));
        assert!(
            ui.find("The login dir is kept.").is_err(),
            "panel closes on cancel"
        );
    }
    // A core refusal (usage reference) stays visible in the panel.
    update(&mut state, Message::RemoveAsk("gone".into()));
    update(
        &mut state,
        Message::RemoveDone(Err(
            "cannot remove instance \"gone\": still used by usage entries:\n  u".into(),
        )),
    );
    let mut ui = iced_test::simulator(app::view(&state));
    ui.find("Keep").expect("panel stays open on refusal");
    // Placeables render with Unicode isolates, so the title text is pinned
    // here instead of through the exact-match widget selector.
    let title: String = i18n_embed_fl::fl!(
        agentdeck_gui::i18n::loader(),
        "remove-ask",
        id = "gone".to_string()
    );
    assert!(title.contains("gone") && title.contains('?'), "{title:?}");
    agentdeck_gui::i18n::set_language("en");
}
