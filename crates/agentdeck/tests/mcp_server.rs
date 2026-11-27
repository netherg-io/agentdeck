//! `agentdeck mcp-server` over real stdio: handshake, tool list, a skill
//! install that reaches every harness dir, an instructions edit, and refusal
//! once the plugin is disabled.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

fn session(dir: &Path, requests: &[Value]) -> Vec<Value> {
    session_env(dir, &[], requests)
}

fn session_env(dir: &Path, env: &[(&str, &str)], requests: &[Value]) -> Vec<Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentdeck"))
        .arg("mcp-server")
        .env_remove("T3CODE_THREAD_ID")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("JEV_OPENROUTER_API_KEY")
        .env_remove("AI_GATEWAY_API_KEY")
        .env("AGENTDECK_T3_DIR", dir.join("no-t3"))
        .envs(env.iter().copied())
        .env("HOME", dir.join("home"))
        .env("AGENTDECK_CONFIG_DIR", dir.join("config"))
        .env("AGENTDECK_STATE_DIR", dir.join("state"))
        .env("XDG_STATE_HOME", dir.join("xdg-state"))
        .env(
            "AGENTDECK_CODEX_CONFIG",
            dir.join("home/.codex/config.toml"),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for r in requests {
        writeln!(stdin, "{r}").unwrap();
    }
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn call(id: u64, name: &str, args: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": args}})
}

fn text(reply: &Value) -> String {
    reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn tools_manage_skills_and_instructions() {
    let dir = std::env::temp_dir().join(format!("agentdeck-mcp-server-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let home = dir.join("home");
    for d in [".agents/skills", ".claude/skills", ".general/skills"] {
        std::fs::create_dir_all(home.join(d)).unwrap();
    }
    std::fs::write(home.join("AGENTS.md"), "# Rules\n\nbe brief\n").unwrap();
    std::fs::create_dir_all(dir.join("config")).unwrap();
    let registry = |on: bool| {
        let flag = if on { "mcp-server = true\n" } else { "" };
        std::fs::write(
            dir.join("config/instances.toml"),
            format!("version = 1\n\n[plugins]\n{flag}"),
        )
        .unwrap();
    };
    let src = dir.join("src/hello");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("SKILL.md"),
        "---\nname: hello\ndescription: Say hello\n---\n\n# Hello\n",
    )
    .unwrap();
    registry(true);
    let replies = session(
        &dir,
        &[
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
            call(3, "skill_install", json!({"source": src})),
            call(4, "skills_list", json!({})),
            call(
                5,
                "instructions_edit",
                json!({"old": "be brief", "new": "be very brief"}),
            ),
            call(6, "skill_remove", json!({"name": "absent"})),
        ],
    );
    assert_eq!(
        replies.len(),
        6,
        "the notification gets no reply: {replies:?}"
    );
    assert_eq!(replies[0]["result"]["serverInfo"]["name"], "agentdeck");
    let names: Vec<&str> = replies[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"skill_install") && names.contains(&"instructions_edit"));
    assert!(
        replies[2]["result"].get("isError").is_none(),
        "{}",
        text(&replies[2])
    );
    assert!(home.join(".agents/skills/hello/SKILL.md").is_file());
    assert!(std::fs::read_link(home.join(".claude/skills/hello")).is_ok());
    assert!(text(&replies[3]).contains("\"hello\""));
    assert_eq!(
        std::fs::read_to_string(home.join("AGENTS.md")).unwrap(),
        "# Rules\n\nbe very brief\n"
    );
    assert_eq!(replies[5]["result"]["isError"], true);

    registry(false);
    let replies = session(&dir, &[call(1, "skills_list", json!({}))]);
    assert_eq!(replies[0]["result"]["isError"], true);
    assert!(text(&replies[0]).contains("disabled"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn thread_state_survives_a_new_session() {
    let dir = std::env::temp_dir().join(format!("agentdeck-mcp-state-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("home")).unwrap();
    std::fs::create_dir_all(dir.join("config")).unwrap();
    std::fs::write(
        dir.join("config/instances.toml"),
        "version = 1\n\n[plugins]\nmcp-server = true\n",
    )
    .unwrap();
    let env = [("T3CODE_THREAD_ID", "thread-1")];
    let init = json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {}});
    let first = session_env(
        &dir,
        &env,
        &[
            init.clone(),
            call(
                1,
                "state_write",
                json!({"state": "Goal: finish #71\nnext: deploy", "decision": "«Подтверждаю»"}),
            ),
        ],
    );
    assert!(
        !first[0]["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("Goal: finish")
    );
    assert!(
        first[1]["result"].get("isError").is_none(),
        "{}",
        text(&first[1])
    );

    // A new provider session (new MCP process) gets the goal at initialize.
    let second = session_env(&dir, &env, &[init, call(2, "state_read", json!({}))]);
    let instr = second[0]["result"]["instructions"].as_str().unwrap();
    assert!(instr.contains("First line: Goal: finish #71"), "{instr}");
    let read: Value = serde_json::from_str(&text(&second[1])).unwrap();
    assert_eq!(read["state"], "Goal: finish #71\nnext: deploy");
    assert_eq!(read["decisions"][0]["text"], "«Подтверждаю»");

    // Without a T3 thread the tools ask for one instead of guessing.
    let none = session_env(&dir, &[], &[call(3, "state_read", json!({}))]);
    assert_eq!(none[0]["result"]["isError"], true);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn coordinator_run_state_inbox_and_assumptions() {
    let dir = std::env::temp_dir().join(format!("agentdeck-mcp-coord-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("home")).unwrap();
    std::fs::create_dir_all(dir.join("config")).unwrap();
    std::fs::write(
        dir.join("config/instances.toml"),
        "version = 1\n\n[plugins]\nmcp-server = true\ncoordinator = true\n",
    )
    .unwrap();
    let env = [("T3CODE_THREAD_ID", "coord-1")];

    // New coordinator tools are listed, and state_write sets phase + mandate.
    let replies = session_env(
        &dir,
        &env,
        &[
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
            call(
                2,
                "state_write",
                json!({"state": "Goal: ship #71\nnext: dispatch", "phase": "running",
                    "mandate": {"project": "blockfield", "fix_forward_max": 3}}),
            ),
            call(3, "state_read", json!({})),
            call(
                4,
                "owner_inbox_add",
                json!({"title": "Paste prod token", "reason": "only the owner has it",
                    "action": "paste into chat", "blocked_items": ["release"]}),
            ),
            call(
                5,
                "assumption_add",
                json!({"decision": "Use X", "rationale": "fits the contract",
                    "revert_hint": "switch to Y"}),
            ),
            call(6, "assumption_list", json!({})),
        ],
    );
    assert_eq!(replies.len(), 6, "{replies:?}");
    let names: Vec<&str> = replies[0]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for tool in [
        "owner_inbox_add",
        "owner_inbox_resolve",
        "owner_inbox_list",
        "assumption_add",
        "assumption_list",
    ] {
        assert!(names.contains(&tool), "{names:?}");
    }
    assert!(
        replies[1]["result"].get("isError").is_none(),
        "{}",
        text(&replies[1])
    );
    let wrote: Value = serde_json::from_str(&text(&replies[1])).unwrap();
    assert_eq!(wrote["phase"], "running");
    assert_eq!(wrote["state_stale"], false);
    let read: Value = serde_json::from_str(&text(&replies[2])).unwrap();
    assert_eq!(read["phase"], "running");
    assert_eq!(read["mandate"]["project"], "blockfield");
    assert_eq!(read["mandate"]["fix_forward_max"], 3);
    assert_eq!(read["state_stale"], false);
    assert_eq!(read["checklist_updates_since_state"], 0);
    let inbox: Value = serde_json::from_str(&text(&replies[3])).unwrap();
    assert_eq!(inbox["id"], "inbox-1");
    assert_eq!(inbox["resolved"], false);
    let asm: Value = serde_json::from_str(&text(&replies[4])).unwrap();
    assert_eq!(asm["id"], "asm-1");
    let asms: Value = serde_json::from_str(&text(&replies[5])).unwrap();
    assert_eq!(asms["items"].as_array().unwrap().len(), 1);

    // Five checklist updates age the snapshot; a checkpoint is then refused
    // until `state` is rewritten.
    let mut reqs = vec![call(
        10,
        "checklist_update",
        json!({"items": [{"id": "u1", "title": "Unit one"}]}),
    )];
    for i in 11..15 {
        reqs.push(call(
            i,
            "checklist_update",
            json!({"items": [{"id": "u1", "note": "still running"}]}),
        ));
    }
    reqs.push(call(15, "state_read", json!({})));
    reqs.push(call(16, "state_write", json!({"record_checkpoint": true})));
    let replies = session_env(&dir, &env, &reqs);
    assert!(
        replies[5]["result"].get("isError").is_none(),
        "{}",
        text(&replies[5])
    );
    let read: Value = serde_json::from_str(&text(&replies[5])).unwrap();
    assert_eq!(read["state_stale"], true, "{read}");
    assert_eq!(replies[6]["result"]["isError"], true);
    assert!(text(&replies[6]).contains("stale"), "{}", text(&replies[6]));

    // Fresh snapshot + checkpoint in one call succeeds; resolve the inbox item.
    let replies = session_env(
        &dir,
        &env,
        &[
            call(
                20,
                "state_write",
                json!({"state": "Goal: ship #71\nnext: verify", "record_checkpoint": true}),
            ),
            call(21, "owner_inbox_resolve", json!({"id": "inbox-1"})),
            call(22, "owner_inbox_list", json!({})),
            call(23, "owner_inbox_list", json!({"include_resolved": true})),
            call(
                24,
                "state_write",
                json!({"sleep_until": "2026-11-22T00:00:00Z"}),
            ),
            call(25, "state_write", json!({"sleep_until": null})),
        ],
    );
    let wrote: Value = serde_json::from_str(&text(&replies[0])).unwrap();
    assert_eq!(wrote["state_stale"], false);
    assert_eq!(wrote["checkpoint_count"], 1);
    let resolved: Value = serde_json::from_str(&text(&replies[1])).unwrap();
    assert_eq!(resolved["resolved"], true);
    let open: Value = serde_json::from_str(&text(&replies[2])).unwrap();
    assert_eq!(open["items"].as_array().unwrap().len(), 0);
    let all: Value = serde_json::from_str(&text(&replies[3])).unwrap();
    assert_eq!(all["items"].as_array().unwrap().len(), 1);
    let set: Value = serde_json::from_str(&text(&replies[4])).unwrap();
    assert_eq!(set["state_stale"], false);
    let read: Value = serde_json::from_str(&text(
        &session_env(&dir, &env, &[call(26, "state_read", json!({}))])[0],
    ))
    .unwrap();
    assert!(read.get("sleep_until").is_none_or(Value::is_null), "{read}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mandate_cannot_self_grant_and_sleep_until_validated() {
    let dir = std::env::temp_dir().join(format!("agentdeck-mcp-mandate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("home")).unwrap();
    std::fs::create_dir_all(dir.join("config")).unwrap();
    std::fs::write(
        dir.join("config/instances.toml"),
        "version = 1\n\n[plugins]\nmcp-server = true\ncoordinator = true\n",
    )
    .unwrap();
    let env = [("T3CODE_THREAD_ID", "mand-1")];
    let replies = session_env(
        &dir,
        &env,
        &[
            call(
                1,
                "state_write",
                json!({"state": "Goal: ship #71\nnext: dispatch",
                    "mandate": {"project": "blockfield", "owner_granted": true}}),
            ),
            call(2, "state_read", json!({})),
            call(3, "state_write", json!({"sleep_until": "not-a-date"})),
            call(4, "state_write", json!({"phase": "running"})),
            call(5, "state_read", json!({})),
        ],
    );
    assert!(
        replies[0]["result"].get("isError").is_none(),
        "{}",
        text(&replies[0])
    );
    // The MCP can never self-grant: owner_granted is forced false.
    let read: Value = serde_json::from_str(&text(&replies[1])).unwrap();
    assert_eq!(read["mandate"]["project"], "blockfield");
    assert_eq!(read["mandate"]["owner_granted"], false);
    // A malformed sleep_until is refused at the write boundary.
    assert_eq!(replies[2]["result"]["isError"], true);
    assert!(
        text(&replies[2]).contains("RFC3339"),
        "{}",
        text(&replies[2])
    );
    // A meta-only write preserves the existing mandate instead of clearing it.
    assert!(
        replies[3]["result"].get("isError").is_none(),
        "{}",
        text(&replies[3])
    );
    let read: Value = serde_json::from_str(&text(&replies[4])).unwrap();
    assert_eq!(read["phase"], "running");
    assert_eq!(read["mandate"]["project"], "blockfield");
    assert_eq!(read["mandate"]["owner_granted"], false);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn coordinator_tools_refused_without_coordinator_plugin() {
    let dir = std::env::temp_dir().join(format!("agentdeck-mcp-nocoord-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("home")).unwrap();
    std::fs::create_dir_all(dir.join("config")).unwrap();
    std::fs::write(
        dir.join("config/instances.toml"),
        "version = 1\n\n[plugins]\nmcp-server = true\n",
    )
    .unwrap();
    let env = [("T3CODE_THREAD_ID", "t1")];
    let replies = session_env(
        &dir,
        &env,
        &[call(
            1,
            "owner_inbox_add",
            json!({"title": "x", "reason": "y", "action": "z"}),
        )],
    );
    assert_eq!(replies[0]["result"]["isError"], true);
    assert!(
        text(&replies[0]).contains("coordinator plugin is disabled"),
        "{}",
        text(&replies[0])
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn coordinator_pool_checkpoint_and_hygiene_mcp_tools() {
    let dir = std::env::temp_dir().join(format!("agentdeck-mcp-coord-u5-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let t3 = dir.join("t3");
    std::fs::create_dir_all(dir.join("home")).unwrap();
    std::fs::create_dir_all(dir.join("config")).unwrap();
    std::fs::create_dir_all(t3.join("userdata")).unwrap();
    std::fs::create_dir_all(t3.join("caches")).unwrap();
    std::fs::write(
        t3.join("userdata/settings.json"),
        r#"{"providerInstances":{"claude_a":{"enabled":true},"codex_b":{"enabled":true}}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("config/instances.toml"),
        "version = 1\n\n\
         [plugins]\n\
         mcp-server = true\n\
         coordinator = true\n\n\
         [coordinator]\n\
         min_weekly_reserve_pct = 10.0\n\
         min_free_disk_gb = 1\n\n\
         [[coordinator.pool]]\n\
         instance_id = \"claude_a\"\n\
         model = \"claude-opus-4-6\"\n\
         enabled = true\n\n\
         [[coordinator.pool]]\n\
         instance_id = \"codex_b\"\n\
         model = \"gpt-6.2-sol\"\n\
         enabled = true\n",
    )
    .unwrap();
    // claude_a is thin (5% 5h), codex_b is healthy and full (80% 5h, 70% weekly).
    std::fs::write(
        t3.join("caches/claude_a.json"),
        r#"{"status":"ready","usageLimits":{"windows":[{"id":"5h","windowDurationMins":300,"usedPercent":95.0,"resetsAt":"2099-11-21T06:00:00Z"},{"id":"week","windowDurationMins":10080,"usedPercent":20.0}]}}"#,
    )
    .unwrap();
    std::fs::write(
        t3.join("caches/codex_b.json"),
        r#"{"status":"ready","usageLimits":{"windows":[{"id":"5h","windowDurationMins":300,"usedPercent":20.0},{"id":"week","windowDurationMins":10080,"usedPercent":30.0}]}}"#,
    )
    .unwrap();

    let t3_str = t3.to_string_lossy().into_owned();
    let dir_str = dir.to_string_lossy().into_owned();
    let env = [
        ("T3CODE_THREAD_ID", "coord-u5"),
        ("AGENTDECK_T3_DIR", t3_str.as_str()),
    ];

    let replies = session_env(
        &dir,
        &env,
        &[
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
            call(
                2,
                "coordinator_next_target",
                json!({"current_instance": "claude_a"}),
            ),
            call(
                3,
                "state_write",
                json!({"state": "Goal: wire U5\nnext: verify", "phase": "running"}),
            ),
            call(
                4,
                "coordinator_checkpoint_check",
                json!({"milestone_just_completed": false, "run_count_since_checkpoint": 5, "item_count_since_checkpoint": 50}),
            ),
            call(
                5,
                "coordinator_checkpoint_check",
                json!({"milestone_just_completed": true, "run_count_since_checkpoint": 5, "item_count_since_checkpoint": 50}),
            ),
            call(6, "coordinator_hygiene_check", json!({"path": dir_str})),
        ],
    );
    let names: Vec<&str> = replies[0]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for tool in [
        "coordinator_next_target",
        "coordinator_checkpoint_check",
        "coordinator_hygiene_check",
    ] {
        assert!(names.contains(&tool), "missing {tool} in {names:?}");
    }
    let next: Value = serde_json::from_str(&text(&replies[1])).unwrap();
    assert_eq!(next["status"], "selected", "{next}");
    assert_eq!(next["target"]["instance_id"], "codex_b");
    assert_eq!(next["proactive_rotate"], true);

    let quiet: Value = serde_json::from_str(&text(&replies[3])).unwrap();
    assert_eq!(quiet["should_checkpoint"], false, "{quiet}");
    assert_eq!(quiet["state_stale"], false);

    let milestone: Value = serde_json::from_str(&text(&replies[4])).unwrap();
    assert_eq!(milestone["should_checkpoint"], true, "{milestone}");
    assert_eq!(milestone["state_stale"], false);
    assert_eq!(milestone["recommended_target"]["instance_id"], "codex_b");

    let hyg: Value = serde_json::from_str(&text(&replies[5])).unwrap();
    assert_eq!(hyg["min_gb"], 1, "{hyg}");
    assert_eq!(
        hyg["ok"].as_bool().unwrap(),
        hyg["free_gb"].as_u64().unwrap() >= 1,
        "{hyg}"
    );

    // 5 checklist updates make state stale -> checkpoint_check must refuse immediately.
    let mut stale_reqs = vec![call(
        10,
        "checklist_update",
        json!({"items": [{"id": "u1", "title": "Unit"}]}),
    )];
    for i in 11..15 {
        stale_reqs.push(call(
            i,
            "checklist_update",
            json!({"items": [{"id": "u1", "note": format!("tick {i}")}]}),
        ));
    }
    stale_reqs.push(call(
        15,
        "coordinator_checkpoint_check",
        json!({"milestone_just_completed": true, "run_count_since_checkpoint": 200}),
    ));
    let stale_replies = session_env(&dir, &env, &stale_reqs);
    let chk: Value = serde_json::from_str(&text(&stale_replies[5])).unwrap();
    assert_eq!(chk["state_stale"], true, "{chk}");
    assert_eq!(chk["should_checkpoint"], false, "{chk}");
    assert!(chk["reason"].as_str().unwrap().contains("stale"), "{chk}");

    // Exhaust both pool entries -> sleeping_for_quota with wake_at.
    std::fs::write(
        t3.join("caches/claude_a.json"),
        r#"{"status":"ready","usageLimits":{"windows":[{"id":"5h","windowDurationMins":300,"usedPercent":100.0,"resetsAt":"2099-11-21T06:00:00Z"}]}}"#,
    )
    .unwrap();
    std::fs::write(
        t3.join("caches/codex_b.json"),
        r#"{"status":"ready","usageLimits":{"windows":[{"id":"5h","windowDurationMins":300,"usedPercent":100.0,"resetsAt":"2099-11-21T05:00:00Z"}]}}"#,
    )
    .unwrap();
    let sleep_replies = session_env(
        &dir,
        &env,
        &[call(20, "coordinator_next_target", json!({}))],
    );
    let sleep: Value = serde_json::from_str(&text(&sleep_replies[0])).unwrap();
    assert_eq!(sleep["status"], "sleeping_for_quota", "{sleep}");
    assert_eq!(sleep["wake_at"], "2099-11-21T05:02:00Z", "{sleep}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn state_write_blocks_cross_thread_phase_mandate_and_revokes_on_complete() {
    let dir = std::env::temp_dir().join(format!("agentdeck-mcp-xthread-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("home")).unwrap();
    std::fs::create_dir_all(dir.join("config")).unwrap();
    std::fs::write(
        dir.join("config/instances.toml"),
        "version = 1\n\n[plugins]\nmcp-server = true\ncoordinator = true\n",
    )
    .unwrap();

    // Seed thread-a with an owner-granted mandate.
    let paths = agentdeck_core::Paths::new(dir.join("config"), dir.join("state"));
    let mandate = agentdeck_core::thread_state::RunMandate {
        project: "blockfield".into(),
        allowed_hosts: vec!["local".into()],
        ..Default::default()
    };
    agentdeck_core::thread_state::set_run_meta(
        &paths,
        None,
        "thread-a",
        Some(agentdeck_core::thread_state::RunPhase::Running),
        Some(mandate.clone()),
        None,
        false,
    )
    .unwrap();
    agentdeck_core::thread_state::grant_mandate_if_matches(&paths, "thread-a", &mandate).unwrap();

    // Session bound to thread-b cannot mutate phase or mandate on thread-a, but can still read it.
    let replies = session_env(
        &dir,
        &[("T3CODE_THREAD_ID", "thread-b")],
        &[
            call(
                1,
                "state_write",
                json!({"thread": "thread-a", "phase": "completed"}),
            ),
            call(
                2,
                "state_write",
                json!({"thread": "thread-a", "mandate": {"project": "hijack"}}),
            ),
            call(3, "state_read", json!({"thread": "thread-a"})),
            call(
                4,
                "state_write",
                json!({"state": "Goal: own thread", "phase": "running"}),
            ),
        ],
    );
    assert_eq!(replies[0]["result"]["isError"], true);
    assert!(
        text(&replies[0]).contains("cannot mutate phase or mandate"),
        "{}",
        text(&replies[0])
    );
    assert_eq!(replies[1]["result"]["isError"], true);
    assert!(
        text(&replies[1]).contains("cannot mutate phase or mandate"),
        "{}",
        text(&replies[1])
    );
    let read_a: Value = serde_json::from_str(&text(&replies[2])).unwrap();
    assert_eq!(read_a["phase"], "running");
    assert_eq!(read_a["mandate"]["owner_granted"], true);
    assert!(
        replies[3]["result"].get("isError").is_none(),
        "{}",
        text(&replies[3])
    );

    // Completing own thread revokes owner_granted; later switching back to running does not resurrect it.
    let replies = session_env(
        &dir,
        &[("T3CODE_THREAD_ID", "thread-a")],
        &[
            call(10, "state_write", json!({"phase": "completed"})),
            call(11, "state_write", json!({"phase": "running"})),
            call(12, "state_read", json!({})),
        ],
    );
    assert!(
        replies[0]["result"].get("isError").is_none(),
        "{}",
        text(&replies[0])
    );
    assert!(
        replies[1]["result"].get("isError").is_none(),
        "{}",
        text(&replies[1])
    );
    let after: Value = serde_json::from_str(&text(&replies[2])).unwrap();
    assert_eq!(after["phase"], "running");
    assert_eq!(after["mandate"]["owner_granted"], false);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn coordinator_mcp_prefers_observed_model_tracks_checkpoint_deltas_and_worktrees() {
    let dir = std::env::temp_dir().join(format!("agentdeck-mcp-r2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let t3 = dir.join("t3");
    std::fs::create_dir_all(dir.join("home")).unwrap();
    std::fs::create_dir_all(dir.join("config")).unwrap();
    std::fs::create_dir_all(t3.join("userdata")).unwrap();
    std::fs::create_dir_all(t3.join("caches")).unwrap();

    std::fs::write(
        dir.join("config/instances.toml"),
        "version = 1\n\n\
         [plugins]\n\
         mcp-server = true\n\
         coordinator = true\n\n\
         [coordinator]\n\
         checkpoint_soft_runs = 5\n\
         checkpoint_soft_items = 10\n\
         min_free_disk_gb = 1\n\n\
         [[coordinator.pool]]\n\
         instance_id = \"agy\"\n\
         model = \"claude-sonnet-4-6\"\n\
         enabled = true\n\n\
         [[coordinator.pool]]\n\
         instance_id = \"agy\"\n\
         model = \"gemini-4-neon\"\n\
         enabled = true\n",
    )
    .unwrap();
    std::fs::write(
        t3.join("userdata/settings.json"),
        r#"{"providerInstances":{"agy":{"enabled":true}}}"#,
    )
    .unwrap();
    // 3p (Claude) is at 80% 5h, Gemini is at 79% 5h (within 2% hysteresis).
    std::fs::write(
        t3.join("caches/agy.json"),
        r#"{"status":"ready","usageLimits":{"windows":[
            {"id":"3p-5h","windowDurationMins":300,"usedPercent":20.0},
            {"id":"gemini-5h","windowDurationMins":300,"usedPercent":21.0}
        ]}}"#,
    )
    .unwrap();

    // Seed SQLite with thread observed on ("agy", "gemini-4-neon") with 10 runs and 20 items.
    let db = rusqlite::Connection::open(t3.join("userdata/statev2.sqlite")).unwrap();
    db.execute_batch(
        "CREATE TABLE orchestration_v2_projection_threads (
           thread_id TEXT PRIMARY KEY, title TEXT, updated_at TEXT, payload_json TEXT, provider_instance_id TEXT
         );
         CREATE TABLE orchestration_v2_projection_runs (
           run_id TEXT PRIMARY KEY, thread_id TEXT, ordinal INTEGER, status TEXT,
           requested_at TEXT, completed_at TEXT, payload_json TEXT, provider_instance_id TEXT
         );
         CREATE TABLE orchestration_v2_projection_turn_items (
           turn_item_id TEXT PRIMARY KEY, thread_id TEXT, run_id TEXT, ordinal INTEGER,
           type TEXT, status TEXT, payload_json TEXT
         );
         CREATE TABLE orchestration_v2_projection_thread_item_counts (
           thread_id TEXT PRIMARY KEY, item_count INTEGER
         );",
    )
    .unwrap();
    db.execute(
        "INSERT INTO orchestration_v2_projection_threads VALUES ('t-agy','Agy','2026-11-21T01:00:00Z',?1,'agy')",
        [r#"{"modelSelection":{"instanceId":"agy","model":"gemini-4-neon"}}"#],
    )
    .unwrap();
    db.execute(
        "INSERT INTO orchestration_v2_projection_thread_item_counts VALUES ('t-agy', 20)",
        [],
    )
    .unwrap();
    for ord in 1..=10i64 {
        db.execute(
            "INSERT INTO orchestration_v2_projection_runs VALUES (?1,'t-agy',?2,'completed','2026-11-21T01:00:00Z','2026-11-21T01:05:00Z','{}','agy')",
            rusqlite::params![format!("r{ord}"), ord],
        )
        .unwrap();
    }
    for i in 1..=20i64 {
        db.execute(
            "INSERT INTO orchestration_v2_projection_turn_items VALUES (?1,'t-agy','r10',?2,'assistant_message','completed','{\"text\":\"ok\"}')",
            rusqlite::params![format!("item-{i}"), i],
        )
        .unwrap();
    }

    let t3_str = t3.to_string_lossy().into_owned();
    let env = [
        ("T3CODE_THREAD_ID", "t-agy"),
        ("AGENTDECK_T3_DIR", t3_str.as_str()),
    ];

    // 1. Observed model ("gemini-4-neon") wins over first pool entry ("claude-sonnet-4-6") for hysteresis.
    let replies = session_env(
        &dir,
        &env,
        &[
            call(
                1,
                "coordinator_next_target",
                json!({"current_instance": "agy"}),
            ),
            call(
                2,
                "state_write",
                json!({"state": "Goal: ship\nnext: u2", "phase": "running", "record_checkpoint": true}),
            ),
            call(3, "state_read", json!({})),
        ],
    );
    let next: Value = serde_json::from_str(&text(&replies[0])).unwrap();
    assert_eq!(next["target"]["model"], "gemini-4-neon", "{next}");
    let saved: Value = serde_json::from_str(&text(&replies[2])).unwrap();
    assert_eq!(saved["checkpoint_run_count"], 10, "{saved}");
    assert_eq!(saved["checkpoint_item_count"], 20, "{saved}");

    // 2. Add 1 quiet run + 2 items (cumulative 11/22 > 5/10, but delta 1/2 < 5/10) -> must NOT checkpoint.
    db.execute(
        "INSERT INTO orchestration_v2_projection_runs VALUES ('r11','t-agy',11,'completed','2026-11-21T01:10:00Z','2026-11-21T01:11:00Z','{}','agy')",
        [],
    )
    .unwrap();
    db.execute(
        "UPDATE orchestration_v2_projection_thread_item_counts SET item_count = 22 WHERE thread_id = 't-agy'",
        [],
    )
    .unwrap();
    for i in 21..=22i64 {
        db.execute(
            "INSERT INTO orchestration_v2_projection_turn_items VALUES (?1,'t-agy','r11',?2,'assistant_message','completed','{\"text\":\"quiet\"}')",
            rusqlite::params![format!("item-{i}"), i],
        )
        .unwrap();
    }
    drop(db);

    let replies = session_env(
        &dir,
        &env,
        &[
            call(4, "coordinator_checkpoint_check", json!({})),
            call(
                5,
                "coordinator_next_target",
                json!({"current_instance": "agy", "current_model": "claude-sonnet-4-6"}),
            ),
        ],
    );
    let chk: Value = serde_json::from_str(&text(&replies[0])).unwrap();
    assert_eq!(chk["should_checkpoint"], false, "{chk}");
    assert_eq!(chk["recommended_target"]["model"], "gemini-4-neon", "{chk}");
    let explicit: Value = serde_json::from_str(&text(&replies[1])).unwrap();
    assert_eq!(
        explicit["target"]["model"], "claude-sonnet-4-6",
        "{explicit}"
    );

    // 3. coordinator_hygiene_check with work_dir lists stale merged checkout.
    let work_dir = dir.join("wt");
    let origin = dir.join("origin.git");
    let stale_wt = work_dir.join("old-wt");
    std::fs::create_dir_all(&work_dir).unwrap();
    let git = |cwd: &Path, args: &[&str]| {
        let out = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{:?}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&dir, &["init", "--bare", origin.to_str().unwrap()]);
    git(
        &dir,
        &[
            "clone",
            origin.to_str().unwrap(),
            stale_wt.to_str().unwrap(),
        ],
    );
    git(&stale_wt, &["checkout", "-b", "main"]);
    std::fs::write(stale_wt.join("f.txt"), "x\n").unwrap();
    git(&stale_wt, &["add", "f.txt"]);
    git(&stale_wt, &["commit", "-m", "init"]);
    git(&stale_wt, &["push", "-u", "origin", "main"]);

    let replies = session_env(
        &dir,
        &env,
        &[call(
            6,
            "coordinator_hygiene_check",
            json!({"path": dir, "work_dir": work_dir}),
        )],
    );
    let hyg: Value = serde_json::from_str(&text(&replies[0])).unwrap();
    let stale = hyg["stale_worktrees"].as_array().unwrap();
    assert_eq!(stale.len(), 1, "{hyg}");
    assert!(stale[0].as_str().unwrap().ends_with("old-wt"), "{hyg}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn route_batch_lists_schema_handles_invalid_and_warms_hook_cache() {
    let dir =
        std::env::temp_dir().join(format!("agentdeck-mcp-route-batch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let home = dir.join("home");
    let runtime = dir.join("runtime");
    let xdg_state = dir.join("xdg-state");
    for p in [
        dir.join("config"),
        home.join(".t3/userdata"),
        home.join(".local/state/ai-usage"),
        xdg_state.join("model-route"),
        runtime.clone(),
    ] {
        std::fs::create_dir_all(p).unwrap();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    std::fs::write(
        dir.join("config/instances.toml"),
        "version = 1\n\n[plugins]\nmcp-server = true\nrouter = true\n",
    )
    .unwrap();
    std::fs::write(home.join(".t3/userdata/settings.json"), "{}").unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/catalog.fixture.json"),
        xdg_state.join("model-route/catalog.json"),
    )
    .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64() as i64;
    let usage = json!({"accounts": [
        {"provider": "claude", "id": "claude-main", "plan": "Max 5x", "status": "ok", "updated_at": now, "balances": [],
         "windows": [
            {"key": "weekly_all", "label": "Week", "remaining_percent": 70, "duration_mins": 10080, "resets_at": now + 5 * 86400},
            {"key": "session", "label": "5h", "remaining_percent": 70, "duration_mins": 300, "resets_at": now + 3 * 3600}
         ]},
        {"provider": "codex", "id": "codex-main", "plan": "Pro 5x", "status": "ok", "updated_at": now, "balances": [],
         "windows": [
            {"key": "weekly", "label": "Week", "remaining_percent": 90, "duration_mins": 10080, "resets_at": now + 5 * 86400},
            {"key": "5h", "label": "5h", "remaining_percent": 90, "duration_mins": 300, "resets_at": now + 3 * 3600}
         ]}
    ]});
    std::fs::write(
        home.join(".local/state/ai-usage/state.json"),
        usage.to_string(),
    )
    .unwrap();
    let router_json = dir.join("router.json");
    let router_cfg = json!({
        "version": 1,
        "decisionProvider": "openrouter",
        "decisionModel": "~typesafe/jev-latest",
        "pool": [
            {"providerInstanceId": "codex", "model": "gpt-6.1-sol", "enabled": true},
            {"providerInstanceId": "claudeAgent", "model": "claude-opus-5-5", "enabled": true}
        ],
        "accounts": {
            "codex": {"instances": ["codex"], "usageProvider": "codex", "usageId": "codex-main"},
            "claude": {"instances": ["claudeAgent"], "usageProvider": "claude", "usageId": "claude-main"}
        }
    });
    std::fs::write(&router_json, router_cfg.to_string()).unwrap();
    let mock_jev = dir.join("jev.json");
    std::fs::write(
        &mock_jev,
        json!({
            "answers": {
                "size": {"score": 1, "confidence": 0.9},
                "difficulty": {"score": 1, "confidence": 0.9},
                "review": {"score": 0, "confidence": 0.95},
                "codex_gpt_6_1_sol": {"score": 2.8, "confidence": 0.9, "probabilities": {"0": 0.0, "1": 0.05, "2": 0.15, "3": 0.8}},
                "claudeagent_claude_opus_5_5": {"score": 2.2, "confidence": 0.85, "probabilities": {"0": 0.0, "1": 0.1, "2": 0.7, "3": 0.2}}
            },
            "usage": {"cost": 0.003}
        })
        .to_string(),
    )
    .unwrap();

    let runtime_s = runtime.to_string_lossy().into_owned();
    let router_s = router_json.to_string_lossy().into_owned();
    let mock_s = mock_jev.to_string_lossy().into_owned();
    let env = [
        ("XDG_RUNTIME_DIR", runtime_s.as_str()),
        ("MODEL_ROUTE_ROUTER_JSON", router_s.as_str()),
        ("AGENTDECK_ROUTE_MOCK_JEV", mock_s.as_str()),
    ];

    let launch0 = json!({
        "tool": "mcp__t3-code__delegate_task",
        "input": {
            "task": "Add unit tests for parser",
            "target": {"providerInstanceId": "codex", "model": "gpt-6.1-sol", "options": {"reasoningEffort": "high"}}
        }
    });
    let launch1_invalid = json!({
        "tool": "mcp__t3-code__delegate_task",
        "input": {"task": ""}
    });
    let launch2 = json!({
        "tool": "mcp__t3-code__delegate_task",
        "input": {"task": "[route:exclude=claudeAgent] Refactor cache key helper"}
    });

    let replies = session_env(
        &dir,
        &env,
        &[
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
            call(
                2,
                "route_batch",
                json!({
                    "harness": "claude",
                    "launches": [launch0, launch1_invalid, launch2]
                }),
            ),
        ],
    );
    // 1. tools/list includes route_batch with a valid inputSchema.
    let route_tool = replies[0]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "route_batch")
        .expect("route_batch must appear in tools/list");
    assert_eq!(route_tool["inputSchema"]["type"], "object");
    assert_eq!(route_tool["inputSchema"]["required"], json!(["launches"]));
    assert_eq!(
        route_tool["inputSchema"]["properties"]["launches"]["type"],
        "array"
    );
    assert!(
        route_tool["description"]
            .as_str()
            .unwrap()
            .contains("10-minute")
    );

    // 2. route_batch returns per-launch results in order; invalid launch gets {error} while others succeed.
    assert!(
        replies[1]["result"].get("isError").is_none(),
        "{}",
        text(&replies[1])
    );
    let batch: Value = serde_json::from_str(&text(&replies[1])).unwrap();
    let results = batch["results"].as_array().unwrap();
    assert_eq!(results.len(), 3, "{batch}");
    assert_eq!(results[0]["target"]["providerInstanceId"], "codex");
    assert_eq!(results[0]["jevCache"], "miss");
    assert_eq!(results[0]["jevChannel"], "primary");
    assert!(results[1].get("error").is_some(), "{}", results[1]);
    assert_eq!(results[2]["target"]["providerInstanceId"], "codex");
    assert_eq!(results[2]["jevCache"], "miss");

    // 3. Remove the mock Jev file and unset all API keys: any judge call will now fail.
    std::fs::remove_file(&mock_jev).unwrap();

    // 3a. Direct `agentdeck route` with the exact same source + task hits the warmed cache.
    let mut route_cmd = Command::new(env!("CARGO_BIN_EXE_agentdeck"))
        .args([
            "route",
            "--source",
            "claudeAgent",
            "--validate-target",
            "codex",
            "gpt-6.1-sol",
            "-",
        ])
        .env("HOME", &home)
        .env("XDG_STATE_HOME", &xdg_state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("MODEL_ROUTE_ROUTER_JSON", &router_json)
        .env_remove("AGENTDECK_ROUTE_MOCK_JEV")
        .env_remove("JEV_OPENROUTER_API_KEY")
        .env_remove("AI_GATEWAY_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    route_cmd
        .stdin
        .take()
        .unwrap()
        .write_all(b"Add unit tests for parser")
        .unwrap();
    let route_out = route_cmd.wait_with_output().unwrap();
    assert!(
        route_out.status.success(),
        "route must hit cache without judge call: {}",
        String::from_utf8_lossy(&route_out.stderr)
    );
    let routed: Value = serde_json::from_slice(&route_out.stdout).unwrap();
    assert_eq!(routed["jevCache"], "hit", "{routed}");

    // 3b. Routing the identical launch through the actual hook (`agentdeck route hook --harness claude`)
    // also succeeds with zero judge calls (cache hit!).
    let hint = results[0]["briefHint"].as_str().unwrap_or_default();
    let hook_payload = json!({
        "tool_name": "mcp__t3-code__delegate_task",
        "tool_input": {
            "task": format!("Add unit tests for parser\n\n{hint}"),
            "target": results[0]["target"]
        }
    });
    let mut hook_cmd = Command::new(env!("CARGO_BIN_EXE_agentdeck"))
        .args(["route", "hook", "--harness", "claude"])
        .env("HOME", &home)
        .env("AGENTDECK_CONFIG_DIR", dir.join("config"))
        .env("AGENTDECK_STATE_DIR", dir.join("state"))
        .env("XDG_STATE_HOME", &xdg_state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("MODEL_ROUTE_ROUTER_JSON", &router_json)
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("AGENTDECK_ROUTE_MOCK_JEV")
        .env_remove("JEV_OPENROUTER_API_KEY")
        .env_remove("AI_GATEWAY_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    hook_cmd
        .stdin
        .take()
        .unwrap()
        .write_all(hook_payload.to_string().as_bytes())
        .unwrap();
    let hook_out = hook_cmd.wait_with_output().unwrap();
    assert!(hook_out.status.success());
    let hook_stdout = String::from_utf8_lossy(&hook_out.stdout);
    let decision_log =
        std::fs::read_to_string(xdg_state.join("model-route/decisions.jsonl")).unwrap();
    assert!(
        decision_log.contains("\"decision\":\"allow:delegate\""),
        "hook must allow from warm cache without calling judge; stdout={hook_stdout}; log={decision_log}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn route_batch_resolves_secondary_account_via_t3_thread_and_hits_hook_cache() {
    let dir = std::env::temp_dir().join(format!(
        "agentdeck-mcp-route-batch-secondary-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let home = dir.join("home");
    let runtime = dir.join("runtime");
    let xdg_state = dir.join("xdg-state");
    let codex_b_home = dir.join("codex-b-shadow");
    for p in [
        dir.join("config"),
        home.join(".t3/userdata"),
        home.join(".local/state/ai-usage"),
        xdg_state.join("model-route"),
        runtime.clone(),
        codex_b_home.clone(),
    ] {
        std::fs::create_dir_all(p).unwrap();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    std::fs::write(
        dir.join("config/instances.toml"),
        "version = 1\n\n[plugins]\nmcp-server = true\nrouter = true\n",
    )
    .unwrap();
    std::fs::write(
        home.join(".t3/userdata/settings.json"),
        json!({
            "providerInstances": {
                "codex": {"driver": "codex", "config": {}},
                "codex_b": {"driver": "codex", "config": {"shadowHomePath": codex_b_home.to_string_lossy()}}
            }
        })
        .to_string(),
    )
    .unwrap();
    // Seed statev2.sqlite binding thread-codex-b to secondary instance `codex_b`.
    let db = rusqlite::Connection::open(home.join(".t3/userdata/statev2.sqlite")).unwrap();
    db.execute_batch(
        "CREATE TABLE orchestration_v2_projection_threads (
            thread_id TEXT PRIMARY KEY,
            title TEXT,
            updated_at TEXT,
            payload_json TEXT,
            provider_instance_id TEXT
         );
         CREATE TABLE orchestration_v2_projection_runs (
            run_id TEXT PRIMARY KEY,
            thread_id TEXT NOT NULL,
            ordinal INTEGER NOT NULL,
            provider TEXT NOT NULL,
            provider_thread_id TEXT,
            status TEXT NOT NULL,
            requested_at TEXT NOT NULL,
            completed_at TEXT,
            payload_json TEXT NOT NULL,
            provider_instance_id TEXT
         );
         CREATE TABLE orchestration_v2_projection_subagents (
            subagent_id TEXT PRIMARY KEY,
            thread_id TEXT NOT NULL,
            run_id TEXT,
            parent_node_id TEXT NOT NULL,
            provider TEXT NOT NULL,
            provider_thread_id TEXT,
            child_thread_id TEXT,
            origin TEXT NOT NULL,
            status TEXT NOT NULL,
            started_at TEXT,
            completed_at TEXT,
            updated_at TEXT NOT NULL,
            payload_json TEXT NOT NULL,
            driver TEXT,
            provider_instance_id TEXT
         );
         CREATE TABLE orchestration_v2_projection_provider_threads (
            provider_thread_id TEXT PRIMARY KEY,
            thread_id TEXT,
            owner_node_id TEXT,
            provider TEXT NOT NULL,
            provider_session_id TEXT,
            status TEXT NOT NULL,
            first_run_ordinal INTEGER,
            last_run_ordinal INTEGER,
            updated_at TEXT NOT NULL,
            payload_json TEXT NOT NULL,
            driver TEXT,
            provider_instance_id TEXT
         );
         INSERT INTO orchestration_v2_projection_threads (thread_id, title, payload_json, provider_instance_id)
            VALUES ('thread-codex-b', 'Secondary', '{}', 'codex_b');
         INSERT INTO orchestration_v2_projection_runs (run_id, thread_id, ordinal, provider, status, requested_at, payload_json, provider_instance_id)
            VALUES ('run-1', 'thread-codex-b', 1, 'codex', 'running', '2026-11-27T00:00:00Z', '{}', 'codex_b');",
    )
    .unwrap();
    drop(db);

    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/catalog.fixture.json"),
        xdg_state.join("model-route/catalog.json"),
    )
    .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64() as i64;
    let usage = json!({"accounts": [
        {"provider": "claude", "id": "claude-main", "plan": "Max 5x", "status": "ok", "updated_at": now, "balances": [],
         "windows": [
            {"key": "weekly_all", "label": "Week", "remaining_percent": 90, "duration_mins": 10080, "resets_at": now + 5 * 86400},
            {"key": "session", "label": "5h", "remaining_percent": 90, "duration_mins": 300, "resets_at": now + 3 * 3600}
         ]},
        {"provider": "codex", "id": "codex-main", "plan": "Pro 5x", "status": "ok", "updated_at": now, "balances": [],
         "windows": [
            {"key": "weekly", "label": "Week", "remaining_percent": 90, "duration_mins": 10080, "resets_at": now + 5 * 86400},
            {"key": "5h", "label": "5h", "remaining_percent": 90, "duration_mins": 300, "resets_at": now + 3 * 3600}
         ]},
        {"provider": "codex", "id": "codex-b", "plan": "Pro 5x", "status": "ok", "updated_at": now, "balances": [],
         "windows": [
            {"key": "weekly", "label": "Week", "remaining_percent": 90, "duration_mins": 10080, "resets_at": now + 5 * 86400},
            {"key": "5h", "label": "5h", "remaining_percent": 90, "duration_mins": 300, "resets_at": now + 3 * 3600}
         ]}
    ]});
    std::fs::write(
        home.join(".local/state/ai-usage/state.json"),
        usage.to_string(),
    )
    .unwrap();
    let router_json = dir.join("router.json");
    let router_cfg = json!({
        "version": 1,
        "decisionProvider": "openrouter",
        "decisionModel": "~typesafe/jev-latest",
        "pool": [
            {"providerInstanceId": "codex", "model": "gpt-6.1-sol", "enabled": true},
            {"providerInstanceId": "claudeAgent", "model": "claude-opus-5-5", "enabled": true}
        ],
        "accounts": {
            "codex": {"instances": ["codex"], "usageProvider": "codex", "usageId": "codex-main"},
            "codex_b": {"instances": ["codex_b"], "usageProvider": "codex", "usageId": "codex-b"},
            "claude": {"instances": ["claudeAgent"], "usageProvider": "claude", "usageId": "claude-main"}
        }
    });
    std::fs::write(&router_json, router_cfg.to_string()).unwrap();
    let mock_jev = dir.join("jev.json");
    std::fs::write(
        &mock_jev,
        json!({
            "answers": {
                "size": {"score": 1, "confidence": 0.9},
                "difficulty": {"score": 1, "confidence": 0.9},
                "review": {"score": 0, "confidence": 0.95},
                "codex_gpt_6_1_sol": {"score": 2.8, "confidence": 0.9, "probabilities": {"0": 0.0, "1": 0.05, "2": 0.15, "3": 0.8}},
                "claudeagent_claude_opus_5_5": {"score": 2.2, "confidence": 0.85, "probabilities": {"0": 0.0, "1": 0.1, "2": 0.7, "3": 0.2}}
            },
            "usage": {"cost": 0.003}
        })
        .to_string(),
    )
    .unwrap();

    let runtime_s = runtime.to_string_lossy().into_owned();
    let router_s = router_json.to_string_lossy().into_owned();
    let mock_s = mock_jev.to_string_lossy().into_owned();
    // Notice: NO CODEX_HOME, NO explicit harness or source in route_batch! Only T3CODE_THREAD_ID.
    let env = [
        ("T3CODE_THREAD_ID", "thread-codex-b"),
        ("XDG_RUNTIME_DIR", runtime_s.as_str()),
        ("MODEL_ROUTE_ROUTER_JSON", router_s.as_str()),
        ("AGENTDECK_ROUTE_MOCK_JEV", mock_s.as_str()),
    ];
    let launch = json!({
        "tool": "mcp__t3-code__delegate_task",
        "input": {
            "task": "Add unit tests for secondary account cache",
            "target": {"providerInstanceId": "codex", "model": "gpt-6.1-sol", "options": {"reasoningEffort": "high"}}
        }
    });
    let replies = session_env(
        &dir,
        &env,
        &[call(2, "route_batch", json!({ "launches": [launch] }))],
    );
    assert!(
        replies[0]["result"].get("isError").is_none(),
        "{}",
        text(&replies[0])
    );
    let batch: Value = serde_json::from_str(&text(&replies[0])).unwrap();
    let results = batch["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0]["source"], "codex_b",
        "route_batch must resolve secondary account via T3 thread binding: {}",
        results[0]
    );
    assert_eq!(results[0]["jevCache"], "miss");

    // Remove mock Jev & keys so any cache miss fails closed.
    std::fs::remove_file(&mock_jev).unwrap();

    // Run actual hook for secondary Codex account with ONLY CODEX_HOME (no T3CODE_THREAD_ID),
    // verifying cross-mechanism source parity and cache hit with zero judge calls.
    let hint = results[0]["briefHint"].as_str().unwrap_or_default();
    let hook_payload = json!({
        "tool_name": "mcp__t3-code__delegate_task",
        "tool_input": {
            "task": format!("Add unit tests for secondary account cache\n\n{hint}"),
            "target": results[0]["target"]
        }
    });
    let mut hook_cmd = Command::new(env!("CARGO_BIN_EXE_agentdeck"))
        .args(["route", "hook", "--harness", "codex"])
        .env("HOME", &home)
        .env("AGENTDECK_CONFIG_DIR", dir.join("config"))
        .env("AGENTDECK_STATE_DIR", dir.join("state"))
        .env("XDG_STATE_HOME", &xdg_state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("MODEL_ROUTE_ROUTER_JSON", &router_json)
        .env("CODEX_HOME", &codex_b_home)
        .env_remove("T3CODE_THREAD_ID")
        .env_remove("AGENTDECK_ROUTE_MOCK_JEV")
        .env_remove("JEV_OPENROUTER_API_KEY")
        .env_remove("AI_GATEWAY_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    hook_cmd
        .stdin
        .take()
        .unwrap()
        .write_all(hook_payload.to_string().as_bytes())
        .unwrap();
    let hook_out = hook_cmd.wait_with_output().unwrap();
    assert!(hook_out.status.success());
    let decision_log =
        std::fs::read_to_string(xdg_state.join("model-route/decisions.jsonl")).unwrap();
    assert!(
        decision_log.contains("\"instance\":\"codex_b\"")
            && decision_log.contains("\"decision\":\"allow:delegate\""),
        "hook for secondary account must hit route_batch cache with zero judge calls; log={decision_log}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
