//! Overflow check: every page at the minimum window size (900x560) with very
//! long fixture names, so truncation and fixed action slots are visible in
//! the shots. Dense 900x560 PNGs exceed the 100 KB golden limit, so they are
//! not committed: `AGENTDECK_SHOTS=<dir>` writes EN and RU shots there for
//! review. Headless only, fixture data, temp dirs.

use std::path::PathBuf;

use agentdeck_core::processes::{GroupKind, ProcInfo, SessionGroup};
use agentdeck_core::usage::{UsageEntry, UsageKind};
use agentdeck_core::{Paths, PoolEntry};
use agentdeck_gui::app::{self, Message, Page};
use agentdeck_gui::data::{Data, InstanceRow, PluginRow, QuotaBar, T3Info, UsageQuotas};
use agentdeck_gui::pages::add_instance::AddMsg;
use agentdeck_gui::pages::{drift, harnesses, pool, shared, usage};

const LONG_ID: &str = "claude-d756536e-very-long-instance-identifier-for-overflow-checks";
const LONG_NAME: &str =
    "Очень длинное отображаемое имя инстанса, которое никак не помещается в карточку";
const LONG_PATH: &str =
    "~/.config/agentdeck/some/really/deep/nested/directory/structure/that/goes/on/and/on/skills";

fn quotas(display: &str, status: &str) -> UsageQuotas {
    UsageQuotas {
        usage_id: "u".into(),
        display: display.into(),
        status: status.into(),
        error: None,
        bars: vec![
            QuotaBar {
                label: "5 hours".into(),
                used_percent: 35.0,
                scope: None,
                resets_in_mins: Some(9_999),
            },
            QuotaBar {
                label: "Week".into(),
                used_percent: 93.0,
                scope: Some("claude-opus-5-5-with-a-very-long-model-scope".into()),
                resets_in_mins: Some(135),
            },
        ],
    }
}

fn proc(pid: u32, argv: &[&str]) -> ProcInfo {
    ProcInfo {
        pid,
        ppid: 1,
        pgrp: 100,
        comm: argv[0].into(),
        argv: argv.iter().map(|s| s.to_string()).collect(),
        cpu_percent: 1.5,
        rss_bytes: 250_000_000,
        read_bytes: None,
        write_bytes: None,
        start_ticks: 1000,
        scope: None,
        owner_tag: None,
        container: None,
    }
}

fn data() -> Data {
    let long_cmd = [
        "node",
        "/home/user/.nvm/versions/node/v24.0.0/lib/node_modules/@some-scope/very-long-mcp-server-package/dist/index.js",
        "--stdio",
    ];
    Data {
        loaded: true,
        instances: vec![
            InstanceRow {
                id: LONG_ID.into(),
                display_name: LONG_NAME.into(),
                harness: "claude".into(),
                enabled: false,
                max_parallel: Some(u32::MAX),
                t3: Some(T3Info {
                    status: Some("ready".into()),
                    threads: Some(1234),
                }),
                quotas: vec![quotas(LONG_NAME, "ok")],
            },
            InstanceRow {
                id: "codex-second".into(),
                display_name: "Codex second".into(),
                harness: "codex".into(),
                enabled: true,
                max_parallel: None,
                t3: None,
                quotas: vec![quotas("Codex", "stale"), quotas(LONG_NAME, "error")],
            },
        ],
        standalone: vec![UsageQuotas {
            error: Some(format!("no key configured for {LONG_PATH}")),
            bars: Vec::new(),
            ..quotas("OpenRouter with a long display name", "error")
        }],
        plugins: vec![
            PluginRow {
                name: "agent-process".into(),
                enabled: true,
                available: true,
                detail: vec![format!("link {LONG_PATH}/agent-process -> agentdeck")],
            },
            PluginRow {
                name: "router".into(),
                enabled: false,
                available: false,
                detail: Vec::new(),
            },
        ],
        error: None,
        auto_apply: true,
        processes: vec![
            SessionGroup {
                kind: GroupKind::Agent {
                    owner: "claude-d756536e".into(),
                    id: "20261030T184618Z-abcdef".into(),
                },
                title: "claude-d756536e (20261030T184618Z-abcdef0123456789-very-long-run)".into(),
                pid: 100,
                start_ticks: 1000,
                pgrp: 100,
                can_stop: true,
                processes: vec![
                    proc(100, &long_cmd),
                    proc(101, &long_cmd),
                    proc(102, &["sh"]),
                ],
                cpu_percent: 123.4,
                rss_bytes: 9_999_000_000,
                read_bytes: None,
                write_bytes: None,
                scope_mem_bytes: None,
                container: None,
            },
            SessionGroup {
                kind: GroupKind::Container {
                    id: "a".repeat(64),
                    name: "chicago_drywall_wp".into(),
                },
                title: "chicago_drywall_wp (cd-wp)".into(),
                pid: 200,
                start_ticks: 2000,
                pgrp: 200,
                can_stop: true,
                processes: vec![
                    proc(200, &["conmon"]),
                    proc(201, &["apache2", "-DFOREGROUND"]),
                ],
                cpu_percent: 1.5,
                rss_bytes: 120_000_000,
                read_bytes: None,
                write_bytes: None,
                scope_mem_bytes: None,
                container: Some("chicago_drywall_wp".into()),
            },
            SessionGroup {
                kind: GroupKind::Harness {
                    name: "claude".into(),
                },
                title: "claude".into(),
                pid: 300,
                start_ticks: 3000,
                pgrp: 300,
                can_stop: true,
                processes: vec![proc(300, &["claude"])],
                cpu_percent: 0.5,
                rss_bytes: 300_000_000,
                read_bytes: None,
                write_bytes: None,
                scope_mem_bytes: None,
                container: Some("dev-box".into()),
            },
        ],
    }
}

fn drift_snapshot() -> drift::Snapshot {
    let item = |text: String, actions: Vec<drift::Action>| drift::Item {
        key: "k".into(),
        text,
        actions,
        ..drift::Item::default()
    };
    let action = |kind, server: Option<&str>| drift::Action {
        kind,
        key: "k".into(),
        server: server.map(str::to_string),
    };
    drift::Snapshot {
        groups: vec![
            drift::Group {
                id: LONG_ID.into(),
                title: Some(format!("Claude {LONG_ID}")),
                harness: "claude".into(),
                items: vec![
                    item(
                        format!("settings key {LONG_PATH}/settings.json differs from the primary"),
                        vec![action(drift::Kind::Apply, None)],
                    ),
                    item(
                        "MCP servers missing here".into(),
                        vec![
                            action(
                                drift::Kind::Import,
                                Some("context7-with-an-extremely-long-server-name"),
                            ),
                            action(drift::Kind::Import, Some("playwright")),
                            action(drift::Kind::Import, None),
                        ],
                    ),
                ],
            },
            drift::Group {
                id: "codex".into(),
                title: Some("Codex main".into()),
                harness: "codex".into(),
                items: vec![drift::Item {
                    text_key: Some(drift::TextKey::NoDrift),
                    ..drift::Item::default()
                }],
            },
        ],
        error: None,
    }
}

fn shot(state: &app::State, lang: &str, name: &str) {
    agentdeck_gui::i18n::set_language(lang);
    let mut ui = iced_test::Simulator::with_size(
        iced::Settings::default(),
        (900.0, 560.0),
        app::view(state),
    );
    let snapshot = ui
        .snapshot(&agentdeck_gui::style::theme(true))
        .expect("headless snapshot");
    if let Some(dir) = std::env::var_os("AGENTDECK_SHOTS").map(PathBuf::from) {
        std::fs::create_dir_all(&dir).unwrap();
        let stem = dir.join(format!("{name}_{lang}"));
        let _ = std::fs::remove_file(dir.join(format!("{name}_{lang}-wgpu.png")));
        assert!(snapshot.matches_image(stem.with_extension("png")).unwrap());
    }
}

fn update(state: &mut app::State, message: Message) {
    let _ = app::update(state, message);
}

#[test]
fn every_page_fits_at_min_size_with_long_names() {
    let dir = std::env::temp_dir().join(format!("agentdeck-gui-overflow-{}", std::process::id()));
    let paths = Paths::new(dir.join("config"), dir.join("state"));
    for lang in ["en", "ru"] {
        let mut state = app::State::for_tests(paths.clone(), data());
        update(
            &mut state,
            Message::Drift(drift::Message::SnapshotLoaded(drift_snapshot())),
        );
        shot(&state, lang, "instances");
        update(&mut state, Message::RenameAsk(LONG_ID.into()));
        shot(&state, lang, "instance_rename");
        update(&mut state, Message::RenameCancel);

        update(&mut state, Message::SelectPage(Page::Processes));
        update(&mut state, Message::ProcToggle(100, 1000));
        update(&mut state, Message::PluginDetail("agent-process".into()));
        shot(&state, lang, "processes");

        update(&mut state, Message::SelectPage(Page::Drift));
        shot(&state, lang, "drift");

        update(&mut state, Message::SelectPage(Page::Pool));
        let entry = |instance: &str, model: &str, enabled| PoolEntry {
            instance: instance.into(),
            model: model.into(),
            options: [(
                "reasoning-effort".to_string(),
                agentdeck_core::PoolValue::String("extra-high-with-long-value".into()),
            )]
            .into(),
            enabled,
        };
        update(
            &mut state,
            Message::Pool(pool::Msg::Loaded(pool::Loaded {
                entries: vec![
                    entry(
                        LONG_ID,
                        "claude-opus-5-5-20261030-preview-long-model-id",
                        true,
                    ),
                    entry("codex-second", "gpt-5", false),
                ],
                instances: vec![LONG_ID.into(), "codex-second".into()],
                ..Default::default()
            })),
        );
        shot(&state, lang, "pool");
        update(&mut state, Message::Pool(pool::Msg::Search("gpt".into())));
        shot(&state, lang, "pool_search");
        update(
            &mut state,
            Message::Pool(pool::Msg::Search("missing-model".into())),
        );
        shot(&state, lang, "pool_empty_search");
        update(&mut state, Message::Pool(pool::Msg::Search(String::new())));
        update(&mut state, Message::Pool(pool::Msg::OpenAdd));
        shot(&state, lang, "pool_add");
        update(&mut state, Message::Pool(pool::Msg::CloseAdd));

        update(&mut state, Message::SelectPage(Page::Limits));
        update(&mut state, Message::ParallelAsk(LONG_ID.into()));
        shot(&state, lang, "limits");
        update(&mut state, Message::ParallelCancel);

        update(&mut state, Message::SelectPage(Page::UsageSources));
        let source = |id: &str, kind, instance: Option<&str>| UsageEntry {
            id: id.into(),
            kind,
            instance: instance.map(Into::into),
            display: format!("{id} with a long display name for overflow"),
            config_dir: None,
            codex_home: None,
            token_file: None,
            api_key_env: None,
            db_file: None,
            extra: Default::default(),
        };
        update(
            &mut state,
            Message::Usage(usage::Msg::Loaded(usage::Loaded {
                sources: vec![
                    source(LONG_ID, UsageKind::Claude, Some(LONG_ID)),
                    source("openrouter-main", UsageKind::Openrouter, None),
                ],
                instances: vec![LONG_ID.into()],
                prefs: agentdeck_usage::visibility::Prefs {
                    hidden: ["openrouter".to_string()].into(),
                    ..Default::default()
                },
                error: None,
            })),
        );
        shot(&state, lang, "usage_sources");
        update(&mut state, Message::Usage(usage::Msg::OpenAdd));
        update(
            &mut state,
            Message::Usage(usage::Msg::Kind(usage::Kind(UsageKind::Claude))),
        );
        shot(&state, lang, "usage_add");
        update(
            &mut state,
            Message::Usage(usage::Msg::Kind(usage::Kind(UsageKind::Openrouter))),
        );
        shot(&state, lang, "usage_add_key");
        update(&mut state, Message::Usage(usage::Msg::CloseAdd));
        update(&mut state, Message::SelectPage(Page::UsageWidget));
        shot(&state, lang, "usage_widget");
        update(&mut state, Message::SelectPage(Page::UsagePoll));
        shot(&state, lang, "usage_poll");
        update(&mut state, Message::SelectPage(Page::RouterSettings));
        shot(&state, lang, "router_settings");
        update(&mut state, Message::SelectPage(Page::Modules));
        update(&mut state, Message::PluginDetail("agent-process".into()));
        shot(&state, lang, "modules");

        update(&mut state, Message::SelectPage(Page::Shared));
        let target = |name: &str, status: &str| shared::Target {
            name: name.into(),
            status: status.into(),
        };
        update(
            &mut state,
            Message::Shared(shared::Msg::Loaded(shared::Snapshot {
                skills: vec![
                    (LONG_PATH.into(), 12, 3),
                    ("~/.claude/skills".into(), 15, 0),
                ],
                instructions: vec![
                    target(&format!("{LONG_PATH}/AGENTS.md"), "linked to ~/AGENTS.md"),
                    target(
                        "~/.claude/CLAUDE.md",
                        "not a link: regular file with local edits",
                    ),
                ],
                mcp_servers: vec!["context7".into(), "playwright-with-long-name".into()],
                mcp_targets: vec![target(&format!("{LONG_PATH}/.claude.json"), "in sync")],
                error: None,
            })),
        );
        shot(&state, lang, "shared");
        update(
            &mut state,
            Message::Shared(shared::Msg::Tab(shared::Kind::Mcp)),
        );
        shot(&state, lang, "shared_mcp");

        update(&mut state, Message::SelectPage(Page::Harnesses));
        let status = |name: &str, bin: Option<&str>, version: Option<&str>, method: &str| {
            agentdeck_core::harness::Status {
                name: name.into(),
                binary: bin.map(PathBuf::from),
                version: version.map(str::to_string),
                install_method: method.into(),
            }
        };
        update(
            &mut state,
            Message::Harnesses(harnesses::Msg::Loaded(vec![
                status(
                    "claude",
                    Some("/b/claude"),
                    Some("2.1.0 (Claude Code) build 20261030 with a long suffix"),
                    "native installer (`claude update`) with an unusually long description",
                ),
                status("codex", None, None, "standalone installer (`codex update`)"),
            ])),
        );
        shot(&state, lang, "harnesses");

        update(&mut state, Message::SelectPage(Page::AddInstance));
        update(&mut state, Message::Add(AddMsg::Id(LONG_ID.into())));
        shot(&state, lang, "add_details");
        update(
            &mut state,
            Message::Add(AddMsg::PreviewReady(Ok(vec![
                format!("write {LONG_PATH}/instances.toml:"),
                format!("+[[instance]] id = \"{LONG_ID}\""),
            ]))),
        );
        shot(&state, lang, "add_preview");
        update(
            &mut state,
            Message::Add(AddMsg::ApplyDone(Ok(vec!["added".into()]))),
        );
        shot(&state, lang, "add_login");
        update(&mut state, Message::Add(AddMsg::Next));
        shot(&state, lang, "add_finish");

        update(&mut state, Message::SelectPage(Page::Processes));
        update(&mut state, Message::ProcStop(0));
        shot(&state, lang, "stop_modal");
    }
    agentdeck_gui::i18n::set_language("en");
    let _ = std::fs::remove_dir_all(&dir);
}
