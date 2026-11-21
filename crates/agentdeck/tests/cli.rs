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

#[test]
fn approver_status_readonly_audit_sanitized_and_config_bounds() {
    use std::os::unix::fs::PermissionsExt;

    let dir: PathBuf = std::env::temp_dir().join(format!(
        "agentdeck-cli-approver-r2-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let home = dir.join("home");
    let config_dir = dir.join("config");
    let state_dir = dir.join("state");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(state_dir.join("approver")).unwrap();

    let run_env = |args: &[&str]| -> (Option<i32>, String, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_agentdeck"))
            .args(args)
            .env("HOME", &home)
            .env("AGENTDECK_CONFIG_DIR", &config_dir)
            .env("AGENTDECK_STATE_DIR", &state_dir)
            .output()
            .unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };

    // 1. Missing approver.env -> telegram_status == "missing".
    let (code, out, err) = run_env(&["approver", "status", "--json"]);
    assert_eq!(code, Some(0), "{out}{err}");
    let st: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(st["telegram_configured"], false);
    assert_eq!(st["telegram_status"], "missing");

    // 2. Valid approver.env with 0644 -> status is read-only and NEVER mutates permissions.
    let env_path = config_dir.join("approver.env");
    std::fs::write(&env_path, "TELEGRAM_BOT_TOKEN=tok\nTELEGRAM_OWNER_ID=42\n").unwrap();
    std::fs::set_permissions(&env_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let (code, out, err) = run_env(&["approver", "status", "--json"]);
    assert_eq!(code, Some(0), "{out}{err}");
    let st: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(st["telegram_configured"], true);
    assert_eq!(st["telegram_status"], "configured");
    assert_eq!(
        std::fs::metadata(&env_path).unwrap().permissions().mode() & 0o777,
        0o644,
        "read-only approver status must not mutate approver.env permissions"
    );

    // 3. Malformed approver.env -> distinguished as "invalid", permissions still untouched.
    std::fs::write(&env_path, "NOT_A_KEY_VALUE_LINE\n").unwrap();
    let (code, out, err) = run_env(&["approver", "status", "--json"]);
    assert_eq!(code, Some(0), "{out}{err}");
    let st: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(st["telegram_configured"], false);
    assert_eq!(st["telegram_status"], "invalid");
    assert_eq!(
        std::fs::metadata(&env_path).unwrap().permissions().mode() & 0o777,
        0o644
    );

    // 4. Sanitized audit output: raw provider text in legacy JSONL is redacted on CLI export.
    let legacy = serde_json::json!({
        "timestamp": 1_700_000_001,
        "request_id": "req-1",
        "host": "local",
        "kind": "confirm",
        "sha256_payload": "deadbeef",
        "perimeter": "clear",
        "judge_allow": false,
        "judge_confidence": 0.0,
        "judge_reason": "HTTP 500 echoed SYNTHETIC_SECRET_MARKER_999",
        "outcome": "denied",
        "source": "Denied",
        "elapsed_ms": 12
    });
    std::fs::write(
        state_dir.join("approver/audit.jsonl"),
        format!("{legacy}\n"),
    )
    .unwrap();
    let (code, out, err) = run_env(&["approver", "audit", "--json"]);
    assert_eq!(code, Some(0), "{out}{err}");
    assert!(!out.contains("SYNTHETIC_SECRET_MARKER_999"), "{out}");
    let rows: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(rows[0]["judge_reason"], "judge_reason_redacted");
    let (code, out, err) = run_env(&["approver", "audit"]);
    assert_eq!(code, Some(0), "{out}{err}");
    assert!(!out.contains("SYNTHETIC_SECRET_MARKER_999"), "{out}");

    // 5. approver config enforces bounds (0, i64::MAX + 1, negative) and preserves unrelated fields.
    let (code, out, err) = run_env(&["approver", "config", "--add-deny-pattern", "drop-table"]);
    assert_eq!(code, Some(0), "{out}{err}");
    let (code, out, err) = run_env(&["approver", "config", "--autonomous-timeout", "90"]);
    assert_eq!(code, Some(0), "{out}{err}");
    for bad in ["0", "9223372036854775808", "-1"] {
        let (code, out, err) = run_env(&["approver", "config", "--autonomous-timeout", bad]);
        assert_ne!(code, Some(0), "bad timeout {bad} must fail: {out}{err}");
    }
    let (code, out, err) = run_env(&["approver", "config"]);
    assert_eq!(code, Some(0), "{out}{err}");
    let cfg: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(cfg["autonomous_timeout_secs"], 90);
    assert_eq!(cfg["extra_hard_deny_patterns"], serde_json::json!(["drop-table"]));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn coordinator_pool_hygiene_and_mandate_cli() {
    let dir: PathBuf = std::env::temp_dir().join(format!(
        "agentdeck-cli-coord-r2-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let home = dir.join("home");
    let config_dir = dir.join("config");
    let state_dir = dir.join("state");
    let t3 = dir.join("t3");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(t3.join("userdata")).unwrap();
    std::fs::create_dir_all(t3.join("caches")).unwrap();

    let run_env = |args: &[&str]| -> (Option<i32>, String, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_agentdeck"))
            .args(args)
            .env("HOME", &home)
            .env("AGENTDECK_CONFIG_DIR", &config_dir)
            .env("AGENTDECK_STATE_DIR", &state_dir)
            .env("AGENTDECK_T3_DIR", &t3)
            .output()
            .unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };

    // 1. Multi-model pool on `agy` plus an unknown/deleted instance `ghost`.
    std::fs::write(
        config_dir.join("instances.toml"),
        "version = 1\n\n\
         [[instance]]\n\
         id = \"agy\"\n\
         harness = \"agy\"\n\
         home = \"/fake/agy\"\n\
         enabled = true\n\
         t3 = true\n\n\
         [coordinator]\n\
         min_free_disk_gb = 1\n\n\
         [[coordinator.pool]]\n\
         instance_id = \"ghost\"\n\
         model = \"claude-opus-4-6\"\n\
         enabled = true\n\n\
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
    // 3p (Claude) has 80% remaining; Gemini has 79% remaining (within 2% hysteresis).
    std::fs::write(
        t3.join("caches/agy.json"),
        r#"{"status":"ready","usageLimits":{"windows":[
            {"id":"3p-5h","windowDurationMins":300,"usedPercent":20.0},
            {"id":"gemini-5h","windowDurationMins":300,"usedPercent":21.0}
        ]}}"#,
    )
    .unwrap();

    // Ambiguous `--current agy` without `--current-model` must fail loudly.
    let (code, out, err) = run_env(&["coordinator", "pool", "--current", "agy"]);
    assert_ne!(code, Some(0), "ambiguous --current must fail: {out}{err}");
    assert!(
        err.contains("--current-model") || out.contains("--current-model"),
        "{out}{err}"
    );

    // Explicit `--current agy --current-model gemini-4-neon` preserves gemini via 2% hysteresis
    // and excludes `ghost` (unknown/deleted registry instance) as unhealthy.
    let (code, out, err) = run_env(&[
        "coordinator",
        "pool",
        "--current",
        "agy",
        "--current-model",
        "gemini-4-neon",
        "--json",
    ]);
    assert_eq!(code, Some(0), "{out}{err}");
    let pool_json: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(pool_json["entries"][0]["instance_id"], "ghost");
    assert_eq!(pool_json["entries"][0]["healthy"], false);
    assert_eq!(pool_json["selection"]["status"], "selected");
    assert_eq!(pool_json["selection"]["target"]["instance_id"], "agy");
    assert_eq!(pool_json["selection"]["target"]["model"], "gemini-4-neon");

    // Removing `agy` from registry immediately makes it unhealthy even if T3 cache stays on disk.
    let (code, out, err) = run_env(&["instance", "remove", "agy"]);
    assert_eq!(code, Some(0), "{out}{err}");
    std::fs::write(
        t3.join("userdata/settings.json"),
        r#"{"providerInstances":{}}"#,
    )
    .unwrap();
    let (code, out, err) = run_env(&["coordinator", "pool", "--json"]);
    assert_eq!(code, Some(0), "{out}{err}");
    let pool_json: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(pool_json["selection"]["status"], "sleeping_for_quota");

    // 2. Owner CLI grant-mandate / revoke-mandate.
    let paths = agentdeck_core::Paths::new(config_dir.clone(), state_dir.clone());
    agentdeck_core::thread_state::set_run_meta(
        &paths,
        None,
        "t-cli-mand",
        Some(agentdeck_core::thread_state::RunPhase::Intake),
        Some(agentdeck_core::thread_state::RunMandate {
            project: "blockfield".into(),
            allowed_hosts: vec!["homeserver".into()],
            allowed_apps: vec!["staging".into()],
            notes: "smoke test".into(),
            ..Default::default()
        }),
        None,
        false,
    )
    .unwrap();
    let (code, out, err) = run_env(&["coordinator", "grant-mandate", "t-cli-mand", "--yes"]);
    assert_eq!(code, Some(0), "{out}{err}");
    assert!(out.contains("homeserver") && out.contains("granted mandate"), "{out}");
    let st = agentdeck_core::thread_state::read(&paths, None, "t-cli-mand")
        .unwrap()
        .unwrap()
        .data;
    assert!(st.mandate.as_ref().unwrap().owner_granted);

    let (code, out, err) = run_env(&["coordinator", "revoke-mandate", "t-cli-mand"]);
    assert_eq!(code, Some(0), "{out}{err}");
    let st = agentdeck_core::thread_state::read(&paths, None, "t-cli-mand")
        .unwrap()
        .unwrap()
        .data;
    assert!(!st.mandate.as_ref().unwrap().owner_granted);
    assert_eq!(st.mandate.as_ref().unwrap().allowed_hosts, vec!["homeserver"]);

    // 3. coordinator hygiene --work-dir discovers clean merged checkout and skips dirty checkout.
    let work_dir = dir.join("worktrees");
    let origin = dir.join("origin.git");
    let clean_wt = work_dir.join("stale-merged");
    let dirty_wt = work_dir.join("dirty-wt");
    std::fs::create_dir_all(&work_dir).unwrap();
    let git = |cwd: &std::path::Path, args: &[&str]| {
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
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&dir, &["init", "--bare", origin.to_str().unwrap()]);
    git(&dir, &["clone", origin.to_str().unwrap(), clean_wt.to_str().unwrap()]);
    git(&clean_wt, &["checkout", "-b", "main"]);
    std::fs::write(clean_wt.join("README.md"), "hi\n").unwrap();
    git(&clean_wt, &["add", "README.md"]);
    git(&clean_wt, &["commit", "-m", "init"]);
    git(&clean_wt, &["push", "-u", "origin", "main"]);

    git(&dir, &["clone", origin.to_str().unwrap(), dirty_wt.to_str().unwrap()]);
    std::fs::write(dirty_wt.join("dirty.txt"), "untracked\n").unwrap();

    let (code, out, err) = run_env(&[
        "coordinator",
        "hygiene",
        "--path",
        dir.to_str().unwrap(),
        "--work-dir",
        work_dir.to_str().unwrap(),
        "--json",
    ]);
    assert!(code == Some(0) || code == Some(1), "{out}{err}");
    let hyg: serde_json::Value = serde_json::from_str(&out).unwrap();
    let stale = hyg["stale_worktrees"].as_array().unwrap();
    assert_eq!(stale.len(), 1, "{hyg}");
    assert!(
        stale[0].as_str().unwrap().ends_with("stale-merged"),
        "{hyg}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
