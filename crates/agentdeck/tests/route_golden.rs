//! Golden-file test: the Rust `agentdeck route` output must match the committed
//! snapshots in `tests/golden/route/` (captured once from the legacy Node
//! `route.mjs`; `UPDATE_GOLDEN=1` rewrites them from the Rust output).
//! No real API calls, no real key file, no Node needed.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn workspace_root() -> PathBuf {
    let mut d = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    d.pop();
    d.pop();
    d
}

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/route")
        .join(format!("{name}.json"))
}

/// Canonicalize run-specific drift so goldens compare equal across runs:
/// wall-clock drift between fixture setup and the run leaks into
/// `resets in Xh` / `resetsInHours`. JSON outputs are re-serialized with
/// sorted keys; text outputs keep their bytes.
fn canon_output(bytes: &[u8]) -> Vec<u8> {
    let s = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(s.len());
    let mut rest = s.as_ref();
    while let Some(i) = rest.find("resets in ") {
        let head = &rest[..i + "resets in ".len()];
        let tail = &rest[i + "resets in ".len()..];
        let num_end = tail
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(tail.len());
        if num_end > 0 && tail[num_end..].starts_with('h') {
            out.push_str(head);
            out.push_str("Rh");
            rest = &tail[num_end + 1..];
        } else {
            out.push_str(head);
            rest = tail;
        }
    }
    out.push_str(rest);
    match serde_json::from_str::<serde_json::Value>(&out) {
        Ok(mut v) => {
            fn zero_resets(v: &mut serde_json::Value) {
                match v {
                    serde_json::Value::Object(o) => {
                        for (k, val) in o.iter_mut() {
                            if k == "resetsInHours" {
                                *val = serde_json::json!(0);
                            } else {
                                zero_resets(val);
                            }
                        }
                    }
                    serde_json::Value::Array(a) => a.iter_mut().for_each(zero_resets),
                    _ => {}
                }
            }
            zero_resets(&mut v);
            serde_json::to_vec(&v).unwrap()
        }
        Err(_) => out.into_bytes(),
    }
}

struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(pool: &[(&str, &str)]) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-route-golden-{}-{}",
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
            "rt-rust",
        ] {
            std::fs::create_dir_all(dir.join(rel)).unwrap();
        }
        std::fs::copy(
            workspace_root().join("crates/agentdeck/tests/fixtures/catalog.fixture.json"),
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

/// Map Rust output back to the legacy Node shape for `UPDATE_GOLDEN=1`:
/// drop the additive-only default-rules line (`--check`) or the Rust-only
/// `userRules`/`quotaEstimate` provenance (JSON), then canonicalize.
fn to_golden_shape(args: &[&str], rout: &[u8]) -> Vec<u8> {
    if args.contains(&"--check") {
        let actual = std::str::from_utf8(rout).unwrap();
        let line = "user rules (source: default): no-review: gemini-.*argon; denied: (none)\n";
        return canon_output(actual.replacen(line, "", 1).as_bytes());
    }
    let mut v: serde_json::Value = serde_json::from_slice(rout).unwrap();
    v.as_object_mut().unwrap().remove("userRules");
    for entry in v["ranking"].as_array_mut().unwrap() {
        entry.as_object_mut().unwrap().remove("quotaEstimate");
    }
    canon_output(&serde_json::to_vec(&v).unwrap())
}

fn check_case(
    name: &str,
    fx: &Fixture,
    args: &[&str],
    jev: &serde_json::Value,
    stdin: Option<&str>,
) {
    let mock = write_mock(fx, jev);
    let (rc, raw_out, rerr) = run_rust(fx, args, &mock, stdin);
    let rout = canon_output(&raw_out);
    let path = golden_path(name);
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        let golden = serde_json::json!({"exit": rc, "stdout": String::from_utf8(to_golden_shape(args, &rout)).unwrap()});
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(&golden).unwrap() + "\n").unwrap();
    }
    // The golden was captured once from the legacy Node router; the golden
    // file is the contract and Node is no longer needed.
    if !path.exists() {
        panic!(
            "{name}: missing golden {}; regenerate with UPDATE_GOLDEN=1",
            path.display()
        );
    }
    let golden: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let nc = golden["exit"].as_i64().unwrap() as i32;
    let nout = golden["stdout"].as_str().unwrap().as_bytes().to_vec();
    assert_eq!(
        nc,
        rc,
        "{name}: exit codes differ (golden {nc}, rust {rc}): {}",
        String::from_utf8_lossy(&rerr)
    );
    if nout == rout {
        return;
    }
    if args.contains(&"--check") {
        let actual = std::str::from_utf8(&rout).unwrap();
        let line = "user rules (source: default): no-review: gemini-.*argon; denied: (none)\n";
        assert!(actual.contains(line), "{name}: missing default rules");
        assert_eq!(
            actual.replacen(line, "", 1).as_bytes(),
            nout,
            "{name}: non-additive output differs"
        );
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
    if let (Some(n), Some(r)) = (strip(&nout), strip(&rout)) {
        assert_eq!(n, r, "{name}: non-additive output differs");
        let rust_v: serde_json::Value = serde_json::from_slice(&rout).unwrap();
        assert_eq!(
            rust_v["userRules"],
            serde_json::json!({
                "source": "default", "noReviewModels": ["gemini-.*argon"], "denyModels": []
            }),
            "{name}: unexpected default user rules"
        );
        let entries = rust_v["ranking"].as_array().unwrap();
        assert!(!entries.is_empty(), "{name}: empty ranking");
        for entry in entries {
            let q = &entry["quotaEstimate"];
            assert!(
                ["measured", "estimate", "unknown"].contains(&q["basis"].as_str().unwrap_or("")),
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
    if nout != rout {
        let dir = std::env::temp_dir().join(format!("route-golden-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("golden.json"), &nout).unwrap();
        std::fs::write(dir.join("rust.json"), &rout).unwrap();
    }
    assert_eq!(
        nout,
        rout,
        "{name}: stdout differs; see {}",
        std::env::temp_dir()
            .join(format!("route-golden-{name}"))
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
fn route_matches_golden() {
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

    // Effort mapping per descriptor, difficulties 0..3 (winner content unchecked here).
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
        check_case(
            "check",
            &fx,
            &["--source", "claudeAgent", "--check"],
            &jev,
            None,
        );
    }

    // Missing router JSON fails closed; bad Jev fails (legacy Node asserted
    // the same exit codes and the shared "router JSON unusable" message).
    {
        let fx = Fixture::new(&[OPUS]);
        let missing = fx.dir.join("no-such-router.json");
        let mock = write_mock(&fx, &serde_json::json!({}));
        let rust = Command::new(env!("CARGO_BIN_EXE_agentdeck"))
            .args(["route", "--source", "claudeAgent", "task"])
            .current_dir(&fx.dir)
            .envs(fx.env("rt-rust"))
            .env("MODEL_ROUTE_ROUTER_JSON", &missing)
            .output()
            .unwrap();
        assert_eq!(rust.status.code(), Some(1));
        assert!(rust.stdout.is_empty());
        // Fail-closed with the shared message; only the trailing OS error
        // text is platform-specific.
        assert!(
            String::from_utf8_lossy(&rust.stderr).contains("router JSON unusable"),
            "stderr: {:?}",
            rust.stderr
        );
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
        let (rc, rout, _) = run_rust(
            &fx,
            &["--source", "claudeAgent", "Fix the typo"],
            &mock,
            None,
        );
        assert_eq!(rc, 1);
        assert!(rout.is_empty());
    }
}

/// `--effort` replaces the JEV difficulty only for the reasoning level.
#[test]
fn effort_flag_overrides_the_reasoning_level() {
    let pool = [OPUS, SOL];
    let keys = ["claudeagent_claude_opus_5_5", "codex_gpt_6_1_sol"];
    let target = |diff: f64, flags: &[&str]| {
        let fx = Fixture::new(&pool);
        let jev = answers_for(
            &keys
                .iter()
                .map(|k| (*k, level(2.4, 0.95)))
                .collect::<Vec<_>>(),
            1.0,
            diff,
        );
        let mock = write_mock(&fx, &jev);
        let mut args = vec!["--source", "claudeAgent"];
        args.extend_from_slice(flags);
        args.push("Do the task");
        let (code, out, _) = run_rust(&fx, &args, &mock, None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap_or_default();
        (code, v["target"].clone())
    };
    let (_, base_low) = target(0.0, &[]);
    let (_, base_high) = target(2.0, &[]);
    assert_ne!(base_low, base_high, "fixture must distinguish levels");
    assert_eq!(target(0.0, &["--effort", "xhigh"]).1, base_high);
    assert_eq!(target(3.0, &["--effort", "medium"]).1, base_low);
    assert_eq!(target(0.0, &["--effort", "bogus"]).0, 2);
}

/// Jev answer cache through the actual CLI (ported from the legacy Node
/// `jev-cache.test.mjs`): miss then hit, validation reuses the cached
/// answers, quota is re-read on a hit, corrupt entries refetch, private
/// cache permissions.
#[test]
fn jev_cache_contract() {
    use std::os::unix::fs::PermissionsExt;
    let pool = [OPUS, SOL];
    let fx = Fixture::new(&pool);
    let prefers_opus = answers_for(
        &[
            ("claudeagent_claude_opus_5_5", level(2.9, 0.95)),
            ("codex_gpt_6_1_sol", level(2.0, 0.95)),
        ],
        0.0,
        0.0,
    );
    let mock = write_mock(&fx, &prefers_opus);
    let task = [
        "--source",
        "claudeAgent",
        "Implement the feature in parser.c",
    ];
    let run = |args: &[&str]| {
        let (code, out, err) = run_rust(&fx, args, &mock, None);
        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
        serde_json::from_slice::<serde_json::Value>(&out).unwrap()
    };
    let out = run(&task);
    assert_eq!(out["jevCache"], "miss");
    assert_eq!(out["target"]["model"], "claude-opus-5-5");
    let cache_dir = fx.dir.join("rt-rust/model-route-jev-cache");
    let files: Vec<_> = std::fs::read_dir(&cache_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.ends_with(".json"))
        .collect();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].len(), 64 + ".json".len());
    assert_eq!(
        std::fs::metadata(cache_dir.join(&files[0]))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(&cache_dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    // A fresh sample would prefer Sol; validation reuses the cached answers.
    let out = run(&[
        "--source",
        "claudeAgent",
        "--validate-target",
        "claudeAgent",
        "claude-opus-5-5",
        "Implement the feature in parser.c",
    ]);
    assert_eq!(out["jevCache"], "hit");
    assert_eq!(out["validation"]["allowed"], true);
    // Quota is re-read on a hit: an exhausted winner is blocked, still
    // without a new Jev request (same single cache file).
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64() as i64;
    let usage = serde_json::json!({"accounts": [
        {"provider": "claude", "id": "claude-main", "plan": "Max 5x", "status": "ok", "updated_at": now, "balances": [],
         "windows": [{"key": "weekly_all", "label": "Week", "remaining_percent": 0.05, "duration_mins": 10080, "resets_at": now + 30 * 3600},
            {"key": "session", "label": "5h", "remaining_percent": 0, "duration_mins": 300, "resets_at": now + 3600}]},
        {"provider": "codex", "id": "codex-main", "plan": "Pro 5x", "status": "ok", "updated_at": now, "balances": [],
         "windows": [{"key": "weekly", "label": "Week", "remaining_percent": 80, "duration_mins": 10080, "resets_at": now + 3 * 86400}]},
    ]});
    std::fs::write(
        fx.dir.join(".local/state/ai-usage/state.json"),
        serde_json::to_string(&usage).unwrap(),
    )
    .unwrap();
    let out = run(&[
        "--source",
        "claudeAgent",
        "--validate-target",
        "claudeAgent",
        "claude-opus-5-5",
        "Implement the feature in parser.c",
    ]);
    assert_eq!(out["jevCache"], "hit");
    assert_eq!(out["validation"]["allowed"], false);
    assert_eq!(out["target"]["model"], "gpt-6.1-sol");
    // Corrupt cache entries refetch instead of failing.
    std::fs::write(cache_dir.join(&files[0]), "{").unwrap();
    let out = run(&task);
    assert_eq!(out["jevCache"], "miss");
    assert_eq!(out["target"]["model"], "gpt-6.1-sol");
}

/// `--check` over cached quota states (ported from the legacy Node
/// `policy.test.mjs` CLI tail): stale/error snapshots report cached quota
/// and gate sizes; unknown quota allows all sizes.
#[test]
fn check_reports_cached_quota_states() {
    let fx = Fixture::new(&[OPUS]);
    let run_check = |usage: serde_json::Value| {
        std::fs::write(
            fx.dir.join(".local/state/ai-usage/state.json"),
            serde_json::to_string(&usage).unwrap(),
        )
        .unwrap();
        let mock = write_mock(&fx, &serde_json::json!({}));
        let (code, out, _) = run_rust(&fx, &["--check"], &mock, None);
        assert_eq!(code, 0);
        String::from_utf8(out).unwrap()
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64() as i64;
    let cached = |status: &str| {
        serde_json::json!({"accounts": [{
            "provider": "claude", "id": "claude-main", "plan": "Max 5x",
            "status": status, "error": "quota HTTP 429", "updated_at": now - 2400,
            "balances": [], "windows": [
                {"key": "weekly_all", "label": "Week", "remaining_percent": 0.05,
                 "duration_mins": 10080, "resets_at": now + 30 * 3600}]}]})
    };
    for status in ["ok", "stale", "error"] {
        let output = run_check(cached(status));
        assert!(output.contains("allowed sizes: none"), "{status}: {output}");
        if status != "ok" {
            assert!(output.contains("cached quota"), "{status}: {output}");
        }
    }
    let mut unknown = cached("ok");
    unknown["accounts"][0]["windows"] = serde_json::json!([]);
    unknown["accounts"][0]["balances"] = serde_json::json!([]);
    let output = run_check(unknown);
    assert!(output.contains("quota unknown"), "{output}");
    assert!(output.contains("allowed sizes: 0,1,2,3"), "{output}");
}

/// `--catalog` install contract (ported from the legacy Node
/// `policy.test.mjs` CLI tail): providers are saved with private
/// permissions; bad input fails without touching the file.
#[test]
fn catalog_save_contract() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fixture::new(&[OPUS]);
    let catalog_path = fx.dir.join(".local/state/model-route/catalog.json");
    let fixture: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            workspace_root().join("crates/agentdeck/tests/fixtures/catalog.fixture.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let mock = write_mock(&fx, &serde_json::json!({}));
    let run = |stdin: &str| run_rust(&fx, &["--catalog"], &mock, Some(stdin));
    let (code, _, _) = run(&serde_json::to_string(&fixture).unwrap());
    assert_eq!(code, 0);
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&catalog_path).unwrap()).unwrap();
    assert_eq!(saved["providers"], fixture["providers"]);
    assert_eq!(
        std::fs::metadata(&catalog_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let before = std::fs::read(&catalog_path).unwrap();
    let (code, _, _) = run("{}");
    assert_ne!(code, 0);
    assert_eq!(std::fs::read(&catalog_path).unwrap(), before);
}
