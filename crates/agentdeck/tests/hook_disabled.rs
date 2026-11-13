//! A router hook left behind in a harness file must stop routing once the
//! router plugin is disabled in the registry, and keep routing (fail closed)
//! when the registry says it is enabled.

use std::io::Write;
use std::process::{Command, Stdio};

fn run_hook(registry: &str, harness: &str) -> String {
    let dir = std::env::temp_dir().join(format!(
        "agentdeck-hook-disabled-{}-{harness}-{}",
        std::process::id(),
        registry.len()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("instances.toml"), registry).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentdeck"))
        .args(["route", "hook", "--harness", harness])
        .env("HOME", dir.join("home"))
        .env("AGENTDECK_CONFIG_DIR", &config)
        .env("AGENTDECK_STATE_DIR", dir.join("state"))
        .env("XDG_STATE_HOME", dir.join("xdg-state"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"tool_name":"mcp__t3-code__delegate_task","tool_input":{"task":"x"}}"#)
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn disabled_router_hook_allows_cleanly() {
    let off = "version = 1\n\n[plugins]\nagent-process = true\n";
    assert_eq!(
        run_hook(off, "claude"),
        "",
        "claude: no output is a clean allow"
    );
    let agy: serde_json::Value = serde_json::from_str(&run_hook(off, "agy")).unwrap();
    assert_eq!(agy["decision"], "allow");
}

#[test]
fn enabled_router_hook_still_decides() {
    let on = "version = 1\n\n[plugins]\nrouter = true\n";
    // With no pool or T3 data the hook must still answer, never stay silent.
    assert!(!run_hook(on, "claude").is_empty());
}
