//! CLI smoke test: runs the built binary with temp config/state dirs.

use std::path::PathBuf;
use std::process::Command;

fn run(dir: &std::path::Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_agentdeck"))
        .args(args)
        .env("AGENTDECK_CONFIG_DIR", dir.join("config"))
        .env("AGENTDECK_STATE_DIR", dir.join("state"))
        .output()
        .unwrap();
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

#[test]
fn instance_lifecycle() {
    let dir: PathBuf = std::env::temp_dir().join(format!(
        "agentdeck-cli-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    // Dry-run first: nothing lands on disk.
    let (ok, out) = run(
        &dir,
        &[
            "instance",
            "add",
            "--id",
            "x",
            "--harness",
            "claude",
            "--home",
            "/fake/x",
            "--dry-run",
        ],
    );
    assert!(ok, "{out}");
    assert!(!dir.join("config/instances.toml").exists());

    let (ok, _) = run(
        &dir,
        &[
            "instance",
            "add",
            "--id",
            "x",
            "--harness",
            "claude",
            "--home",
            "/fake/x",
        ],
    );
    assert!(ok);
    let (ok, out) = run(&dir, &["instance", "list", "--json"]);
    assert!(ok, "{out}");
    assert!(out.contains("\"id\": \"x\""), "{out}");

    let (ok, _) = run(&dir, &["instance", "set", "x", "--enabled", "false"]);
    assert!(ok);
    let (ok, out) = run(&dir, &["instance", "list"]);
    assert!(ok, "{out}");
    assert!(out.contains("enabled=false"), "{out}");

    let (ok, out) = run(&dir, &["instance", "remove", "x"]);
    assert!(ok, "{out}");
    assert!(out.contains("home dir kept"), "{out}");
    let (ok, out) = run(&dir, &["instance", "list", "--json"]);
    assert!(ok, "{out}");
    assert!(!out.contains("\"id\""), "{out}");

    // Bad id is rejected.
    let (ok, out) = run(
        &dir,
        &[
            "instance",
            "add",
            "--id",
            "Bad!",
            "--harness",
            "claude",
            "--home",
            "h",
        ],
    );
    assert!(!ok, "{out}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn instance_run_preserves_profile_arguments_and_exit_status() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("agentdeck-run-{}", std::process::id()));
    let paths = agentdeck_core::Paths::new(dir.join("config"), dir.join("state"));
    std::fs::create_dir_all(&dir).unwrap();
    let binary = dir.join("fake cli's executable");
    std::fs::write(&binary, "#!/bin/sh\nprintf 'profile=%s\\n' \"$CLAUDE_CONFIG_DIR\"\nprintf '<%s>\\n' \"$@\"\nexit 7\n").unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    let profile = dir.join("profile's folder");
    agentdeck_core::Registry::update(&paths, |r| {
        r.add(agentdeck_core::AddSpec {
            id: "second".into(),
            harness: agentdeck_core::Harness::Claude,
            display_name: None,
            home: profile.display().to_string(),
            binary: Some(binary.display().to_string()),
            no_t3: true,
        })
    })
    .unwrap();
    // Interactive runs must bypass the daemon's secret loading entirely.
    std::fs::create_dir_all(paths.config_dir.join("usage.env")).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentdeck"))
        .args([
            "instance",
            "run",
            "second",
            "--",
            "one two",
            "a'b",
            "$(literal)",
            "--flag",
        ])
        .env("AGENTDECK_CONFIG_DIR", &paths.config_dir)
        .env("AGENTDECK_STATE_DIR", &paths.state_dir)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(7),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!(
            "profile={}\n<one two>\n<a'b>\n<$(literal)>\n<--flag>\n",
            profile.display()
        )
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn legacy_usage_hooks_exit_zero_quietly() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = std::env::temp_dir().join(format!("agentdeck-hook-compat-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    for (args, stdin_body) in [
        (
            vec!["usage", "hook", "claude", "--account", "claude-main"],
            r#"{"model":{"display_name":"Sonnet"},"rate_limits":{"five_hour":{"used_percentage":12}}}"#,
        ),
        (
            vec!["usage", "hook", "claude", "--account", "claude-main"],
            "",
        ),
        (
            vec!["usage", "hook", "agy", "--account", "agy-main"],
            r#"{"model":{"display_name":"Gemini"},"quota":{"gemini-5h":{"remaining_fraction":0.5}}}"#,
        ),
        (vec!["usage", "hook", "agy", "--account", "agy-main"], "{}"),
    ] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_agentdeck"))
            .args(&args)
            .env("AGENTDECK_CONFIG_DIR", dir.join("config"))
            .env("AGENTDECK_STATE_DIR", dir.join("state"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin_body.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.stdout.is_empty(), "{args:?} stdout: {:?}", out.stdout);
        assert!(out.stderr.is_empty(), "{args:?} stderr: {:?}", out.stderr);
    }

    let help = Command::new(env!("CARGO_BIN_EXE_agentdeck"))
        .args(["usage", "--help"])
        .output()
        .unwrap();
    assert!(help.status.success());
    let help_text = String::from_utf8_lossy(&help.stdout);
    assert!(!help_text.contains("\n  hook"), "{help_text}");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn approver_and_coordinator_cli_subcommands() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::process::Stdio;

    let dir: PathBuf = std::env::temp_dir().join(format!(
        "agentdeck-cli-u5-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let home = dir.join("home");
    let t3 = dir.join("t3");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(t3.join("caches")).unwrap();

    let run_env = |args: &[&str]| -> (Option<i32>, String, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_agentdeck"))
            .args(args)
            .env("HOME", &home)
            .env("AGENTDECK_CONFIG_DIR", dir.join("config"))
            .env("AGENTDECK_STATE_DIR", dir.join("state"))
            .env("AGENTDECK_T3_DIR", &t3)
            .output()
            .unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };

    // Plugin enable/list/disable for approver.
    let (code, out, err) = run_env(&["plugin", "enable", "approver"]);
    assert_eq!(code, Some(0), "{out}{err}");
    let (code, out, err) = run_env(&["plugin", "list"]);
    assert_eq!(code, Some(0), "{out}{err}");
    assert!(out.contains("approver enabled=yes"), "{out}");

    // approver config --dry-run then apply, status, audit, eval.
    let (code, out, err) = run_env(&[
        "approver",
        "config",
        "--autonomous-timeout",
        "95",
        "--judge-threshold",
        "0.9",
        "--add-remote-host",
        "homeserver",
        "--dry-run",
    ]);
    assert_eq!(code, Some(0), "{out}{err}");
    assert!(out.contains("dry-run"), "{out}");

    let (code, out, err) = run_env(&[
        "approver",
        "config",
        "--autonomous-timeout",
        "95",
        "--judge-threshold",
        "0.9",
        "--add-remote-host",
        "homeserver",
    ]);
    assert_eq!(code, Some(0), "{out}{err}");

    let (code, out, err) = run_env(&["approver", "status", "--json"]);
    assert_eq!(code, Some(0), "{out}{err}");
    let st: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(st["enabled"], true);
    assert_eq!(st["autonomous_timeout_secs"], 95);
    assert_eq!(st["remote_hosts"], serde_json::json!(["homeserver"]));

    let (code, out, err) = run_env(&["approver", "audit"]);
    assert_eq!(code, Some(0), "{out}{err}");
    assert!(out.contains("no audit records"), "{out}");

    let (code, out, err) = run_env(&["approver", "eval"]);
    assert_eq!(code, Some(0), "{out}{err}");
    assert!(out.contains("100.0%"), "{out}");

    // approver client returns exit 253 when socket is missing.
    let missing_sock = dir.join("missing.sock");
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentdeck"))
        .args(["approver", "client", "--socket"])
        .arg(&missing_sock)
        .env("HOME", &home)
        .env("AGENTDECK_CONFIG_DIR", dir.join("config"))
        .env("AGENTDECK_STATE_DIR", dir.join("state"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(
        child.stdin.take().unwrap(),
        r#"{{"kind":"confirm","title":"t","text":"b"}}"#
    )
    .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(253));

    // approver client streams pending keepalive and exits 0 on result.
    // Keep socket path short (< SUN_LEN = 108 bytes on Linux).
    let live_sock = PathBuf::from(format!("/tmp/ad-u5-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&live_sock);
    let listener = UnixListener::bind(&live_sock).unwrap();
    let srv = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        let mut r = BufReader::new(stream.try_clone().unwrap());
        r.read_line(&mut line).unwrap();
        assert!(line.contains("\"confirm\""));
        writeln!(stream, r#"{{"type":"pending","id":"abc","elapsed_secs":0}}"#).unwrap();
        writeln!(stream, r#"{{"type":"pending","id":"abc","elapsed_secs":10}}"#).unwrap();
        writeln!(stream, r#"{{"type":"result","result":"approved"}}"#).unwrap();
        stream.flush().unwrap();
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentdeck"))
        .args(["approver", "client", "--socket"])
        .arg(&live_sock)
        .env("HOME", &home)
        .env("AGENTDECK_CONFIG_DIR", dir.join("config"))
        .env("AGENTDECK_STATE_DIR", dir.join("state"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(
        child.stdin.take().unwrap(),
        r#"{{"kind":"confirm","title":"t","text":"b"}}"#
    )
    .unwrap();
    let out = child.wait_with_output().unwrap();
    srv.join().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 3, "{stdout}");
    assert!(lines[0].contains("\"pending\""));
    assert!(lines[2].contains("\"approved\""));

    // Coordinator CLI subcommands: status, pool, hygiene, eval.
    let (code, out, err) = run_env(&["coordinator", "status"]);
    assert_eq!(code, Some(0), "{out}{err}");
    assert!(out.contains("no coordinator threads"), "{out}");

    let (code, out, err) = run_env(&["coordinator", "pool"]);
    assert_eq!(code, Some(0), "{out}{err}");
    assert!(out.contains("no Coordinator Pool"), "{out}");

    let (code, out, err) = run_env(&["coordinator", "hygiene", "--path", dir.to_str().unwrap(), "--json"]);
    assert!(code == Some(0) || code == Some(1), "{out}{err}");
    let hyg: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(hyg.get("free_gb").is_some() && hyg.get("min_gb").is_some(), "{hyg}");

    let (code, out, err) = run_env(&["coordinator", "eval"]);
    assert_eq!(code, Some(0), "{out}{err}");
    assert!(out.contains("checkpoint eval:"), "{out}");

    let (code, out, err) = run_env(&["plugin", "disable", "approver"]);
    assert_eq!(code, Some(0), "{out}{err}");

    let _ = std::fs::remove_file(&live_sock);
    let _ = std::fs::remove_dir_all(&dir);
}
