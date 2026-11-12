//! `agentdeck mcp-server` over real stdio: handshake, tool list, a skill
//! install that reaches every harness dir, an instructions edit, and refusal
//! once the plugin is disabled.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

fn session(dir: &Path, requests: &[Value]) -> Vec<Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentdeck"))
        .arg("mcp-server")
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
