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
