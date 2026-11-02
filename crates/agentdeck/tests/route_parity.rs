//! Differential test: the Rust `agentdeck route` must print byte-identical JSON
//! (and exit codes) to the Node `route.mjs` on the same fixtures with Jev
//! mocked. No real API calls, no real key file. Skips with a note when Node
//! is absent.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn workspace_root() -> PathBuf {
    let mut d = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    d.pop();
    d.pop();
    d
}

fn node_available() -> bool {
    Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(pool: &[(&str, &str)]) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-route-parity-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        for rel in [
            ".t3/userdata",
            ".local/state/ai-usage",
            ".local/state/model-route",
            "rt-node",
            "rt-rust",
        ] {
            std::fs::create_dir_all(dir.join(rel)).unwrap();
        }
        std::fs::copy(
            workspace_root().join("crates/agentdeck-core/assets/router/catalog.fixture.json"),
            dir.join(".local/state/model-route/catalog.json"),
        )
        .unwrap();
        std::fs::write(dir.join(".t3/userdata/settings.json"), "{}").unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64() as i64;
        let usage = serde_json::json!({"accounts": [
            {"provider": "claude", "id": "claude-main", "plan": "Max 5x", "status": "ok", "updated_at": now, "balances": [],
             "windows": [
                {"key": "weekly_all", "label": "Week", "remaining_percent": 60, "duration_mins": 10080, "resets_at": now + 6 * 86400 + 1800},
                {"key": "session", "label": "5h", "remaining_percent": 20, "duration_mins": 300, "resets_at": now + 2 * 3600}]},
            {"provider": "codex", "id": "codex-main", "plan": "Pro 5x", "status": "ok", "updated_at": now, "balances": [],
             "windows": [{"key": "weekly", "label": "Week", "remaining_percent": 80, "duration_mins": 10080, "resets_at": now + 3 * 86400}]},
            {"provider": "openrouter", "status": "ok", "updated_at": now, "balances": [{"total": "10.00"}], "windows": []},
        ]});
        std::fs::write(
            dir.join(".local/state/ai-usage/state.json"),
            serde_json::to_string(&usage).unwrap(),
        )
        .unwrap();
        let pool_json: Vec<_> = pool
            .iter()
            .map(|(p, m)| serde_json::json!({"providerInstanceId": p, "model": m, "enabled": true}))
            .collect();
        let router = serde_json::json!({"version": 1, "pool": pool_json,
            "accounts": {
                "claude": {"instances": ["claudeAgent"], "usageProvider": "claude", "usageId": "claude-main"},
                "codex": {"instances": ["codex", "codex_second"], "usageProvider": "codex", "usageId": "codex-main"},
                "openrouter": {"instances": ["opencode"], "usageProvider": "openrouter"}},
            "usageStatePath": dir.join(".local/state/ai-usage/state.json")});
        std::fs::write(
            dir.join("router.json"),
            serde_json::to_string(&router).unwrap(),
        )
        .unwrap();
        Self { dir }
    }

    fn env(&self, runtime: &str) -> Vec<(String, String)> {
        vec![
            ("HOME".into(), self.dir.to_string_lossy().into_owned()),
            (
                "XDG_STATE_HOME".into(),
                self.dir.join(".local/state").to_string_lossy().into_owned(),
            ),
            (
                "XDG_RUNTIME_DIR".into(),
                self.dir.join(runtime).to_string_lossy().into_owned(),
            ),
            (
                "MODEL_ROUTE_ROUTER_JSON".into(),
                self.dir.join("router.json").to_string_lossy().into_owned(),
            ),
            ("JEV_OPENROUTER_API_KEY".into(), "test".into()),
        ]
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn level(score: f64, fit: f64) -> serde_json::Value {
    serde_json::json!({"score": score, "confidence": 0.9,
        "probabilities": {"0": 0, "1": 1.0 - fit, "2": fit - 0.2, "3": 0.2}})
}

fn write_mock(fx: &Fixture, answers: &serde_json::Value) -> PathBuf {
    let p = fx.dir.join("mock.json");
    std::fs::write(&p, serde_json::to_string(answers).unwrap()).unwrap();
    p
}

fn run_node(
    fx: &Fixture,
    args: &[&str],
    mock: &Path,
    stdin: Option<&str>,
) -> (i32, Vec<u8>, Vec<u8>) {
    let wrapper = fx.dir.join("wrap.mjs");
    let route = workspace_root().join("crates/agentdeck-core/assets/router/route.mjs");
    std::fs::write(
        &wrapper,
        format!(
            "import fs from 'node:fs';\nimport os from 'node:os';\nos.homedir = () => process.cwd();\n\
             globalThis.fetch = async () => ({{ ok: true, json: async () => JSON.parse(fs.readFileSync(process.env.MOCK_JEV, 'utf8')) }});\n\
             process.argv = [process.execPath, 'route.mjs', ...JSON.parse(process.env.ROUTE_ARGS)];\n\
             await import({});\n",
            serde_json::to_string(route.to_str().unwrap()).unwrap()
        ),
    )
    .unwrap();
    let mut cmd = Command::new("node");
    cmd.arg(&wrapper)
        .current_dir(&fx.dir)
        .envs(fx.env("rt-node"));
    cmd.env("ROUTE_ARGS", serde_json::to_string(args).unwrap());
    cmd.env("MOCK_JEV", mock);
    if stdin.is_some() {
        cmd.stdin(Stdio::piped());
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn node");
    if let Some(input) = stdin {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let out = child.wait_with_output().expect("node output");
    (out.status.code().unwrap_or(-1), out.stdout, out.stderr)
}

fn run_rust(
    fx: &Fixture,
    args: &[&str],
    mock: &Path,
    stdin: Option<&str>,
) -> (i32, Vec<u8>, Vec<u8>) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentdeck"));
    cmd.arg("route")
        .args(args)
        .current_dir(&fx.dir)
        .envs(fx.env("rt-rust"));
    cmd.env("AGENTDECK_ROUTE_MOCK_JEV", mock);
    if stdin.is_some() {
        cmd.stdin(Stdio::piped());
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn agentdeck");
    if let Some(input) = stdin {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let out = child.wait_with_output().expect("agentdeck output");
    (out.status.code().unwrap_or(-1), out.stdout, out.stderr)
}

fn check_case(
    name: &str,
    fx: &Fixture,
    args: &[&str],
    jev: &serde_json::Value,
    stdin: Option<&str>,
) {
    let mock = write_mock(fx, jev);
    let (nc, nout, _nerr) = run_node(fx, args, &mock, stdin);
    let (rc, rout, rerr) = run_rust(fx, args, &mock, stdin);
    assert_eq!(
        nc,
        rc,
        "{name}: exit codes differ (node {nc}, rust {rc}): {}",
        String::from_utf8_lossy(&rerr)
    );
    if nout == rout {
        return;
    }
    if args.contains(&"--check") {
        let actual = std::str::from_utf8(&rout).unwrap();
        let line = "user rules (source: default): no-review: gemini-.*argon; denied: (none)\n";
        assert!(actual.contains(line), "{name}: missing default rules");
        assert_eq!(actual.replacen(line, "", 1).as_bytes(), nout, "{name}: non-additive output differs");
        return;
    }
    // Additive-only allowance: the Rust router emits per-candidate
    // `quotaEstimate` provenance (estimates, never ranking). Strip it from
    // both sides, require everything else to match, then validate the new
    // field's shape on the Rust side.
    let strip = |bytes: &[u8]| -> Option<serde_json::Value> {
        let mut v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
        v.as_object_mut()?.remove("userRules");
        let ranking = v.get_mut("ranking")?.as_array_mut()?;
        for entry in ranking.iter_mut() {
            entry.as_object_mut()?.remove("quotaEstimate");
        }
        Some(v)
    };
    match (strip(&nout), strip(&rout)) {
        (Some(n), Some(r)) => {
            assert_eq!(n, r, "{name}: non-additive output differs");
            let rust_v: serde_json::Value = serde_json::from_slice(&rout).unwrap();
            assert_eq!(rust_v["userRules"], serde_json::json!({
                "source": "default", "noReviewModels": ["gemini-.*argon"], "denyModels": []
            }), "{name}: unexpected default user rules");
            let entries = rust_v["ranking"].as_array().unwrap();
            assert!(!entries.is_empty(), "{name}: empty ranking");
            for entry in entries {
                let q = &entry["quotaEstimate"];
                assert!(
                    ["measured", "estimate", "unknown"]
                        .contains(&q["basis"].as_str().unwrap_or("")),
                    "{name}: bad quotaEstimate basis: {q}"
                );
                assert!(q["detail"].is_string(), "{name}: bad detail: {q}");
                assert!(
                    q["loadUnknown"].is_boolean(),
                    "{name}: bad loadUnknown: {q}"
                );
                assert!(q["windows"].is_array(), "{name}: bad windows: {q}");
                assert!(q["balancesUsd"].is_array(), "{name}: bad balancesUsd: {q}");
            }
            return;
        }
        _ => {}
    }
    if nout != rout {
        let dir = std::env::temp_dir().join(format!("route-parity-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("node.json"), &nout).unwrap();
        std::fs::write(dir.join("rust.json"), &rout).unwrap();
    }
    assert_eq!(
        nout,
        rout,
        "{name}: stdout differs; see {}",
        std::env::temp_dir()
            .join(format!("route-parity-{name}"))
            .display()
    );
}

const OPUS: (&str, &str) = ("claudeAgent", "claude-opus-5-5");
const HAIKU: (&str, &str) = ("claudeAgent", "claude-haiku-5-5");
const SOL: (&str, &str) = ("codex", "gpt-6.1-sol");
const OAI_OPUS: (&str, &str) = ("opencode", "anthropic/claude-opus-5-5");
const HAIKU45: (&str, &str) = ("claudeAgent", "claude-haiku-4-5");

fn answers_for(
    pairs: &[(&str, serde_json::Value)],
    size: f64,
    difficulty: f64,
) -> serde_json::Value {
    let mut answers = serde_json::Map::new();
    answers.insert(
        "size".into(),
        serde_json::json!({"score": size, "confidence": 0.9}),
    );
    answers.insert(
        "difficulty".into(),
        serde_json::json!({"score": difficulty, "confidence": 0.9}),
    );
    for (key, v) in pairs {
        answers.insert(key.to_string(), v.clone());
    }
    serde_json::json!({"answers": answers})
}

#[test]
fn route_parity_with_node() {
    if !node_available() {
        eprintln!("SKIP: node is absent, cannot differentially test against the Node router");
        return;
    }
    let full_pool = [OPUS, HAIKU, SOL, OAI_OPUS, HAIKU45];
    let keys: Vec<String> = full_pool
        .iter()
        .map(|(p, m)| {
            format!("{p}/{m}")
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() {
                        c.to_ascii_lowercase()
                    } else {
                        '_'
                    }
                })
                .collect()
        })
        .collect();

    // Effort mapping per descriptor, difficulties 0..3 (winner content unchecked here, only parity).
    for (i, diff) in [0.0, 1.0, 2.0, 3.0].iter().enumerate() {
        let fx = Fixture::new(&full_pool);
        let jev = answers_for(
            &keys
                .iter()
                .map(|k| (k.as_str(), level(2.4, 0.95)))
                .collect::<Vec<_>>(),
            1.0,
            *diff,
        );
        check_case(
            &format!("effort-d{i}"),
            &fx,
            &["--source", "claudeAgent", "Do the task"],
            &jev,
            None,
        );
    }

    // Eligibility threshold: rounded fit 0.700 is still below 0.7.
    {
        let fx = Fixture::new(&[OPUS, HAIKU]);
        let jev = answers_for(
            &[
                ("claudeagent_claude_opus_5_5", level(2.409, 0.95)),
                ("claudeagent_claude_haiku_5_5", level(2.4, 0.6996)),
            ],
            0.0,
            0.0,
        );
        check_case(
            "fit-threshold",
            &fx,
            &["--source", "claudeAgent", "Fix the typo"],
            &jev,
            None,
        );
    }

    // Shared-quota penalty: a small quality edge does not spend shared quota.
    {
        let fx = Fixture::new(&[OPUS, SOL]);
        let jev = answers_for(
            &[
                ("claudeagent_claude_opus_5_5", level(2.9, 0.95)),
                ("codex_gpt_6_1_sol", level(2.4, 0.95)),
            ],
            0.0,
            0.0,
        );
        check_case(
            "shared-quota",
            &fx,
            &["--source", "claudeAgent", "Fix the typo"],
            &jev,
            None,
        );
    }

    // Cheap-model bonus at size 0, halved for hard tasks at difficulty 2.
    for (name, diff) in ["cheap-bonus", "hard-halving"].iter().zip([0.0, 2.0]) {
        let fx = Fixture::new(&[OPUS, HAIKU]);
        let jev = answers_for(
            &[
                ("claudeagent_claude_opus_5_5", level(2.4, 0.95)),
                ("claudeagent_claude_haiku_5_5", level(2.4, 0.95)),
            ],
            0.0,
            diff,
        );
        check_case(
            name,
            &fx,
            &["--source", "claudeAgent", "Fix the typo"],
            &jev,
            None,
        );
    }

    // Blocked sizes: an exhausted account cannot take the task.
    {
        let fx = Fixture::new(&[OPUS, SOL]);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64() as i64;
        let usage = serde_json::json!({"accounts": [
            {"provider": "claude", "id": "claude-main", "plan": "Max 5x", "status": "ok", "updated_at": now, "balances": [],
             "windows": [
                {"key": "weekly_all", "label": "Week", "remaining_percent": 0.05, "duration_mins": 10080, "resets_at": now + 30 * 3600},
                {"key": "session", "label": "5h", "remaining_percent": 0, "duration_mins": 300, "resets_at": now + 3600}]},
            {"provider": "codex", "id": "codex-main", "plan": "Pro 5x", "status": "ok", "updated_at": now, "balances": [],
             "windows": [{"key": "weekly", "label": "Week", "remaining_percent": 80, "duration_mins": 10080, "resets_at": now + 3 * 86400}]},
        ]});
        std::fs::write(
            fx.dir.join(".local/state/ai-usage/state.json"),
            serde_json::to_string(&usage).unwrap(),
        )
        .unwrap();
        let jev = answers_for(
            &[
                ("claudeagent_claude_opus_5_5", level(2.9, 0.95)),
                ("codex_gpt_6_1_sol", level(2.4, 0.95)),
            ],
            0.0,
            0.0,
        );
        check_case(
            "blocked",
            &fx,
            &["--source", "claudeAgent", "Fix the typo"],
            &jev,
            None,
        );
    }

    // Target validation: winner allowed, near-equal denied, unknown favorite denied.
    {
        let fx = Fixture::new(&[OPUS, HAIKU]);
        let jev = answers_for(
            &[
                ("claudeagent_claude_opus_5_5", level(2.4, 0.95)),
                ("claudeagent_claude_haiku_5_5", level(2.4, 0.95)),
            ],
            0.0,
            0.0,
        );
        check_case(
            "validate-winner",
            &fx,
            &[
                "--source",
                "claudeAgent",
                "--validate-target",
                "claudeAgent",
                "claude-haiku-5-5",
                "Fix the typo",
            ],
            &jev,
            None,
        );
        check_case(
            "validate-gap",
            &fx,
            &[
                "--source",
                "claudeAgent",
                "--validate-target",
                "claudeAgent",
                "claude-opus-5-5",
                "Fix the typo",
            ],
            &jev,
            None,
        );
        check_case(
            "validate-unknown",
            &fx,
            &[
                "--source",
                "claudeAgent",
                "--validate-target",
                "claudeAgent",
                "claude-sonnet-5-5",
                "Fix the typo",
            ],
            &jev,
            None,
        );
    }

    // Unknown source routes without protection; low size confidence warns.
    {
        let fx = Fixture::new(&[OPUS, HAIKU]);
        let jev = answers_for(
            &[
                ("claudeagent_claude_opus_5_5", level(2.4, 0.95)),
                ("claudeagent_claude_haiku_5_5", level(2.4, 0.95)),
            ],
            0.0,
            0.0,
        );
        check_case(
            "unknown-source",
            &fx,
            &["--source", "some_new_instance", "Fix the typo"],
            &jev,
            None,
        );
        let mut low = jev.clone();
        low["answers"]["size"] = serde_json::json!({"score": 0, "confidence": 0.3});
        check_case(
            "low-confidence",
            &fx,
            &["--source", "claudeAgent", "Fix the typo"],
            &low,
            None,
        );
    }

    // Task from stdin; --check text output.
    {
        let fx = Fixture::new(&[OPUS, HAIKU]);
        let jev = answers_for(
            &[
                ("claudeagent_claude_opus_5_5", level(2.4, 0.95)),
                ("claudeagent_claude_haiku_5_5", level(2.4, 0.95)),
            ],
            0.0,
            0.0,
        );
        check_case(
            "stdin-task",
            &fx,
            &["--source", "claudeAgent", "-"],
            &jev,
            Some("Fix the typo from stdin"),
        );
    }
    {
        let fx = Fixture::new(&[OPUS, HAIKU]);
        let jev = serde_json::json!({});
        check_case("check", &fx, &["--check"], &jev, None);
    }

    // Missing router JSON fails closed with the same message; bad Jev fails.
    {
        let fx = Fixture::new(&[OPUS]);
        let missing = fx.dir.join("no-such-router.json");
        let mock = write_mock(&fx, &serde_json::json!({}));
        let node = Command::new("node")
            .arg(workspace_root().join("crates/agentdeck-core/assets/router/route.mjs"))
            .args(["--source", "claudeAgent", "task"])
            .current_dir(&fx.dir)
            .envs(fx.env("rt-node"))
            .env("MODEL_ROUTE_ROUTER_JSON", &missing)
            .output()
            .unwrap();
        let rust = Command::new(env!("CARGO_BIN_EXE_agentdeck"))
            .args(["route", "--source", "claudeAgent", "task"])
            .current_dir(&fx.dir)
            .envs(fx.env("rt-rust"))
            .env("MODEL_ROUTE_ROUTER_JSON", &missing)
            .output()
            .unwrap();
        assert_eq!(node.status.code(), Some(1));
        assert_eq!(rust.status.code(), Some(1));
        assert!(node.stdout.is_empty() && rust.stdout.is_empty());
        // Fail-closed with the same message; only the trailing OS error text
        // is platform/Node-specific.
        for err in [&node.stderr, &rust.stderr] {
            assert!(
                String::from_utf8_lossy(err).contains("router JSON unusable"),
                "stderr: {err:?}"
            );
        }
        let _ = mock;
    }
    {
        let fx = Fixture::new(&[OPUS, HAIKU]);
        let bad = answers_for(
            &[
                ("claudeagent_claude_opus_5_5", level(9.0, 0.95)),
                ("claudeagent_claude_haiku_5_5", level(2.4, 0.95)),
            ],
            0.0,
            0.0,
        );
        let mock = write_mock(&fx, &bad);
        let (nc, nout, _) = run_node(
            &fx,
            &["--source", "claudeAgent", "Fix the typo"],
            &mock,
            None,
        );
        let (rc, rout, _) = run_rust(
            &fx,
            &["--source", "claudeAgent", "Fix the typo"],
            &mock,
            None,
        );
        assert_eq!((nc, rc), (1, 1));
        assert!(nout.is_empty() && rout.is_empty());
    }
}
