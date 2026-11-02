//! Headless rules-settings tests: defaults vs user source rendering (EN/RU),
//! invalid patterns blocking save, and other-settings saves preserving rules.
//! No window, no goldens, isolated temp dirs only.

use agentdeck_core::settings::{self, UserRules};
use agentdeck_core::{Paths, Registry};
use agentdeck_gui::pages::settings as settings_page;

static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn lock() -> std::sync::MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

struct Guard(std::path::PathBuf);
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fixture_paths(tag: &str) -> (Paths, Guard) {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "agentdeck-rules-gui-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    (
        Paths::new(dir.join("config"), dir.join("state")),
        Guard(dir),
    )
}

fn loaded_state(paths: &Paths) -> settings_page::State {
    let mut state = settings_page::State::default();
    let msg = settings_page::load(paths);
    let _ = settings_page::update(&mut state, paths.clone(), msg);
    state
}

#[test]
fn defaults_render_with_source_and_override_note() {
    let _env = lock();
    let (paths, _guard) = fixture_paths("defaults");
    Registry::update(&paths, |_| Ok(())).unwrap();
    for (lang, source, note) in [
        (
            "en",
            "Model rules · Built-in defaults",
            "A model you name directly can still override these rules.",
        ),
        (
            "ru",
            "Правила моделей · Встроенные значения",
            "Явно названную модель всё равно можно использовать.",
        ),
    ] {
        agentdeck_gui::i18n::set_language(lang);
        let state = loaded_state(&paths);
        assert_eq!(state.no_review, "gemini-.*argon");
        assert!(state.deny.is_empty());
        assert!(!state.rules_user);
        let mut ui = iced_test::simulator(settings_page::router_view(&state));
        ui.find(source).expect("rules card with source");
        ui.find(note).expect("override note");
        ui.snapshot(&agentdeck_gui::style::theme(true))
            .expect("headless render");
    }
    agentdeck_gui::i18n::set_language("en");
}

#[test]
fn saved_rules_render_as_user_source() {
    let _env = lock();
    agentdeck_gui::i18n::set_language("en");
    let (paths, _guard) = fixture_paths("user");
    Registry::update(&paths, |_| Ok(())).unwrap();
    settings::write_user_rules(
        &paths,
        &UserRules {
            no_review_models: vec!["qwen".into()],
            deny_models: vec!["^gpt-".into()],
            source: agentdeck_core::settings::UserRuleSource::User,
        },
    )
    .unwrap();
    let state = loaded_state(&paths);
    assert_eq!(state.no_review, "qwen");
    assert_eq!(state.deny, "^gpt-");
    assert!(state.rules_user);
    let mut ui = iced_test::simulator(settings_page::router_view(&state));
    ui.find("Model rules · Your saved rules")
        .expect("user source line");
    ui.find("qwen").expect("saved pattern visible");
}

#[test]
fn invalid_pattern_blocks_save_and_keeps_registry() {
    let _env = lock();
    agentdeck_gui::i18n::set_language("en");
    let (paths, _guard) = fixture_paths("invalid");
    Registry::update(&paths, |_| Ok(())).unwrap();
    let mut state = loaded_state(&paths);
    let _ = settings_page::update(
        &mut state,
        paths.clone(),
        settings_page::Msg::NoReview("([".into()),
    );
    let _ = settings_page::update(&mut state, paths.clone(), settings_page::Msg::SaveRules);
    assert!(!state.busy, "no write started");
    let (text, is_err) = state.result.clone().expect("validation error");
    assert!(is_err, "error toast: {text}");
    assert!(text.contains("not a regex"), "actionable: {text}");
    // Nothing was stored: the registry still has no explicit rules table.
    let reg = Registry::load(&paths).unwrap();
    assert!(
        UserRules::from_registry(&reg).unwrap().source
            == agentdeck_core::settings::UserRuleSource::Default
    );
    assert_eq!(state.no_review, "([".to_string(), "user text kept");
}

#[test]
fn other_settings_saves_preserve_explicit_rules() {
    let _env = lock();
    let (paths, _guard) = fixture_paths("preserve");
    Registry::update(&paths, |_| Ok(())).unwrap();
    settings::write_user_rules(
        &paths,
        &UserRules {
            no_review_models: vec![],
            deny_models: vec!["^gpt-".into()],
            source: agentdeck_core::settings::UserRuleSource::User,
        },
    )
    .unwrap();
    settings::write(
        &paths,
        &settings::Settings {
            decision_provider: None,
            decision_model: Some("owner/model".into()),
            refresh_seconds: Some(60),
            stale_seconds: None,
        },
    )
    .unwrap();
    let reg = Registry::load(&paths).unwrap();
    let rules = UserRules::from_registry(&reg).unwrap();
    // Explicit empty list disables the ban and survives other saves.
    assert!(rules.no_review_models.is_empty());
    assert_eq!(rules.deny_models, vec!["^gpt-".to_string()]);
    assert!(rules.source == agentdeck_core::settings::UserRuleSource::User);
    let kept = settings::read(&reg);
    assert_eq!(kept.decision_model.as_deref(), Some("owner/model"));
    assert_eq!(kept.refresh_seconds, Some(60));
}
