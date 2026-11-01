//! G7 headless tests: Pool, Shared and Harnesses pages (EN/RU snapshots plus
//! interactions) and the opencode/agy wizard rows. Everything runs on temp
//! dirs with HOME and the T3 dir pinned there; no window, no real homes.

use std::path::{Path, PathBuf};

use agentdeck_core::harness::Status;
use agentdeck_core::{Paths, PoolEntry, Registry};
use agentdeck_gui::app::{self, Message, Page};
use agentdeck_gui::pages::add_instance::{AddMsg, HarnessChoice};
use agentdeck_gui::pages::{harnesses, pool, shared};

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
        let golden = format!("{SNAPSHOTS}/{stem}-wgpu.png");
        let bytes = std::fs::metadata(&golden).expect("golden png").len();
        assert!(bytes < 100 * 1024, "golden {golden} must stay under 100 KB");
    }
}

fn snapshot_both(state: &app::State, stem: &str) {
    snapshot_sized(state, stem, 470.0);
}

fn snapshot_sized(state: &app::State, stem: &str, height: f32) {
    for lang in ["en", "ru"] {
        agentdeck_gui::i18n::set_language(lang);
        let mut ui = iced_test::Simulator::with_size(
            iced::Settings::default(),
            (860.0, height),
            app::view(state),
        );
        let snapshot = ui
            .snapshot(&agentdeck_gui::style::theme(true))
            .expect("headless snapshot");
        assert_snapshot(&snapshot, &format!("{stem}_{lang}"));
    }
    agentdeck_gui::i18n::set_language("en");
}

struct Fixture {
    dir: PathBuf,
    paths: Paths,
    _env: std::sync::MutexGuard<'static, ()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// HOME, the T3 dir and XDG state are process-global: hold the lock while set.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn fixture(tag: &str) -> Fixture {
    let env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("agentdeck-gui-g7-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("home")).unwrap();
    unsafe {
        std::env::set_var("HOME", dir.join("home"));
        std::env::set_var("AGENTDECK_T3_DIR", dir.join("t3"));
        std::env::set_var("AGENTDECK_PROC_DIR", dir.join("proc"));
        std::env::set_var("XDG_STATE_HOME", dir.join("xdg"));
    }
    agentdeck_gui::i18n::set_language("en");
    std::fs::create_dir_all(SNAPSHOTS).unwrap();
    Fixture {
        paths: Paths::new(dir.join("config"), dir.join("state")),
        dir,
        _env: env,
    }
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn add_instance(paths: &Paths, id: &str, harness: agentdeck_core::Harness, home: &str) {
    Registry::update(paths, |r| {
        r.add(agentdeck_core::AddSpec {
            id: id.into(),
            harness,
            display_name: None,
            home: home.into(),
            binary: None,
            no_t3: false,
        })?;
        Ok(())
    })
    .unwrap();
}

fn update(state: &mut app::State, message: Message) {
    let _ = app::update(state, message);
}

#[test]
fn pool_page_catalog_search_sort_and_apply() {
    let fx = fixture("pool");
    add_instance(
        &fx.paths,
        "claude-main",
        agentdeck_core::Harness::Claude,
        "/fake/c",
    );
    add_instance(
        &fx.paths,
        "codex-second",
        agentdeck_core::Harness::Codex,
        "/fake/x",
    );
    Registry::update(&fx.paths, |r| {
        r.add_pool(PoolEntry {
            instance: "claude-main".into(),
            model: "sonnet-5".into(),
            options: Default::default(),
            enabled: true,
        })?;
        r.add_pool(PoolEntry {
            instance: "codex-second".into(),
            model: "gpt-5".into(),
            options: [(
                "effort".to_string(),
                agentdeck_core::PoolValue::String("high".into()),
            )]
            .into(),
            enabled: false,
        })?;
        Ok(())
    })
    .unwrap();
    let t3 = fx.dir.join("t3");
    write(
        &t3.join("caches/claude-main.json"),
        r#"{"checkedAt":"2026-10-30","models":[{"slug":"sonnet-5"},{"slug":"opus-5"}]}"#,
    );
    write(
        &t3.join("userdata/settings.json"),
        r#"{"favorites":[{"provider":"claude-main","model":"opus-5"}],
            "providerInstances":{"claude-main":{"enabled":true}}}"#,
    );
    let ctx = pool::Ctx {
        paths: fx.paths.clone(),
        t3_root: Some(t3),
    };
    let mut state = app::State::for_tests(fx.paths.clone(), Default::default());
    update(&mut state, Message::SelectPage(Page::Pool));
    update(&mut state, Message::Pool(pool::load(&ctx)));
    update(
        &mut state,
        Message::Pool(pool::Msg::Instance("claude-main".into())),
    );
    update(&mut state, Message::Pool(pool::Msg::Model("opus-5".into())));
    update(&mut state, Message::Pool(pool::Msg::OpenAdd));
    snapshot_both(&state, "pool_form");

    // Add: the preview lists the registry change; nothing is written yet.
    update(&mut state, Message::Pool(pool::Msg::AskAdd));
    snapshot_both(&state, "pool_preview");
    let mut ui = iced_test::simulator(app::view(&state));
    ui.find("Apply").expect("apply button");
    ui.click("Cancel").expect("cancel button");
    for message in ui.into_messages() {
        update(&mut state, message);
    }
    assert_eq!(Registry::load(&fx.paths).unwrap().pool.len(), 2);

    // A duplicate is refused in the panel and offers no Apply.
    update(
        &mut state,
        Message::Pool(pool::Msg::Model("sonnet-5".into())),
    );
    update(&mut state, Message::Pool(pool::Msg::AskAdd));
    {
        let mut ui = iced_test::simulator(app::view(&state));
        ui.find("Cancel").expect("panel stays for a refusal");
        assert!(ui.find("Apply").is_ok(), "button renders but is disabled");
    }
    update(&mut state, Message::Pool(pool::Msg::Cancel));

    let pool::Msg::Loaded(loaded) = pool::load(&ctx) else {
        panic!("loaded")
    };
    assert_eq!(loaded.entries.len(), 3);
    let mut page = pool::State {
        data: loaded,
        ..Default::default()
    };
    page.search = "OPUS".into();
    let (index, entry) = pool::visible_entries(&page)[0];
    assert_eq!(entry.model, "opus-5");
    assert!(!entry.enabled);
    pool::apply(&ctx, &pool::Op::Toggle(page.data.entries[index].clone())).unwrap();
    assert!(
        Registry::load(&fx.paths)
            .unwrap()
            .pool
            .iter()
            .any(|e| e.model == "opus-5" && e.enabled)
    );
    page.search = "gpt".into();
    page.filter_instance = Some("claude-main".into());
    assert!(pool::visible_entries(&page).is_empty());
    page.search.clear();
    page.filter_instance = None;
    page.sort = pool::Sort::Enabled;
    assert!(pool::visible_entries(&page)[0].1.enabled);
    let mut ui = iced_test::simulator(app::view(&state));
    assert!(ui.find("Import T3 favorites").is_err());
}

#[test]
fn shared_page_status_preview_and_apply() {
    let fx = fixture("shared");
    let home = fx.dir.join("home");
    write(&home.join(".general/skills/alpha/SKILL.md"), "alpha");
    std::fs::create_dir_all(home.join(".claude/skills")).unwrap();
    write(&home.join("AGENTS.md"), "shared instructions");
    write(
        &home.join(".claude.json"),
        r#"{"mcpServers":{"context7":{"command":"npx","env":{"TOKEN":"supersecret"}}}}"#,
    );
    write(&home.join(".claude-b/.claude.json"), "{}");
    write(&home.join(".codex/config.toml"), "model = \"x\"\n");
    add_instance(
        &fx.paths,
        "claude-b",
        agentdeck_core::Harness::Claude,
        "~/.claude-b",
    );
    let ctx = shared::Ctx {
        paths: fx.paths.clone(),
        home: home.clone(),
    };
    let mut state = app::State::for_tests(fx.paths.clone(), Default::default());
    update(&mut state, Message::SelectPage(Page::Shared));
    update(&mut state, Message::Shared(shared::load(&ctx)));
    snapshot_sized(&state, "shared", 360.0);
    update(
        &mut state,
        Message::Shared(shared::Msg::Tab(shared::Kind::Mcp)),
    );
    {
        let mut ui = iced_test::simulator(app::view(&state));
        ui.find("context7").expect("canonical server name");
    }

    // Skills preview (deterministic; MCP backup paths carry timestamps).
    update(
        &mut state,
        Message::Shared(shared::Msg::Ask(shared::Kind::Skills)),
    );
    snapshot_sized(&state, "shared_skills_preview", 290.0);
    update(&mut state, Message::Shared(shared::Msg::Cancel));

    // Skills: preview, then apply exactly the previewed changes.
    let mut sh = shared::State::default();
    let _ = shared::update(&mut sh, ctx.clone(), shared::Msg::Ask(shared::Kind::Skills));
    let pending = sh.pending.expect("skills preview");
    assert!(!pending.changes.is_empty());
    assert!(
        !home.join(".claude/skills/alpha").exists(),
        "preview must not write"
    );
    agentdeck_core::apply_changes(&pending.changes).unwrap();
    assert!(home.join(".claude/skills/alpha").exists());

    let mut sh = shared::State::default();
    let _ = shared::update(&mut sh, ctx, shared::Msg::Ask(shared::Kind::Mcp));
    let text = sh.pending.expect("mcp preview").lines.join("\n");
    assert!(
        text.contains(".claude.json"),
        "claude secondary change planned: {text}"
    );
    assert!(!text.contains("supersecret"), "{text}");
}

#[test]
fn harnesses_page_shows_exact_command() {
    let fx = fixture("harnesses");
    let status = |name: &str, bin: Option<&str>, version: Option<&str>, method: &str| Status {
        name: name.into(),
        binary: bin.map(PathBuf::from),
        version: version.map(str::to_string),
        install_method: method.into(),
    };
    let rows = vec![
        status(
            "claude",
            Some("/fake/bin/claude"),
            Some("2.1.0"),
            "native installer (`claude update`)",
        ),
        status("codex", None, None, "standalone installer (`codex update`)"),
    ];
    let mut state = app::State::for_tests(fx.paths.clone(), Default::default());
    update(&mut state, Message::SelectPage(Page::Harnesses));
    update(&mut state, Message::Harnesses(harnesses::Msg::Loaded(rows)));
    {
        let mut ui = iced_test::simulator(app::view(&state));
        ui.find("Update")
            .expect("update button for the installed harness");
        ui.find("Install")
            .expect("install button for the missing one");
    }
    snapshot_both(&state, "harnesses");

    update(
        &mut state,
        Message::Harnesses(harnesses::Msg::Ask {
            name: "claude".into(),
            install: false,
        }),
    );
    {
        let mut ui = iced_test::simulator(app::view(&state));
        ui.find("+ /fake/bin/claude update")
            .expect("exact update command");
    }
    snapshot_both(&state, "harnesses_confirm");
    update(&mut state, Message::Harnesses(harnesses::Msg::Cancel));
    update(
        &mut state,
        Message::Harnesses(harnesses::Msg::Ask {
            name: "codex".into(),
            install: true,
        }),
    );
    let mut ui = iced_test::simulator(app::view(&state));
    ui.find("+ sh -c curl -fsSL https://chatgpt.com/codex/install.sh | sh")
        .expect("exact install command");
}

#[test]
fn wizard_opencode_and_agy() {
    let fx = fixture("wizard");
    let mut state = app::State::for_tests(fx.paths.clone(), Default::default());
    update(&mut state, Message::SelectPage(Page::AddInstance));
    update(&mut state, Message::Add(AddMsg::Id("second".into())));
    update(
        &mut state,
        Message::Add(AddMsg::Harness(HarnessChoice::Opencode)),
    );
    assert_eq!(
        agentdeck_gui::pages::add_instance::suggest_home(HarnessChoice::Opencode, "second"),
        "~/.opencode-second/opencode"
    );
    // Registry-only add: the preview is just the registry change.
    let spec = agentdeck_core::AddSpec {
        id: "second".into(),
        harness: agentdeck_core::Harness::Opencode,
        display_name: None,
        home: "~/.opencode-second/opencode".into(),
        binary: None,
        no_t3: false,
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let lines = rt
        .block_on(agentdeck_gui::pages::add_instance::apply_add(
            fx.paths.clone(),
            spec,
        ))
        .unwrap();
    assert!(
        lines.iter().any(|l| l.contains("instances.toml")),
        "{lines:?}"
    );
    assert_eq!(
        Registry::load(&fx.paths)
            .unwrap()
            .get("second")
            .unwrap()
            .harness,
        agentdeck_core::Harness::Opencode
    );

    // agy: no login CLI, the panel shows the manual step instead.
    update(
        &mut state,
        Message::Add(AddMsg::Harness(HarnessChoice::Agy)),
    );
    update(
        &mut state,
        Message::Add(AddMsg::ApplyDone(Ok(vec!["added second".into()]))),
    );
    {
        let mut ui = iced_test::simulator(app::view(&state));
        ui.find(
            "agy has no login CLI: sign in with your Google account in the Antigravity IDE. It is a single account per OS user.",
        )
        .expect("agy manual step");
        assert!(ui.find("Start login").is_err(), "agy has no login button");
    }
}
