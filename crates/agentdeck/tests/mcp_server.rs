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
        .envs(env.iter().copied())
        .env("AGENTDECK_T3_DIR", dir.join("no-t3"))
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
            call(24, "state_write", json!({"sleep_until": "2026-11-22T00:00:00Z"})),
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
    let read: Value = serde_json::from_str(
        &text(&session_env(&dir, &env, &[call(26, "state_read", json!({}))])[0]),
    )
    .unwrap();
    assert!(read.get("sleep_until").is_none_or(Value::is_null), "{read}");
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
