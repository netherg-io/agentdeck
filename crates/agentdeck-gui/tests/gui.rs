//! Headless GUI tests (never a window): fixture registry + usage state files,
//! EN/RU snapshot PNGs via iced_test, plus sidebar navigation.
//!
//! Snapshots live in tests/snapshots/ and are committed only while stable and
//! small; `matches_image` creates them on first run and compares afterwards.

use std::path::{Path, PathBuf};

use agentdeck_core::{Paths, Registry};
use agentdeck_gui::{app, data, pages::drift};

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

fn fixture_paths(tag: &str) -> (Paths, PathBuf, tempfile_guard::Guard) {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "agentdeck-gui-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    // Isolate the legacy fallback (`$XDG_STATE_HOME/ai-usage/state.json`) from
    // the real user tree; callers hold ENV_LOCK while this env is set.
    unsafe {
        std::env::set_var("XDG_STATE_HOME", dir.join("xdg"));
    }
    let _ = std::fs::remove_dir_all(&dir);
    let paths = Paths::new(dir.join("config"), dir.join("state"));
    (paths, dir.clone(), tempfile_guard::Guard(dir))
}

// Minimal RAII dir guard (tempfile is not a dependency; keep it that way).
mod tempfile_guard {
    use std::path::PathBuf;
    pub struct Guard(pub PathBuf);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

fn write_registry(paths: &Paths) {
    Registry::update(paths, |r| {
        r.add(agentdeck_core::registry::AddSpec {
            id: "claude-main".into(),
            harness: agentdeck_core::Harness::Claude,
            display_name: Some("Claude main".into()),
            home: "/fake/claude".into(),
            binary: None,
            no_t3: false,
        })?;
        r.add(agentdeck_core::registry::AddSpec {
            id: "codex-second".into(),
            harness: agentdeck_core::Harness::Codex,
            display_name: None,
            home: "/fake/codex-second".into(),
            binary: None,
            no_t3: false,
        })?;
        r.add_usage(agentdeck_core::UsageEntry {
            id: "claude-main".into(),
            kind: agentdeck_core::UsageKind::Claude,
            instance: Some("claude-main".into()),
            display: "Claude main".into(),
            config_dir: Some("~/.claude".into()),
            codex_home: None,
            token_file: None,
            api_key_env: None,
            db_file: None,
            extra: Default::default(),
        })?;
        r.add_usage(agentdeck_core::UsageEntry {
            id: "openrouter-main".into(),
            kind: agentdeck_core::UsageKind::Openrouter,
            instance: None,
            display: "OpenRouter".into(),
            config_dir: None,
            codex_home: None,
            token_file: None,
            api_key_env: Some("AGENTDECK_TEST_KEY".into()),
            db_file: None,
            extra: Default::default(),
        })?;
        r.set_enabled("codex-second", false)?;
        Ok(())
    })
    .unwrap();
}

fn write_state(paths: &Paths) {
    let dir = paths.state_dir.join("usage");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("state.json"),
        serde_json::json!({
            "schema_version": 2,
            "updated_at": 1_769_000_000,
            "accounts": [
                {
                    "id": "claude-main", "name": "Claude main", "provider": "claude",
                    "status": "ok", "plan": "Max 5x", "email": null, "model": null,
                    "windows": [
                        {"key": "five_hour", "label": "5 hours",
                         "used_percent": 35.0, "remaining_percent": 65.0,
                         "duration_mins": 300, "resets_at": null, "scope": null},
                        {"key": "seven_day", "label": "Week",
                         "used_percent": 72.5, "remaining_percent": 27.5,
                         "duration_mins": 10080, "resets_at": null, "scope": null}
                    ],
                    "balances": [], "error": null, "updated_at": 1_769_000_000
                },
                {
                    "id": "openrouter-main", "name": "OpenRouter", "provider": "openrouter",
                    "status": "error", "plan": null, "email": null, "model": null,
                    "windows": [], "balances": [],
                    "error": "no key configured", "updated_at": 1_769_000_000
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
}

fn write_t3(dir: &Path) {
    let t3 = dir.join("t3");
    std::fs::create_dir_all(t3.join("userdata")).unwrap();
    std::fs::create_dir_all(t3.join("caches")).unwrap();
    std::fs::write(
        t3.join("userdata/settings.json"),
        r#"{"providerInstances": {
            "claude-main": {"driver": "claudeAgent", "enabled": true,
                "config": {"homePath": "/fake/claude"}},
            "codex-second": {"driver": "codex", "enabled": false,
                "config": {"homePath": "/fake/codex-second"}}
        }}"#,
    )
    .unwrap();
    std::fs::write(
        t3.join("caches/claude-main.json"),
        r#"{"status": "ready", "checkedAt": "2026-10-24T00:00:00.000Z", "models": []}"#,
    )
    .unwrap();
    // Point the T3 root at the fixture (restored by the caller).
    unsafe {
        std::env::set_var("AGENTDECK_T3_DIR", &t3);
    }
}

fn load_with_fixtures(
    tag: &str,
    _env: &std::sync::MutexGuard<'_, ()>,
) -> (data::Data, Paths, tempfile_guard::Guard) {
    let (paths, dir, guard) = fixture_paths(tag);
    write_registry(&paths);
    write_state(&paths);
    write_t3(&dir);
    let data = data::load(&paths);
    (data, paths, guard)
}

/// `AGENTDECK_T3_DIR` is process-global: tests that point it at fixtures
/// hold this lock for their whole body so parallel tests cannot race.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    // A poisoned lock means a sibling test panicked: keep going so every
    // failure prints instead of cascading into PoisonError noise.
    let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    unsafe {
        std::env::remove_var("AGENTDECK_T3_DIR");
    }
    guard
}

#[test]
fn instances_data_links_registry_t3_and_state() {
    let env = lock_env();
    let (data, _paths, _guard) = load_with_fixtures("data", &env);
    assert_eq!(data.instances.len(), 2);
    let claude = &data.instances[0];
    assert_eq!(claude.display_name, "Claude main");
    assert!(claude.enabled);
    assert_eq!(claude.t3.as_ref().unwrap().status.as_deref(), Some("ready"));
    assert_eq!(claude.quotas.len(), 1);
    assert_eq!(claude.quotas[0].bars.len(), 2);
    assert!((claude.quotas[0].bars[0].used_percent - 35.0).abs() < f64::EPSILON);
    let codex = &data.instances[1];
    assert!(!codex.enabled);
    assert!(codex.t3.as_ref().unwrap().status.is_none());
    assert!(codex.quotas.is_empty());
    assert_eq!(data.standalone.len(), 1);
    assert_eq!(
        data.standalone[0].error.as_deref(),
        Some("no key configured")
    );
}

#[test]
fn missing_state_file_is_no_data_not_an_error() {
    let _env = lock_env();
    let (paths, _dir, _guard) = fixture_paths("nodata");
    // Fixture T3 dir exists but is empty: no settings.json means "no T3".
    unsafe {
        std::env::set_var("AGENTDECK_T3_DIR", _dir.join("t3"));
    }
    write_registry(&paths);
    let data = data::load(&paths);
    assert!(data.error.is_none());
    assert!(data.instances[0].quotas[0].bars.is_empty());
    assert!(data.instances[0].t3.is_none());
}

#[test]
fn snapshots_en_ru_and_sidebar_navigation() {
    let _env = lock_env();
    let (data, paths, _guard) = load_with_fixtures("shots", &_env);
    std::fs::create_dir_all(SNAPSHOTS).unwrap();

    for (lang, theme, stem) in [
        ("en", agentdeck_gui::style::theme(true), "instances_en"),
        (
            "en",
            agentdeck_gui::style::theme(false),
            "instances_en_light",
        ),
        ("ru", agentdeck_gui::style::theme(true), "instances_ru"),
        (
            "ru",
            agentdeck_gui::style::theme(false),
            "instances_ru_light",
        ),
    ] {
        agentdeck_gui::i18n::set_language(lang);
        let state = app::State::for_tests(paths.clone(), data.clone());
        // Smaller than the default 1024x768 keeps goldens under 100 KB.
        let mut ui = iced_test::Simulator::with_size(
            iced::Settings::default(),
            (760.0, 360.0),
            app::view(&state),
        );
        let snapshot = ui.snapshot(&theme).expect("headless snapshot");
        // Golden path gains a `-{renderer}` suffix inside matches_image;
        // exactly one file per stem must exist afterwards.
        assert_snapshot(&snapshot, stem);
        let goldens: Vec<_> = std::fs::read_dir(SNAPSHOTS)
            .expect("snapshots dir")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_stem().and_then(|s| s.to_str()) == Some(format!("{stem}-wgpu").as_str())
                    && p.extension().and_then(|e| e.to_str()) == Some("png")
            })
            .collect();
        assert_eq!(goldens.len(), 1, "one golden per stem: {goldens:?}");
        let bytes = std::fs::metadata(&goldens[0]).expect("golden png").len();
        assert!(bytes > 1_024, "golden {} looks empty", goldens[0].display());
        assert!(
            bytes < 100 * 1024,
            "golden {} over 100 KB",
            goldens[0].display()
        );
    }

    // Clicking only emits an instance-specific action; the test never launches a terminal.
    let mut single = data.clone();
    single.instances.truncate(1);
    let id = single.instances[0].id.clone();
    let state = app::State::for_tests(paths.clone(), single);
    let mut ui = iced_test::simulator(app::view(&state));
    ui.click("CLI").expect("instance CLI button");
    assert!(
        ui.into_messages()
            .any(|message| matches!(message, app::Message::LaunchCli(found) if found == id))
    );

    // Sidebar navigation: every page reachable by clicking.
    agentdeck_gui::i18n::set_language("en");
    let mut state = app::State::for_tests(paths, data);
    for (button, page) in [
        ("Drift", agentdeck_gui::Page::Drift),
        ("Sources", agentdeck_gui::Page::UsageSources),
        ("Limits", agentdeck_gui::Page::Limits),
        ("Processes", agentdeck_gui::Page::Processes),
        ("Instances", agentdeck_gui::Page::Instances),
    ] {
        let mut ui = iced_test::simulator(app::view(&state));
        ui.click(button).expect("nav button");
        for message in ui.into_messages() {
            let _ = app::update(&mut state, message);
        }
        assert_eq!(state.page(), page);
    }
    // AddInstance is reached from the Instances page button, not the sidebar.
    let mut ui = iced_test::simulator(app::view(&state));
    ui.click("Add").expect("add instance button");
    for message in ui.into_messages() {
        let _ = app::update(&mut state, message);
    }
    assert!(matches!(state.page(), agentdeck_gui::Page::AddInstance));
    {
        let mut ui = iced_test::simulator(app::view(&state));
        ui.find("Add instance").expect("add page title");
    }

    // The shared confirm panel (remove dialog) must lay out in both themes.
    let _ = app::update(
        &mut state,
        app::Message::SelectPage(agentdeck_gui::Page::Instances),
    );
    let _ = app::update(&mut state, app::Message::RemoveAsk("claude-main".into()));
    for theme in [
        &agentdeck_gui::style::theme(true),
        &agentdeck_gui::style::theme(false),
    ] {
        agentdeck_gui::i18n::set_language("en");
        let mut ui = iced_test::simulator(app::view(&state));
        ui.snapshot(theme).expect("panel renders");
        ui.find("The login dir is kept.").expect("remove panel");
    }
    agentdeck_gui::i18n::set_language("en");
}

/// Drift fixture: fake HOME + registry + T3 dir with real drift in every
/// group (nothing touches the real user tree; `Ctx` carries temp roots).
fn write_drift_fixtures(tag: &str) -> (drift::Ctx, tempfile_guard::Guard) {
    let (paths, dir, guard) = fixture_paths(tag);
    let home = dir.join("home");
    // Primary Claude profile.
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::write(home.join(".claude/settings.json"), "{\"theme\":\"dark\"}\n").unwrap();
    std::fs::write(
        home.join(".claude.json"),
        "{\"mcpServers\":{\"srv\":{\"command\":\"x\"}}}",
    )
    .unwrap();
    // Secondary with settings + mcp drift (no symlinks: link drift too).
    let second = home.join(".claude-second");
    std::fs::create_dir_all(&second).unwrap();
    std::fs::write(second.join("settings.json"), "{\"theme\":\"light\"}\n").unwrap();
    std::fs::write(
        second.join(".claude.json"),
        "{\"mcpServers\":{\"srv\":{\"command\":\"x\"},\"extra\":{\"command\":\"y\"}}}",
    )
    .unwrap();
    // Codex main + shadow with a real entry where a link belongs.
    let main = home.join(".codex");
    std::fs::create_dir_all(&main).unwrap();
    std::fs::write(main.join("config.toml"), "x = 1\n").unwrap();
    let shadow = home.join(".codex-shadow");
    std::fs::create_dir_all(&shadow).unwrap();
    std::fs::write(shadow.join("config.toml"), "mine").unwrap();
    // Registry: claude + primary/secondary codex homes.
    Registry::update(&paths, |r| {
        r.add(agentdeck_core::registry::AddSpec {
            id: "claude-a".into(),
            harness: agentdeck_core::Harness::Claude,
            display_name: None,
            home: second.to_string_lossy().into_owned(),
            binary: None,
            no_t3: false,
        })?;
        r.add(agentdeck_core::registry::AddSpec {
            id: "codex".into(),
            harness: agentdeck_core::Harness::Codex,
            display_name: None,
            home: "~/.codex".into(),
            binary: None,
            no_t3: false,
        })?;
        r.add(agentdeck_core::registry::AddSpec {
            id: "codex-s".into(),
            harness: agentdeck_core::Harness::Codex,
            display_name: None,
            home: shadow.to_string_lossy().into_owned(),
            binary: None,
            no_t3: false,
        })?;
        Ok(())
    })
    .unwrap();
    // T3: registry instance out of sync + one instance missing from registry.
    let t3 = dir.join("t3");
    std::fs::create_dir_all(t3.join("userdata")).unwrap();
    std::fs::create_dir_all(t3.join("caches")).unwrap();
    std::fs::write(
        t3.join("userdata/settings.json"),
        r#"{"providerInstances": {
            "claude-a": {"driver": "claudeAgent", "enabled": true,
                "config": {"homePath": "/elsewhere"}},
            "codex-extra": {"driver": "codex", "enabled": true,
                "config": {"homePath": "/x", "shadowHomePath": "/y"}}
        }}"#,
    )
    .unwrap();
    let ctx = drift::Ctx {
        paths,
        home,
        t3_root: Some(t3),
    };
    (ctx, guard)
}

fn drift_group<'a>(snap: &'a drift::Snapshot, id: &str) -> &'a drift::Group {
    snap.groups.iter().find(|g| g.id == id).expect(id)
}

/// Trim a rich drift fixture down to a compact deterministic page: one item
/// per kind. Temp paths are normalized by the caller.
fn tidy_for_shot(ctx: &drift::Ctx) {
    let primary = ctx.home.join(".claude");
    let second = ctx.home.join(".claude-second");
    for name in ["skills", "agents", "plugins", "CLAUDE.md"] {
        let link = second.join(name);
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(primary.join(name), &link).unwrap();
    }
    std::fs::write(second.join("settings.json"), "{\"theme\":\"light\"}\n").unwrap();
    std::fs::copy(ctx.home.join(".claude.json"), second.join(".claude.json")).unwrap();
    // Sync the shadow, then plant a single real entry.
    let main = ctx.home.join(".codex");
    let shadow = ctx.home.join(".codex-shadow");
    let changes =
        agentdeck_core::codex::plan_materialize(&main, &shadow, &ctx.paths.state_dir).unwrap();
    agentdeck_core::apply_changes(&changes).unwrap();
    std::fs::remove_file(shadow.join("config.toml")).unwrap();
    std::fs::write(shadow.join("config.toml"), "mine").unwrap();
    // Apply the T3 sync so the golden keeps a single T3 row (the importable
    // instance); the file drift above stays for the Claude/Codex groups.
    let t3_root = ctx.t3_root.clone().unwrap();
    agentdeck_core::t3::sync_t3(&ctx.paths, &t3_root, false).unwrap();
}

/// Rendered text holds temp paths (run-specific); normalize them away.
fn normalize_shot(state: &mut drift::State, root: &str) {
    for group in &mut state.snapshot.groups {
        for item in &mut group.items {
            item.text = item.text.replace(root, "<tmp>");
            if let Some(p) = item.decision_path.take() {
                item.decision_path = Some(p.replace(root, "<tmp>"));
            }
            for a in &mut item.actions {
                a.key = a.key.replace(root, "<tmp>");
            }
        }
    }
}

#[test]
fn drift_snapshot_groups_actions_preview_apply() {
    let _env = lock_env();
    let (ctx, _guard) = write_drift_fixtures("drift-act");
    let snap = drift::snapshot(&ctx);
    assert!(snap.error.is_none(), "{:?}", snap.error);

    // Claude: symlink + settings + mcp drift, Apply and Import offered.
    let claude = drift_group(&snap, "claude-a");
    assert!(claude.items.iter().any(|i| {
        i.actions
            .iter()
            .any(|a| a.kind == drift::Kind::Import && a.key == "settings:theme")
    }));
    assert!(claude.items.iter().any(|i| {
        i.actions
            .iter()
            .any(|a| a.kind == drift::Kind::Import && a.key == "mcpserver:extra")
    }));
    // Redacted UI text never leaks env/header values (fixture has none, but
    // the marker must be there for the mcp diff).
    let mcp = claude
        .items
        .iter()
        .find(|i| i.key == "mcp")
        .expect("mcp item");
    assert!(
        mcp.text.contains("<redacted>") || mcp.text.contains("extra"),
        "{}",
        mcp.text
    );

    // Codex: real entry needs a decision; the fix confirm names the path.
    let codex = drift_group(&snap, "codex-s");
    let real = codex
        .items
        .iter()
        .find(|i| i.decision_path.is_some())
        .expect("real entry item");
    let fix = real
        .actions
        .iter()
        .find(|a| a.kind == drift::Kind::Fix)
        .expect("fix");
    let pending = drift::preview(&ctx, "codex-s", &fix.key, drift::Kind::Fix).expect("preview");
    assert_eq!(
        pending.confirm_path.as_deref(),
        real.decision_path.as_deref(),
        "the Fix confirm names the path"
    );
    assert!(!pending.preview.is_empty());

    // Import a Claude settings key through the page: preview then apply.
    let pending =
        drift::preview(&ctx, "claude-a", "settings:theme", drift::Kind::Import).expect("preview");
    assert!(!pending.preview.is_empty(), "change-list preview");
    let notice = drift::apply(&ctx, &pending);
    assert!(notice.contains("imported"), "{notice}");
    // The key no longer drifts afterwards.
    let snap = drift::snapshot(&ctx);
    let claude = drift_group(&snap, "claude-a");
    assert!(
        claude
            .items
            .iter()
            .all(|i| !i.actions.iter().any(|a| a.key == "settings:theme")),
        "theme imported, no Import left"
    );

    assert!(
        snap.groups.iter().all(|group| group.id != "t3"),
        "provider inventory is outside the drift page"
    );
}

#[test]
fn drift_fix_then_reenable_offer_and_snapshots() {
    let _env = lock_env();
    let (ctx, _guard) = write_drift_fixtures("drift-fix");
    // Fix the shadow through the page (backup+replace after the path confirm).
    let snap = drift::snapshot(&ctx);
    let codex = drift_group(&snap, "codex-s");
    let real = codex
        .items
        .iter()
        .find(|i| i.decision_path.is_some())
        .expect("real");
    let fix = real
        .actions
        .iter()
        .find(|a| a.kind == drift::Kind::Fix)
        .expect("fix")
        .clone();
    let pending = drift::preview(&ctx, "codex-s", &fix.key, drift::Kind::Fix).expect("preview");
    let notice = drift::apply(&ctx, &pending);
    assert!(notice.starts_with("fixed "), "{notice}");
    // Simulate update()'s bookkeeping: a fix on a T3 instance offers Re-enable.
    let mut state = drift::State::default();
    state.reenable.push("codex-s".into());
    state.snapshot = drift::snapshot(&ctx);
    assert!(
        drift_group(&state.snapshot, "codex-s")
            .items
            .iter()
            .all(|i| i.decision_path.is_none()),
        "real entry linked now"
    );
    // Lean deterministic page for the golden snapshots.
    tidy_for_shot(&ctx);
    state.snapshot = drift::snapshot(&ctx);
    state.reenable.clear();
    let root = ctx.home.parent().unwrap().to_string_lossy().into_owned();
    normalize_shot(&mut state, &root);
    // update()'s Done bookkeeping offers Re-enable after a fix on a T3 instance.
    let mut st = drift::State::default();
    let _task = drift::update(
        &mut st,
        ctx.clone(),
        drift::Message::Done {
            notice: "fixed codex-s (0 deferred)".into(),
        },
    );
    assert!(st.reenable.contains(&"codex-s".to_string()));

    // Headless EN/RU snapshots of the page with fixture drift.
    std::fs::create_dir_all(SNAPSHOTS).unwrap();
    for (lang, stem) in [("en", "drift_en"), ("ru", "drift_ru")] {
        agentdeck_gui::i18n::set_language(lang);
        let mut ui = iced_test::simulator(drift::view(&state));
        let snapshot = ui
            .snapshot(&agentdeck_gui::style::theme(true))
            .expect("headless snapshot");
        assert_snapshot(&snapshot, stem);
        let goldens: Vec<_> = std::fs::read_dir(SNAPSHOTS)
            .expect("snapshots dir")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s.starts_with(stem))
                    && p.extension().and_then(|e| e.to_str()) == Some("png")
            })
            .collect();
        assert_eq!(goldens.len(), 1, "one golden per stem: {goldens:?}");
        let bytes = std::fs::metadata(&goldens[0]).expect("golden png").len();
        assert!(bytes > 1_024, "golden {} looks empty", goldens[0].display());
        assert!(
            bytes < 100_000,
            "golden {} over the 100KB budget",
            goldens[0].display()
        );
    }
    agentdeck_gui::i18n::set_language("en");
}

/// Module switches: the Modules page with every status card, enable preview + cancel, EN/RU snapshots.
///
/// HOME and XDG_CONFIG_HOME point at a temp dir so the status/probe reads are
/// deterministic (launcher missing, unit not installed) and read-only.
#[test]
fn module_switch_preview_cancel_and_snapshots() {
    let env = lock_env();
    // Deterministic HOME *before* loading: status reads stay inside the
    // temp dir (launcher missing, unit not installed) and are read-only.
    static HOME_N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let fakehome = std::env::temp_dir().join(format!(
        "agentdeck-gui-plughome-{}-{}",
        std::process::id(),
        HOME_N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&fakehome);
    std::fs::create_dir_all(&fakehome).unwrap();
    let old_home = std::env::var_os("HOME");
    let old_xdg = std::env::var_os("XDG_CONFIG_HOME");
    let old_systemctl = std::env::var_os("AGENTDECK_SYSTEMCTL");
    unsafe {
        std::env::set_var("HOME", &fakehome);
        std::env::set_var("XDG_CONFIG_HOME", fakehome.join("config"));
        // No real systemctl probes: nothing is ever installed or active here.
        std::env::set_var("AGENTDECK_SYSTEMCTL", "/bin/false");
    }
    let (data, paths, _guard) = load_with_fixtures("plugins", &env);
    assert_eq!(data.plugins.len(), 3);
    std::fs::create_dir_all(SNAPSHOTS).unwrap();
    let home = fakehome;

    // System → Modules lists every module's status card; snapshot EN + RU.
    agentdeck_gui::i18n::set_language("en");
    let mut state = app::State::for_tests(paths.clone(), data.clone());
    let _ = app::update(
        &mut state,
        app::Message::SelectPage(agentdeck_gui::Page::Modules),
    );
    {
        let mut ui = iced_test::simulator(app::view(&state));
        for name in ["ai-usage", "router", "agent-process"] {
            ui.find(name).expect("module status card");
        }
    }
    let _ = app::update(&mut state, app::Message::PluginDetail("router".into()));
    for (lang, stem) in [("en", "modules_en"), ("ru", "modules_ru")] {
        agentdeck_gui::i18n::set_language(lang);
        let mut ui = iced_test::Simulator::with_size(
            iced::Settings::default(),
            (960.0, 600.0),
            app::view(&state),
        );
        let snapshot = ui
            .snapshot(&agentdeck_gui::style::theme(true))
            .expect("headless snapshot");
        assert_snapshot(&snapshot, stem);
        let golden = std::fs::read_dir(SNAPSHOTS)
            .expect("snapshots dir")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .find(|p| {
                p.file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s.starts_with(stem))
                    && p.extension().and_then(|e| e.to_str()) == Some("png")
            })
            .expect("golden written");
        let bytes = std::fs::metadata(&golden).expect("golden png").len();
        assert!(bytes > 1_024, "golden {} looks empty", golden.display());
        assert!(bytes < 160 * 1024, "golden {} too big", golden.display());
    }

    // Enable flow: the preview lists changes, Cancel drops it without writes.
    agentdeck_gui::i18n::set_language("en");
    // The sidebar switch sends this; its preview opens on any page.
    let _ = app::update(
        &mut state,
        app::Message::EnablePlugin("agent-process".into()),
    );
    let mut ui = iced_test::simulator(app::view(&state));
    ui.find("Confirm").expect("preview confirm button");
    ui.click("Cancel").expect("preview cancel button");
    for message in ui.into_messages() {
        let _ = app::update(&mut state, message);
    }
    // Nothing was applied: the link was never created, the registry untouched.
    assert!(!home.join(".local/bin/agent-process").exists());
    let raw = std::fs::read_to_string(paths.registry_file()).unwrap();
    assert!(!raw.contains("[plugins]"), "{raw}");
    let mut ui = iced_test::simulator(app::view(&state));
    assert!(ui.find("Confirm").is_err(), "preview cleared after cancel");

    unsafe {
        match old_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match old_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        match old_systemctl {
            Some(v) => std::env::set_var("AGENTDECK_SYSTEMCTL", v),
            None => std::env::remove_var("AGENTDECK_SYSTEMCTL"),
        }
    }
    let _ = std::fs::remove_dir_all(&home);
}

fn fixture_proc(pid: u32, cmd: &str) -> agentdeck_core::processes::ProcInfo {
    agentdeck_core::processes::ProcInfo {
        pid,
        ppid: 1,
        pgrp: pid,
        comm: cmd.to_string(),
        argv: vec![cmd.to_string(), "60".to_string()],
        cpu_percent: 0.5,
        rss_bytes: 2_000_000,
        read_bytes: Some(4096),
        write_bytes: Some(0),
        start_ticks: 1000,
        scope: None,
        owner_tag: Some("ses1".to_string()),
        container: None,
    }
}

fn fixture_groups() -> Vec<agentdeck_core::processes::SessionGroup> {
    use agentdeck_core::processes::{GroupKind, SessionGroup};
    vec![
        SessionGroup {
            kind: GroupKind::Agent {
                owner: "ses1".to_string(),
                id: "abc".to_string(),
            },
            title: "ses1 (abc)".to_string(),
            pid: 100,
            start_ticks: 1000,
            pgrp: 100,
            can_stop: true,
            processes: vec![fixture_proc(100, "sleep"), fixture_proc(101, "sh")],
            cpu_percent: 1.0,
            rss_bytes: 4_000_000,
            read_bytes: Some(8192),
            write_bytes: Some(0),
            scope_mem_bytes: None,
            container: None,
        },
        SessionGroup {
            kind: GroupKind::T3 {
                scope: "t3code-abc123.scope".to_string(),
            },
            title: "t3code-abc123.scope".to_string(),
            pid: 200,
            start_ticks: 2000,
            pgrp: 200,
            can_stop: false,
            processes: vec![fixture_proc(200, "node")],
            cpu_percent: 2.5,
            rss_bytes: 90_000_000,
            read_bytes: None,
            write_bytes: None,
            scope_mem_bytes: None,
            container: None,
        },
        SessionGroup {
            kind: GroupKind::Agent {
                owner: "ses2".to_string(),
                id: "dev".to_string(),
            },
            title: "ses2 (dev)".to_string(),
            pid: 300,
            start_ticks: 3000,
            pgrp: 300,
            can_stop: true,
            processes: std::iter::once(fixture_proc(300, "vite"))
                .chain((301..306).map(|pid| fixture_proc(pid, "mcp-helper")))
                .collect(),
            cpu_percent: 12.5,
            rss_bytes: 310_000_000,
            read_bytes: None,
            write_bytes: None,
            scope_mem_bytes: None,
            container: None,
        },
    ]
}

/// Processes page with the busy group expanded, as the goldens show it.
fn processes_state(paths: &Paths) -> app::State {
    let data = data::Data {
        processes: fixture_groups(),
        ..data::Data::default()
    };
    let mut state = app::State::for_tests(paths.clone(), data);
    let _ = app::update(
        &mut state,
        app::Message::SelectPage(agentdeck_gui::Page::Processes),
    );
    let _ = app::update(&mut state, app::Message::ProcToggle(300, 3000));
    state
}

#[test]
fn snapshots_processes_en_ru_and_stop_dialog() {
    let _env = lock_env();
    let (paths, _dir, _guard) = fixture_paths("procs");
    std::fs::create_dir_all(SNAPSHOTS).unwrap();

    for (lang, stem) in [("en", "processes_en"), ("ru", "processes_ru")] {
        agentdeck_gui::i18n::set_language(lang);
        let state = processes_state(&paths);
        let mut ui = iced_test::Simulator::with_size(
            iced::Settings::default(),
            (960.0, 420.0),
            app::view(&state),
        );
        // Non-stoppable T3 scopes are not shown at all.
        assert!(ui.find("t3code-abc123.scope").is_err());
        // Identical helpers fold into one line.
        ui.find("mcp-helper 60 ×5").expect("folded helpers");
        let snapshot = ui
            .snapshot(&agentdeck_gui::style::theme(true))
            .expect("headless snapshot");
        assert_snapshot(&snapshot, stem);
        let goldens: Vec<_> = std::fs::read_dir(SNAPSHOTS)
            .expect("snapshots dir")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s.starts_with(stem))
                    && p.extension().and_then(|e| e.to_str()) == Some("png")
            })
            .collect();
        assert_eq!(goldens.len(), 1, "one golden per stem: {goldens:?}");
        let bytes = std::fs::metadata(&goldens[0]).expect("golden png").len();
        assert!(bytes > 1_024, "golden {} looks empty", goldens[0].display());
        assert!(
            bytes < 100 * 1024,
            "golden {} over 100 KB",
            goldens[0].display()
        );
    }

    // Stop opens a confirmation listing every process; Cancel closes it.
    // Fixture pids are fake, so the fresh member lookup falls back to the
    // snapshot rows and nothing is ever signaled.
    agentdeck_gui::i18n::set_language("en");
    let mut state = processes_state(&paths);
    let _ = app::update(&mut state, app::Message::ProcToggle(300, 3000));
    let _ = app::update(&mut state, app::Message::ProcStop(0));
    assert!(state.proc_ui().pending.is_some());
    // Esc closes the modal.
    let _ = app::update(&mut state, app::Message::Escape);
    assert!(state.proc_ui().pending.is_none());
    // Enter never confirms the destructive Stop dialog.
    let _ = app::update(&mut state, app::Message::ProcStop(0));
    let _ = app::update(&mut state, app::Message::Enter);
    assert!(state.proc_ui().pending.is_some());
    let _ = app::update(&mut state, app::Message::ProcCancel);
    let mut ui = iced_test::simulator(app::view(&state));
    // The row shows the short title; the run id is in the expanded view.
    ui.click("ses1").expect("group row toggles");
    for message in ui.into_messages() {
        let _ = app::update(&mut state, message);
    }
    {
        let mut ui = iced_test::simulator(app::view(&state));
        ui.find("sh 60").expect("expanded members");
    }
    // Busiest group first: the first Stop is ses2's, and its modal lists
    // every process, folded helpers included.
    let mut ui = iced_test::simulator(app::view(&state));
    ui.click("Stop").expect("stop button");
    for message in ui.into_messages() {
        let _ = app::update(&mut state, message);
    }
    assert!(state.proc_ui().pending.is_some());
    let _ = app::update(&mut state, app::Message::ProcForce(true));
    assert!(state.proc_ui().force);
    let mut ui = iced_test::simulator(app::view(&state));
    ui.find("Stop now").expect("confirm button");
    ui.find("300  vite 60").expect("listed member pid");
    ui.find("305  mcp-helper 60").expect("every helper listed");
    ui.click("Cancel").expect("cancel button");
    for message in ui.into_messages() {
        let _ = app::update(&mut state, message);
    }
    assert!(state.proc_ui().pending.is_none());
}

/// Execute only task output messages; these flows have no window/runtime effects.
fn finish_tasks(state: &mut app::State, task: iced::Task<app::Message>) {
    use iced::futures::StreamExt;
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(async {
            let mut pending = vec![task];
            while let Some(task) = pending.pop() {
                if let Some(mut stream) = iced_test::runtime::task::into_stream(task) {
                    while let Some(action) = stream.next().await {
                        if let iced_test::runtime::Action::Output(message) = action {
                            pending.push(app::update(state, message));
                        } else {
                            panic!("unexpected GUI runtime effect");
                        }
                    }
                }
            }
        });
}

#[test]
fn parallel_limit_edit_validate_persist_reload_and_clear() {
    let env = lock_env();
    let (initial, paths, _guard) = load_with_fixtures("parallel", &env);
    agentdeck_gui::i18n::set_language("en");
    let mut state = app::State::for_tests(paths.clone(), initial);
    let _ = app::update(
        &mut state,
        app::Message::SelectPage(agentdeck_gui::Page::Limits),
    );
    let mut ui = iced_test::simulator(app::view(&state));
    ui.click("Unlimited").unwrap();
    for message in ui.into_messages() {
        let _ = app::update(&mut state, message);
    }
    for value in ["0", "-1", "1.5", "4294967296"] {
        let _ = app::update(&mut state, app::Message::ParallelInput(value.into()));
        let task = app::update(&mut state, app::Message::ParallelSave);
        finish_tasks(&mut state, task);
        let mut ui = iced_test::simulator(app::view(&state));
        ui.find("Enter a whole number from 1 to 4294967295, or leave blank for unlimited.")
            .unwrap();
        assert_eq!(
            Registry::load(&paths)
                .unwrap()
                .get("claude-main")
                .unwrap()
                .max_parallel,
            None
        );
    }
    let _ = app::update(&mut state, app::Message::ParallelInput(" 3 ".into()));
    let mut ui = iced_test::simulator(app::view(&state));
    ui.click("Save").unwrap();
    for message in ui.into_messages() {
        let task = app::update(&mut state, message);
        finish_tasks(&mut state, task);
    }
    assert_eq!(
        Registry::load(&paths)
            .unwrap()
            .get("claude-main")
            .unwrap()
            .max_parallel,
        Some(3)
    );
    iced_test::simulator(app::view(&state)).find("3").unwrap();
    assert_eq!(data::load(&paths).instances[0].max_parallel, Some(3));

    // Cancel keeps the stored value; a failed write keeps the editor usable.
    let _ = app::update(&mut state, app::Message::ParallelAsk("claude-main".into()));
    let _ = app::update(&mut state, app::Message::ParallelInput("9".into()));
    let _ = app::update(&mut state, app::Message::Escape);
    iced_test::simulator(app::view(&state)).find("3").unwrap();
    let _ = app::update(&mut state, app::Message::ParallelAsk("claude-main".into()));
    let task = app::update(
        &mut state,
        app::Message::ParallelDone(Err("write failed".into())),
    );
    finish_tasks(&mut state, task);
    iced_test::simulator(app::view(&state))
        .find("write failed")
        .unwrap();
    for lang in ["en", "ru"] {
        agentdeck_gui::i18n::set_language(lang);
        for dark in [false, true] {
            let mut ui = iced_test::Simulator::with_size(
                iced::Settings::default(),
                (960.0, 600.0),
                app::view(&state),
            );
            let snapshot = ui
                .snapshot(&agentdeck_gui::style::theme(dark))
                .expect("headless parallel editor render");
            if let Some(dir) = std::env::var_os("AGENTDECK_SHOTS").map(PathBuf::from) {
                std::fs::create_dir_all(&dir).unwrap();
                let stem = dir.join(format!("parallel_{lang}_{dark}.png"));
                assert!(snapshot.matches_image(stem).unwrap());
            }
        }
    }
    agentdeck_gui::i18n::set_language("en");
    let _ = app::update(&mut state, app::Message::ParallelInput(" ".into()));
    let task = app::update(&mut state, app::Message::ParallelSave);
    finish_tasks(&mut state, task);
    assert_eq!(
        Registry::load(&paths)
            .unwrap()
            .get("claude-main")
            .unwrap()
            .max_parallel,
        None
    );
    iced_test::simulator(app::view(&state))
        .find("Unlimited")
        .unwrap();
}

#[test]
fn quota_module_hides_providers_and_adds_and_removes_sources() {
    use agentdeck_core::usage::UsageKind;
    use agentdeck_gui::pages::usage::{Kind, Msg};
    let env = lock_env();
    let (initial, paths, _guard) = load_with_fixtures("usage", &env);
    agentdeck_gui::i18n::set_language("en");
    let mut state = app::State::for_tests(paths.clone(), initial);
    let run = |state: &mut app::State, msg: Msg| {
        let task = app::update(state, app::Message::Usage(msg));
        finish_tasks(state, task);
    };
    let task = app::update(
        &mut state,
        app::Message::SelectPage(agentdeck_gui::Page::UsageWidget),
    );
    finish_tasks(&mut state, task);

    // The applet reads this file beside instances.toml.
    let visibility = paths.config_dir.join("ai-usage-visibility.json");
    run(&mut state, Msg::Show("codex".into(), false));
    let hidden = agentdeck_usage::visibility::load(&visibility)
        .unwrap()
        .hidden;
    assert!(hidden.contains("codex"), "{hidden:?}");
    run(&mut state, Msg::Show("codex".into(), true));
    assert!(
        agentdeck_usage::visibility::load(&visibility)
            .unwrap()
            .hidden
            .is_empty()
    );

    // Add goes through a preview; nothing is written before Apply.
    run(&mut state, Msg::OpenAdd);
    run(&mut state, Msg::Kind(Kind(UsageKind::Deepseek)));
    run(&mut state, Msg::Id("deepseek-gui".into()));
    run(&mut state, Msg::Source("DEEPSEEK_GUI_KEY".into()));
    run(&mut state, Msg::AskAdd);
    iced_test::simulator(app::view(&state))
        .find("Apply")
        .unwrap();
    assert!(
        Registry::load(&paths)
            .unwrap()
            .usage
            .iter()
            .all(|u| u.id != "deepseek-gui")
    );
    run(&mut state, Msg::Confirm);
    let reg = Registry::load(&paths).unwrap();
    let added = reg
        .usage
        .iter()
        .find(|u| u.id == "deepseek-gui")
        .expect("added");
    assert_eq!(added.api_key_env.as_deref(), Some("DEEPSEEK_GUI_KEY"));

    // A refused add (missing required field) shows the reason and no Apply.
    run(&mut state, Msg::OpenAdd);
    run(&mut state, Msg::Kind(Kind(UsageKind::Claude)));
    run(&mut state, Msg::Id("claude-gui".into()));
    run(&mut state, Msg::AskAdd);
    iced_test::simulator(app::view(&state))
        .find(r#"usage "claude-gui" (kind claude): missing "config_dir""#)
        .unwrap();
    run(&mut state, Msg::Confirm);
    assert!(
        Registry::load(&paths)
            .unwrap()
            .usage
            .iter()
            .all(|u| u.id != "claude-gui")
    );
    run(&mut state, Msg::Cancel);
    run(&mut state, Msg::CloseAdd);

    run(&mut state, Msg::AskRemove("deepseek-gui".into()));
    run(&mut state, Msg::Confirm);
    assert!(
        Registry::load(&paths)
            .unwrap()
            .usage
            .iter()
            .all(|u| u.id != "deepseek-gui")
    );
}

#[test]
fn global_instructions_appear_in_drift_without_secondary_accounts() {
    let _env = lock_env();
    let (paths, dir, _guard) = fixture_paths("global-instructions");
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    Registry::update(&paths, |_| Ok(())).unwrap();
    std::fs::write(home.join("AGENTS.md"), "shared rules\n").unwrap();
    for (path, _) in agentdeck_core::shared::instruction_targets(&home) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "client-specific rules\n").unwrap();
    }
    let ctx = drift::Ctx {
        paths,
        home,
        t3_root: None,
    };
    let snapshot = drift::snapshot(&ctx);
    assert!(snapshot.error.is_none());
    let group = drift_group(&snapshot, agentdeck_core::sync::INSTRUCTIONS_GROUP_ID);
    assert_eq!(drift::open_items(group), 4);
    assert!(
        group.items.iter().all(|item| {
            item.actions
                .iter()
                .any(|action| action.kind == drift::Kind::Apply)
                && item
                    .actions
                    .iter()
                    .any(|action| action.kind == drift::Kind::Import)
        }),
        "every divergent instruction file needs resolution actions"
    );

    let state = drift::State {
        snapshot,
        ..Default::default()
    };
    for (language, title) in [("en", "Instruction files"), ("ru", "Файлы инструкций")]
    {
        agentdeck_gui::i18n::set_language(language);
        for (name, size) in [("wide", (760.0, 420.0)), ("narrow", (420.0, 540.0))] {
            let mut ui = iced_test::Simulator::with_size(
                iced::Settings::default(),
                size,
                drift::view(&state),
            );
            ui.find(title).expect("localized instruction group");
            for item in
                &drift_group(&state.snapshot, agentdeck_core::sync::INSTRUCTIONS_GROUP_ID).items
            {
                ui.find(item.text.as_str()).expect("instruction drift row");
            }
            let screenshot = ui
                .snapshot(&agentdeck_gui::style::theme(true))
                .expect("headless instruction drift render");
            if let Some(directory) = std::env::var_os("AGENTDECK_SHOTS").map(PathBuf::from) {
                let path = directory.join(format!("global_instructions_{name}_{language}.png"));
                assert!(
                    screenshot
                        .matches_image(path)
                        .expect("save instruction drift render")
                );
            }
        }
    }
    // A valid account id must never shadow the shared instruction group.
    Registry::update(&ctx.paths, |registry| {
        registry.add(agentdeck_core::AddSpec {
            id: "global-instructions".into(),
            harness: agentdeck_core::Harness::Claude,
            display_name: None,
            home: ctx.home.join(".claude").to_string_lossy().into_owned(),
            binary: None,
            no_t3: true,
        })
    })
    .unwrap();
    let snapshot = drift::snapshot(&ctx);
    assert_eq!(
        drift_group(&snapshot, "global-instructions").harness,
        "claude"
    );
    assert_eq!(
        drift::open_items(drift_group(
            &snapshot,
            agentdeck_core::sync::INSTRUCTIONS_GROUP_ID
        )),
        4
    );
    agentdeck_gui::i18n::set_language("en");
}
