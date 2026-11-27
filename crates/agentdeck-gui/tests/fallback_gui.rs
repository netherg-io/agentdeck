//! Headless fallback-channel settings tests: EN/RU rendering, load -> edit ->
//! save round-trip via `read_judge_fallback`/`write_judge_fallback`, and inline
//! validation for Vercel without a model. No window, isolated temp dirs only.

use agentdeck_core::settings::{self, JudgeFallbackSettings};
use agentdeck_core::{Paths, Registry};
use agentdeck_gui::pages::settings::{self as settings_page, FallbackProvider, Msg};

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
        "agentdeck-fallback-gui-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    (
        Paths::new(dir.join("config"), dir.join("state")),
        Guard(dir),
    )
}

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

fn loaded_state(paths: &Paths) -> settings_page::State {
    let mut state = settings_page::State::default();
    let msg = settings_page::load(paths);
    let _ = settings_page::update(&mut state, paths.clone(), msg);
    state
}

fn save_and_reload(state: &mut settings_page::State, paths: &Paths) {
    for out in drain(settings_page::update(
        state,
        paths.clone(),
        Msg::SaveFallback,
    )) {
        for out2 in drain(settings_page::update(state, paths.clone(), out)) {
            let _ = settings_page::update(state, paths.clone(), out2);
        }
    }
}

#[test]
fn fallback_card_renders_in_en_and_ru() {
    let _env = lock();
    let (paths, _guard) = fixture_paths("render");
    Registry::update(&paths, |_| Ok(())).unwrap();
    for (lang, heading, note, provider_label, none_label, model_label, or_hint) in [
        (
            "en",
            "Fallback channel",
            "Router, approver, and checkpoint judges use this channel only on rate limits, server errors, or network failures.",
            "Provider",
            "None",
            "Model",
            "Blank uses the OpenRouter default.",
        ),
        (
            "ru",
            "Резервный канал",
            "Судьи роутера, аппрувера и чекпоинтов используют этот канал только при лимитах запросов, ошибках сервера или сбоях сети.",
            "Провайдер",
            "Нет",
            "Модель",
            "Если поле пустое, используется модель OpenRouter по умолчанию.",
        ),
    ] {
        agentdeck_gui::i18n::set_language(lang);
        assert_eq!(FallbackProvider::None.to_string(), none_label);
        assert_eq!(FallbackProvider::OpenRouter.to_string(), "OpenRouter");
        assert_eq!(FallbackProvider::Vercel.to_string(), "Vercel AI Gateway");

        let mut state = loaded_state(&paths);
        assert_eq!(state.fallback_provider, FallbackProvider::None);
        {
            let mut ui = iced_test::simulator(settings_page::router_view(&state));
            ui.find(heading).expect("fallback heading");
            ui.find(note).expect("fallback note");
            ui.find(provider_label).expect("provider label");
            ui.snapshot(&agentdeck_gui::style::theme(true))
                .expect("headless render");
        }

        // OpenRouter shows the default model placeholder and hint.
        let _ = settings_page::update(
            &mut state,
            paths.clone(),
            Msg::FallbackProvider(FallbackProvider::OpenRouter),
        );
        {
            let mut ui = iced_test::simulator(settings_page::router_view(&state));
            ui.find(model_label).expect("model label");
            ui.find(or_hint).expect("openrouter model hint");
            ui.find(settings::DEFAULT_DECISION_MODEL)
                .expect("openrouter default placeholder");
        }

        // Optional visual capture when AGENTDECK_SHOTS is set (Vercel + inline error).
        if let Some(dir) = std::env::var_os("AGENTDECK_SHOTS").map(std::path::PathBuf::from) {
            let _ = settings_page::update(
                &mut state,
                paths.clone(),
                Msg::FallbackProvider(FallbackProvider::Vercel),
            );
            let _ = settings_page::update(&mut state, paths.clone(), Msg::SaveFallback);
            std::fs::create_dir_all(&dir).unwrap();
            let mut shot_ui = iced_test::Simulator::with_size(
                iced::Settings::default(),
                (760.0, 620.0),
                settings_page::router_view(&state),
            );
            let snapshot = shot_ui
                .snapshot(&agentdeck_gui::style::theme(true))
                .expect("fallback shot");
            let stem = dir.join(format!("fallback_vercel_error_{lang}"));
            let _ =
                std::fs::remove_file(dir.join(format!("fallback_vercel_error_{lang}-wgpu.png")));
            assert!(snapshot.matches_image(stem.with_extension("png")).unwrap());
        }
    }
    agentdeck_gui::i18n::set_language("en");
}

#[test]
fn fallback_load_edit_save_round_trip() {
    let _env = lock();
    agentdeck_gui::i18n::set_language("en");
    let (paths, _guard) = fixture_paths("roundtrip");
    Registry::update(&paths, |_| Ok(())).unwrap();
    // Seed a primary router model to confirm fallback saves preserve it.
    settings::write(
        &paths,
        &settings::Settings {
            decision_provider: None,
            decision_model: Some("owner/primary".into()),
            refresh_seconds: None,
            stale_seconds: None,
        },
    )
    .unwrap();

    // 1. Load defaults (no fallback).
    let mut state = loaded_state(&paths);
    assert_eq!(state.fallback_provider, FallbackProvider::None);
    assert!(state.fallback_model.is_empty());

    // 2. Choose OpenRouter without a model -> placeholder shows default, saves None model.
    let _ = settings_page::update(
        &mut state,
        paths.clone(),
        Msg::FallbackProvider(FallbackProvider::OpenRouter),
    );
    {
        let mut ui = iced_test::simulator(settings_page::router_view(&state));
        ui.find(settings::DEFAULT_DECISION_MODEL)
            .expect("openrouter default model placeholder");
    }
    save_and_reload(&mut state, &paths);
    assert!(!state.busy);
    assert_eq!(state.fallback_provider, FallbackProvider::OpenRouter);
    assert!(state.fallback_model.is_empty());
    let stored = settings::read_judge_fallback(&Registry::load(&paths).unwrap()).unwrap();
    assert_eq!(
        stored,
        JudgeFallbackSettings {
            provider: Some(settings::PROVIDER_OPENROUTER.into()),
            model: None,
        }
    );

    // 3. Edit to Vercel with an explicit model, save, and reload.
    let _ = settings_page::update(
        &mut state,
        paths.clone(),
        Msg::FallbackProvider(FallbackProvider::Vercel),
    );
    let _ = settings_page::update(
        &mut state,
        paths.clone(),
        Msg::FallbackModel("  openai/gpt-4o-mini  ".into()),
    );
    save_and_reload(&mut state, &paths);
    assert_eq!(state.fallback_provider, FallbackProvider::Vercel);
    assert_eq!(state.fallback_model, "openai/gpt-4o-mini");
    let reg = Registry::load(&paths).unwrap();
    assert_eq!(
        settings::read_judge_fallback(&reg).unwrap(),
        JudgeFallbackSettings {
            provider: Some(settings::PROVIDER_VERCEL.into()),
            model: Some("openai/gpt-4o-mini".into()),
        }
    );
    assert_eq!(
        settings::read(&reg).decision_model.as_deref(),
        Some("owner/primary"),
        "primary router setting survives fallback writes"
    );

    // 4. Switch back to None and save -> clears `[judge_fallback]`.
    let _ = settings_page::update(
        &mut state,
        paths.clone(),
        Msg::FallbackProvider(FallbackProvider::None),
    );
    save_and_reload(&mut state, &paths);
    assert_eq!(state.fallback_provider, FallbackProvider::None);
    assert!(state.fallback_model.is_empty());
    assert_eq!(
        settings::read_judge_fallback(&Registry::load(&paths).unwrap()).unwrap(),
        JudgeFallbackSettings::default()
    );
}

#[test]
fn vercel_without_model_shows_inline_validation_error_and_blocks_save() {
    let _env = lock();
    agentdeck_gui::i18n::set_language("en");
    let (paths, _guard) = fixture_paths("vercel-invalid");
    Registry::update(&paths, |_| Ok(())).unwrap();
    let initial = JudgeFallbackSettings {
        provider: Some(settings::PROVIDER_OPENROUTER.into()),
        model: Some("openrouter/kept".into()),
    };
    settings::write_judge_fallback(&paths, &initial).unwrap();

    let mut state = loaded_state(&paths);
    let _ = settings_page::update(
        &mut state,
        paths.clone(),
        Msg::FallbackProvider(FallbackProvider::Vercel),
    );
    let _ = settings_page::update(&mut state, paths.clone(), Msg::FallbackModel("   ".into()));
    let _ = settings_page::update(&mut state, paths.clone(), Msg::SaveFallback);

    assert!(!state.busy, "invalid fallback must not start a write");
    let err = state
        .fallback_error
        .clone()
        .expect("inline validation error");
    assert!(err.contains("requires model"), "unexpected error: {err}");
    // Exact validation message renders inline in the view.
    {
        let mut ui = iced_test::simulator(settings_page::router_view(&state));
        ui.find(err.as_str())
            .expect("inline validation error visible");
    }
    // On-disk config stays untouched.
    assert_eq!(
        settings::read_judge_fallback(&Registry::load(&paths).unwrap()).unwrap(),
        initial
    );

    // Check button also validates inline instead of probing with no model.
    state.fallback_error = None;
    let _ = settings_page::update(&mut state, paths.clone(), Msg::TestFallback);
    assert!(!state.fallback_testing);
    assert!(
        state
            .fallback_error
            .as_deref()
            .is_some_and(|e| e.contains("requires model"))
    );

    // Typing a model clears the inline validation error.
    let _ = settings_page::update(
        &mut state,
        paths.clone(),
        Msg::FallbackModel("openai/gpt-4o-mini".into()),
    );
    assert!(state.fallback_error.is_none());

    // Probe results render in the fallback card just like the primary check.
    let _ = settings_page::update(
        &mut state,
        paths.clone(),
        Msg::TestedFallback(Ok(
            "openai/gpt-4o-mini works: size 0.10, difficulty 0.10, 42 ms".into(),
        )),
    );
    {
        let mut ui = iced_test::simulator(settings_page::router_view(&state));
        ui.find("openai/gpt-4o-mini works: size 0.10, difficulty 0.10, 42 ms")
            .expect("fallback probe ok pill visible");
    }
    let _ = settings_page::update(
        &mut state,
        paths.clone(),
        Msg::TestedFallback(Err("missing AI_GATEWAY_API_KEY".into())),
    );
    {
        let mut ui = iced_test::simulator(settings_page::router_view(&state));
        ui.find("missing AI_GATEWAY_API_KEY")
            .expect("fallback probe error visible");
    }
}

#[test]
fn fallback_check_honours_active_429_block_and_displays_error() {
    let _env = lock();
    agentdeck_gui::i18n::set_language("en");
    let (paths, guard) = fixture_paths("probe-blocked");
    Registry::update(&paths, |_| Ok(())).unwrap();
    let rt = guard.0.join("rt");
    agentdeck_core::judge::create_private_dir(&rt).unwrap();

    let prev_rt = std::env::var_os("XDG_RUNTIME_DIR");
    // SAFETY: serialized by `lock()` across tests in this test binary.
    unsafe { std::env::set_var("XDG_RUNTIME_DIR", &rt) };

    let ep = agentdeck_core::judge::JudgeEndpoint::new(
        settings::PROVIDER_OPENROUTER,
        settings::DEFAULT_DECISION_MODEL,
    );
    let blocks = agentdeck_core::judge::BlockStore::default_runtime().unwrap();
    blocks
        .record_rate_limit(&ep, std::time::Duration::from_secs(60))
        .unwrap();

    let mut state = loaded_state(&paths);
    let _ = settings_page::update(
        &mut state,
        paths.clone(),
        Msg::FallbackProvider(FallbackProvider::OpenRouter),
    );
    let msgs = drain(settings_page::update(
        &mut state,
        paths.clone(),
        Msg::TestFallback,
    ));
    for m in msgs {
        let _ = settings_page::update(&mut state, paths.clone(), m);
    }

    // Restore XDG_RUNTIME_DIR before assertions so failure doesn't poison env.
    match prev_rt {
        Some(v) => unsafe { std::env::set_var("XDG_RUNTIME_DIR", v) },
        None => unsafe { std::env::remove_var("XDG_RUNTIME_DIR") },
    }

    let (text, is_err) = state
        .fallback_test
        .clone()
        .expect("fallback probe should finish with blocked-until error");
    assert!(is_err, "active 429 block must surface as an error: {text}");
    assert!(
        text.contains("429") && text.to_lowercase().contains("blocked"),
        "expected blocked-until 429 error, got: {text}"
    );
    let mut ui = iced_test::simulator(settings_page::router_view(&state));
    ui.find(text.as_str())
        .expect("blocked-until error visible in fallback card");
}
