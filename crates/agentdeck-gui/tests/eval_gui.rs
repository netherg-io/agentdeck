//! Headless assessment-history tests: missing/empty/corrupt/unreadable states,
//! coordinator vs decider rows (EN/RU), and loading from the XDG state dir the
//! router store uses. No window, no goldens, isolated temp dirs only.

use agentdeck_gui::pages::eval_history::{Entry, History, Source};
use agentdeck_gui::pages::settings as settings_page;

static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn lock() -> std::sync::MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn state_with(history: History) -> settings_page::State {
    settings_page::State {
        loaded: settings_page::Loaded {
            history,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn entries() -> Vec<Entry> {
    vec![
        Entry {
            t: 0,
            source: Source::Coordinator,
            task_id: "task-1".into(),
            provider: Some("p".into()),
            model: "perf/model".into(),
            category: "test".into(),
            verdict: "ok".into(),
            evidence: "direct".into(),
            evidence_count: 1,
            self_judged: None,
            cost: None,
            judge: None,
        },
        Entry {
            t: 0,
            source: Source::Decider,
            task_id: "task-2".into(),
            provider: None,
            model: "perf/other".into(),
            category: "review".into(),
            verdict: "pass".into(),
            evidence: String::new(),
            evidence_count: 2,
            self_judged: Some(true),
            cost: Some(0.001),
            judge: Some("openrouter/judge".into()),
        },
    ]
}

#[test]
fn states_render_distinctly_without_raw_text() {
    let _env = lock();
    agentdeck_gui::i18n::set_language("en");
    for (history, needle) in [
        (
            History::Missing,
            "No assessment file yet; nothing has been recorded.",
        ),
        (History::Empty, "No assessments recorded yet."),
        (
            History::Corrupt {
                line: 3,
                what: "bad".into(),
            },
            "Assessment file is damaged at line 3: bad. The file was left unchanged.",
        ),
        (
            History::Unreadable("denied".into()),
            "Could not read assessments: denied",
        ),
    ] {
        let state = state_with(history);
        let mut ui = iced_test::simulator(settings_page::router_view(&state));
        ui.find("Assessment history").expect("card title");
        ui.find("Observations only; routing does not use them.")
            .expect("no-routing note");
        ui.find(needle).expect("state line");
        ui.snapshot(&agentdeck_gui::style::theme(true))
            .expect("headless render");
    }
    agentdeck_gui::i18n::set_language("en");
}

#[test]
fn loaded_rows_show_metadata_in_both_languages() {
    let _env = lock();
    for (lang, coord_detail, dec_detail) in [
        (
            "en",
            "task-1 · test · direct",
            "task-2 · review · 2 evidence ids · self-check · cost $0.001000 · judge openrouter/judge",
        ),
        (
            "ru",
            "task-1 · test · direct",
            "task-2 · review · 2 id свидетельств · самопроверка · цена $0.001000 · судья openrouter/judge",
        ),
    ] {
        agentdeck_gui::i18n::set_language(lang);
        let state = state_with(History::Loaded(entries()));
        let mut ui = iced_test::simulator(settings_page::router_view(&state));
        for needle in [
            "perf/model",
            "perf/other",
            "coordinator",
            "decider",
            "ok",
            "pass",
            coord_detail,
            dec_detail,
        ] {
            ui.find(needle).expect("row metadata");
        }
        ui.snapshot(&agentdeck_gui::style::theme(true))
            .expect("headless render");
    }
    agentdeck_gui::i18n::set_language("en");
}

#[test]
fn refresh_replaces_history() {
    let _env = lock();
    let mut state = state_with(History::Missing);
    let _ = settings_page::update(
        &mut state,
        paths(),
        settings_page::Msg::HistoryLoaded(History::Loaded(entries())),
    );
    let History::Loaded(got) = &state.loaded.history else {
        panic!("refresh must apply");
    };
    assert_eq!(got.len(), 2);
    assert_eq!(got[1].judge.as_deref(), Some("openrouter/judge"));
}

fn paths() -> agentdeck_core::Paths {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "agentdeck-eval-gui-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    agentdeck_core::Paths::new(dir.join("config"), dir.join("state"))
}

#[test]
fn load_reads_router_store_from_xdg_state_home() {
    let _env = lock();
    let dir = std::env::temp_dir().join(format!("agentdeck-eval-xdg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("model-route")).unwrap();
    std::fs::write(
        dir.join("model-route/outcomes.jsonl"),
        "{\"t\":1,\"source\":\"coordinator\",\"task_id\":\"t1\",\"provider\":\"p\",\"model\":\"m\",\"category\":\"test\",\"verdict\":\"ok\",\"evidence\":\"direct\",\"decider\":null}\n{\"t\":2,\"source\":\"decider\",\"task_id\":\"t2\",\"provider\":null,\"model\":\"m\",\"category\":\"test\",\"verdict\":\"pass\",\"confidence\":0.5,\"evidence_ids\":[\"a\"],\"decider\":{\"judge\":{\"provider\":\"p\",\"model\":\"m\"},\"self_judged\":null,\"judge_cost_usd\":null}}\n",
    )
    .unwrap();
    // SAFETY: serialized by LOCK; restored below.
    unsafe { std::env::set_var("XDG_STATE_HOME", &dir) };
    let _keep = Temp(&dir);
    let msg = settings_page::load(&paths());
    unsafe { std::env::remove_var("XDG_STATE_HOME") };
    let settings_page::Msg::Loaded(loaded) = msg else {
        panic!("load always reports Loaded")
    };
    let History::Loaded(got) = loaded.history else {
        panic!("both sources must load")
    };
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].source, Source::Coordinator);
    assert_eq!(got[1].source, Source::Decider);
    let _ = std::fs::remove_dir_all(&dir);
}

struct Temp<'a>(&'a std::path::Path);
impl Drop for Temp<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.0);
    }
}
