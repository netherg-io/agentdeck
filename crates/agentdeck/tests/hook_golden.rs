//! Golden-file test: the Rust `agentdeck route hook` output (stdout, exit
//! code, `decisions.jsonl` record) must match the committed snapshots in
//! `tests/golden/hook/` (captured once from the legacy Node `hook.mjs`;
//! `UPDATE_GOLDEN=1` rewrites them from the Rust output). Covers
//! allow/deny/hand-back across harnesses. No real API calls, no real key
//! file, no Node needed.

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
        .join("tests/golden/hook")
        .join(format!("{name}.json"))
}

/// Canonicalize run-specific drift (see route_golden.rs): wall-clock drift
/// between fixture setup and the run leaks into `resets in Xh` /
/// `resetsInHours`. JSON is re-serialized with sorted keys.
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

fn shared_hint() -> String {
    let roster: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            workspace_root().join("crates/agentdeck-core/assets/router/roster.json"),
        )
        .unwrap(),
    )
    .unwrap();
    roster["briefHint"].as_str().unwrap().to_string()
}

#[test]
fn embedded_muse_hint_matches_the_runtime_asset() {
    let roster: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            workspace_root().join("crates/agentdeck-core/assets/router/roster.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let family = roster["families"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["match"] == "muse-spark")
        .unwrap();
    let hint = family["briefHint"].as_str().unwrap();
    assert!(hint.starts_with("Long commands:"));
    assert!(hint.contains("PARTIAL:"));
    assert!(hint.contains("bounded waits apply only to local commands"));
    assert!(hint.contains("never to PRs, CI, review bots or merges"));
    assert!(!hint.to_lowercase().contains("skill"));
    assert_eq!(
        agentdeck_router::policy::hint_of("opencode-go/muse-spark-1.3-contributor"),
        Some(format!("{}\n\n{hint}", shared_hint()))
    );
}

#[test]
fn shared_hint_hands_pr_waits_to_the_coordinator() {
    for model in [
        "gpt-6.1-terra",
        "claude-opus-5-5",
        "opencode-go/muse-spark-1.3-contributor",
        "gemini-4-argon-high",
        "unprofiled/example",
    ] {
        let hint = agentdeck_router::policy::hint_of(model).unwrap();
        assert!(
            hint.contains("Do not wait for or poll CI, review bots or PR merges"),
            "{model}"
        );
        assert!(
            hint.contains("PR <url>, head <sha>, checks and reviews <state>"),
            "{model}"
        );
        assert!(hint.contains("The coordinator waits for PRs"), "{model}");
        assert!(hint.contains("a normal handoff, not BLOCKED"), "{model}");
        assert!(hint.contains("Waiting for local commands"), "{model}");
    }
}

struct Fixture {
    dir: PathBuf,
    mock_jev: PathBuf,
}

const SOL: (&str, &str) = ("codex", "gpt-6.1-sol");
const OPUS: (&str, &str) = ("claudeAgent", "claude-opus-5-5");

impl Fixture {
    fn new(
        pool: &[(&str, &str)],
        jev: &serde_json::Value,
        usage: Option<serde_json::Value>,
    ) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-hook-golden-{}-{}",
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
        let usage = usage.unwrap_or_else(|| serde_json::json!({"accounts": [
            {"provider": "claude", "id": "claude-main", "plan": "Max 5x", "status": "ok", "updated_at": now, "balances": [],
             "windows": [
                {"key": "weekly_all", "label": "Week", "remaining_percent": 60, "duration_mins": 10080, "resets_at": now + 6 * 86400 + 1800},
                {"key": "session", "label": "5h", "remaining_percent": 20, "duration_mins": 300, "resets_at": now + 2 * 3600}]},
            {"provider": "codex", "id": "codex-main", "plan": "Pro 5x", "status": "ok", "updated_at": now, "balances": [],
             "windows": [{"key": "weekly", "label": "Week", "remaining_percent": 80, "duration_mins": 10080, "resets_at": now + 3 * 86400}]},
        ]}));
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
                "codex": {"instances": ["codex", "codex_second"], "usageProvider": "codex", "usageId": "codex-main"}},
            "usageStatePath": dir.join(".local/state/ai-usage/state.json")});
        std::fs::write(
            dir.join("router.json"),
            serde_json::to_string(&router).unwrap(),
        )
        .unwrap();
        let mock_jev = dir.join("mock.json");
        std::fs::write(&mock_jev, serde_json::to_string(jev).unwrap()).unwrap();
        Self { dir, mock_jev }
    }

    fn base_env(&self, runtime: &str) -> Vec<(String, String)> {
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

    fn transcript(&self, name: &str, lines: &[&str]) -> String {
        let p = self.dir.join(name);
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();
        p.to_string_lossy().into_owned()
    }

    fn sqlite_nested_child(&self) {
        // One app-owned delegated child: session child-session is nested.
        let db = self.dir.join(".t3/userdata/statev2.sqlite");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE orchestration_v2_projection_subagents (subagent_id TEXT, child_thread_id TEXT);
             CREATE TABLE orchestration_v2_projection_provider_threads (provider_thread_id TEXT, thread_id TEXT, payload_json TEXT);
             INSERT INTO orchestration_v2_projection_subagents VALUES ('node:child', 'thread:child');
             INSERT INTO orchestration_v2_projection_provider_threads VALUES ('pt', 'thread:child', '{\"nativeThreadRef\":{\"nativeId\":\"child-session\"}}');",
        )
        .unwrap();
        conn.close().unwrap();
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

fn healthy_jev() -> serde_json::Value {
    answers_for(
        &[
            ("codex_gpt_6_1_sol", level(2.9, 0.95)),
            ("claudeagent_claude_opus_5_5", level(2.4, 0.95)),
        ],
        1.0,
        1.0,
    )
}

fn run_rust(fx: &Fixture, harness: &str, stdin_text: &str) -> (i32, Vec<u8>, Vec<u8>) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentdeck"));
    cmd.args(["route", "hook", "--harness", harness])
        .current_dir(&fx.dir)
        .envs(fx.base_env("rt-rust"));
    cmd.env("AGENTDECK_ROUTE_MOCK_JEV", &fx.mock_jev);
    for v in ["CLAUDE_CONFIG_DIR", "CODEX_HOME", "MODEL_ROUTE_HOOK_DEBUG"] {
        cmd.env_remove(v);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn agentdeck hook");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin_text.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("agentdeck hook output");
    (out.status.code().unwrap_or(-1), out.stdout, out.stderr)
}

fn decisions(dir: &Path) -> Vec<serde_json::Value> {
    let Ok(text) = std::fs::read_to_string(dir.join(".local/state/model-route/decisions.jsonl"))
    else {
        return vec![];
    };
    text.lines()
        .filter_map(|l| {
            serde_json::from_str::<serde_json::Value>(l)
                .ok()
                .map(|mut v| {
                    // Timestamps always differ; everything else must match.
                    v["t"] = serde_json::Value::from("");
                    // The embedded JSON parse error is engine-specific (V8 vs
                    // serde_json); only the shared suffix must match.
                    if let Some(reason) = v.get("reason").and_then(|r| r.as_str()) {
                        if let Some((_, suffix)) = reason.split_once(" | model-route:") {
                            v["reason"] = serde_json::Value::from(format!(
                                "<parse error> | model-route:{suffix}"
                            ));
                        }
                    }
                    // Wall-clock drift leaks into resets strings (see canon).
                    serde_json::from_slice(&canon_output(
                        &serde_json::to_vec(&v).unwrap(),
                    ))
                    .unwrap()
                })
        })
        .collect()
}

/// Node-shaped records for `UPDATE_GOLDEN=1`: strip the Rust-only
/// `quotaEstimate` provenance per ranking entry.
fn to_golden_records(records: &[serde_json::Value]) -> Vec<serde_json::Value> {
    let mut out = records.to_vec();
    for rec in out.iter_mut() {
        if let Some(ranking) = rec.get_mut("ranking").and_then(|v| v.as_array_mut()) {
            for entry in ranking.iter_mut() {
                if let Some(obj) = entry.as_object_mut() {
                    obj.remove("quotaEstimate");
                }
            }
        }
    }
    out
}

fn check_case(name: &str, fx: &Fixture, harness: &str, input: &serde_json::Value) {
    check_case_raw(name, fx, harness, &serde_json::to_string(input).unwrap());
}

fn check_case_raw(name: &str, fx: &Fixture, harness: &str, stdin_text: &str) {
    let path = golden_path(name);
    // The golden was captured once from the legacy Node hook; the golden
    // file is the contract and Node is no longer needed.
    if !path.exists() {
        panic!(
            "{name}: missing golden {}; regenerate with UPDATE_GOLDEN=1",
            path.display()
        );
    }
    let (rc, rout_raw, rerr) = run_rust(fx, harness, &stdin_text);
    let rout = canon_output(&rout_raw);
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        let golden = serde_json::json!({"exit": rc,
            "stdout": String::from_utf8(rout.clone()).unwrap(),
            "records": to_golden_records(&decisions(&fx.dir))});
        std::fs::write(&path, serde_json::to_string_pretty(&golden).unwrap() + "\n").unwrap();
    }
    let golden: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let nc = golden["exit"].as_i64().unwrap() as i32;
    let nout = golden["stdout"].as_str().unwrap().as_bytes().to_vec();
    let golden_records: Vec<serde_json::Value> =
        serde_json::from_value(golden["records"].clone()).unwrap();
    assert_eq!(
        nc, rc,
        "{name}: exit codes differ (golden {nc}, rust {rc}): {}",
        String::from_utf8_lossy(&rerr)
    );
    assert_eq!(
        String::from_utf8_lossy(&nout),
        String::from_utf8_lossy(&rout),
        "{name}: stdout differs"
    );
    // One fixture dir, one record per run: the golden record is the Node
    // record and must match the Rust record, up to the additive-only
    // `quotaEstimate` provenance per ranking entry (same allowance as
    // route_golden.rs: strip it from both sides, require everything else
    // to match, then validate the new field's shape on the Rust side).
    let rust_records = decisions(&fx.dir);
    if golden_records != rust_records {
        let mut n = golden_records.clone();
        let mut r = rust_records.to_vec();
        for records in [&mut n, &mut r] {
            for rec in records.iter_mut() {
                if let Some(ranking) = rec.get_mut("ranking").and_then(|v| v.as_array_mut()) {
                    for entry in ranking.iter_mut() {
                        if let Some(obj) = entry.as_object_mut() {
                            obj.remove("quotaEstimate");
                        }
                    }
                }
            }
        }
        assert_eq!(n, r, "{name}: non-quotaEstimate decisions.jsonl differs");
        for rec in &rust_records {
            if let Some(ranking) = rec.get("ranking").and_then(|v| v.as_array()) {
                for entry in ranking {
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
            }
        }
    }
}

fn delegate_input(
    tool: &str,
    task: &str,
    target: serde_json::Value,
    extra: serde_json::Value,
) -> serde_json::Value {
    let mut input = serde_json::json!({"tool_name": tool,
        "tool_input": {"task": task, "target": target}, "inT3": true});
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        input[k] = v;
    }
    input
}

#[test]
fn instance_capacity_blocks_launch_override_and_recommends_another_instance() {
    let fx = Fixture::new(&[SOL, OPUS], &healthy_jev(), None);
    let path = fx.dir.join("router.json");
    let mut cfg: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    cfg["instanceLimits"] = serde_json::json!({"codex": 1});
    std::fs::write(path, cfg.to_string()).unwrap();
    let sql = |file: &Path, schema: &str| {
        rusqlite::Connection::open(file)
            .unwrap()
            .execute_batch(schema)
            .unwrap();
    };
    let t3 = fx.dir.join(".t3/userdata/statev2.sqlite");
    sql(&t3, "CREATE TABLE orchestration_v2_projection_subagents (
        subagent_id TEXT, provider_instance_id TEXT, payload_json TEXT, started_at TEXT,
        origin TEXT, status TEXT, provider_thread_id TEXT, run_id TEXT, child_thread_id TEXT);
        CREATE TABLE orchestration_v2_projection_provider_threads (
        provider_thread_id TEXT, provider_instance_id TEXT, provider_session_id TEXT, status TEXT);
        CREATE TABLE orchestration_v2_projection_provider_sessions (provider_session_id TEXT, model TEXT, status TEXT);
        CREATE TABLE orchestration_v2_projection_provider_turns (provider_thread_id TEXT, run_attempt_id TEXT, status TEXT);
        CREATE TABLE orchestration_v2_projection_runs (run_id TEXT, provider_instance_id TEXT, provider_thread_id TEXT, payload_json TEXT, status TEXT, thread_id TEXT);
        CREATE TABLE orchestration_v2_projection_run_attempts (attempt_id TEXT, run_id TEXT);");
    let input = |task: &str| {
        delegate_input(
            "delegate_task",
            &format!("{task}\n\n{}", shared_hint()),
            serde_json::json!({"providerInstanceId":"codex","model":"gpt-6.1-sol", "options":{"reasoningEffort":"medium"}}),
            serde_json::json!({"source":"codex"}),
        )
    };
    let verdict = |input: serde_json::Value| {
        let (code, out, err) = run_rust(&fx, "shell", &input.to_string());
        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
        serde_json::from_slice::<serde_json::Value>(&out).unwrap()
    };
    assert_eq!(verdict(input("Fix typo"))["decision"], "allow");
    let denied = verdict(input("Another task"));
    assert_eq!(denied["decision"], "deny");
    assert!(
        denied["reason"]
            .as_str()
            .unwrap()
            .contains("instance parallel limit")
    );
    let rows = decisions(&fx.dir);
    assert_eq!(
        rows.last().unwrap()["target"]["providerInstanceId"],
        "claudeAgent"
    );
    let ranking = rows.last().unwrap()["ranking"].as_array().unwrap();
    let codex = ranking.iter().find(|r| r["provider"] == "codex").unwrap();
    assert_eq!(codex["eligible"], false);
    assert_eq!(codex["pendingLaunches"], 1);
    assert_eq!(codex["instanceActiveAgents"], 0);
    assert_eq!(codex["allowed"], true); // Quota is sufficient, capacity is not.
    // A deliberate model override still cannot exceed an instance's capacity.
    assert_eq!(
        verdict(input("[route:user-choice] Use GPT-6.1 Sol"))["decision"],
        "deny"
    );
    assert_eq!(
        decisions(&fx.dir).last().unwrap()["decision"],
        "deny:capacity"
    );
    sql(
        &fx.dir.join(".local/state/model-route/admissions.sqlite"),
        "UPDATE pending SET created = 0",
    );
    assert_eq!(
        verdict(input("Retry after failed dispatch"))["decision"],
        "allow"
    );
    sql(
        &fx.dir.join(".local/state/model-route/admissions.sqlite"),
        "DELETE FROM pending",
    );
    // A held queued run (left behind by a failed task) never starts, so it frees the slot.
    sql(
        &t3,
        "INSERT INTO orchestration_v2_projection_runs VALUES ('held', 'codex', NULL, '{\"modelSelection\":{\"model\":\"gpt-6-astra\"},\"queueHeld\":true}', 'queued', 'held')",
    );
    assert_eq!(verdict(input("With held run"))["decision"], "allow");
    sql(
        &fx.dir.join(".local/state/model-route/admissions.sqlite"),
        "DELETE FROM pending",
    );
    // An ordinary queued branch occupies the same instance slot, without a subagent.
    sql(
        &t3,
        "INSERT INTO orchestration_v2_projection_runs VALUES ('branch', 'codex', NULL, '{\"modelSelection\":{\"model\":\"gpt-6-astra\"}}', 'queued', 'branch')",
    );
    assert_eq!(verdict(input("With ordinary branch"))["decision"], "deny");
    std::fs::remove_file(&t3).unwrap();
    assert_eq!(verdict(input("Unknown load"))["decision"], "deny");
}

#[test]
fn hook_matches_golden() {
    let hint = shared_hint();
    let sol = serde_json::json!({"providerInstanceId": "codex", "model": "gpt-6.1-sol",
        "options": {"reasoningEffort": "medium"}});
    let hinted = |base: &str| format!("{base}\n\n{hint}");

    // 1. Shell delegate with the required hint: allow.
    {
        let fx = Fixture::new(&[SOL, OPUS], &healthy_jev(), None);
        let mut input = delegate_input(
            "delegate_task",
            &hinted("Fix the typo"),
            sol.clone(),
            serde_json::json!({}),
        );
        input["source"] = "codex".into();
        check_case("allow-delegate-shell", &fx, "shell", &input);
    }
    // 2. Same without the hint: deny:hint.
    {
        let fx = Fixture::new(&[SOL, OPUS], &healthy_jev(), None);
        let mut input = delegate_input(
            "delegate_task",
            "Fix the typo",
            sol.clone(),
            serde_json::json!({}),
        );
        input["source"] = "codex".into();
        check_case("deny-hint-shell", &fx, "shell", &input);
    }
    // 3. Delegate without a target: deny:invalid-delegate (no route call).
    {
        let fx = Fixture::new(&[SOL, OPUS], &healthy_jev(), None);
        let mut input = serde_json::json!({"tool_name": "delegate_task",
            "tool_input": {"task": "Fix the typo"}, "inT3": true});
        input["source"] = "codex".into();
        check_case("deny-invalid-shell", &fx, "shell", &input);
    }
    // 4. Shell source outside the live catalog: deny:shell-source.
    {
        let fx = Fixture::new(&[SOL, OPUS], &healthy_jev(), None);
        let mut input = delegate_input(
            "delegate_task",
            "Fix the typo",
            sol.clone(),
            serde_json::json!({}),
        );
        input["source"] = "spoofed".into();
        check_case("deny-shell-source", &fx, "shell", &input);
    }
    // 5. Unrelated tool passes through silently.
    {
        let fx = Fixture::new(&[SOL, OPUS], &healthy_jev(), None);
        let mut input = serde_json::json!({"tool_name": "update_plan",
            "tool_input": {"task": "x"}, "inT3": true});
        input["source"] = "codex".into();
        check_case("passthrough-shell", &fx, "shell", &input);
    }
    // 6. OpenCode native subagent: deny with a hand-back to delegate_task.
    {
        let fx = Fixture::new(&[SOL, OPUS], &healthy_jev(), None);
        let input = serde_json::json!({"tool_name": "subagent",
            "tool_input": {"message": "Do the thing"}, "session_id": "root", "inT3": true});
        check_case("deny-native-opencode", &fx, "opencode", &input);
    }
    // 7. OpenCode delegate with the hint: allow.
    {
        let fx = Fixture::new(&[SOL, OPUS], &healthy_jev(), None);
        let input = delegate_input(
            "delegate_task",
            &hinted("Fix the typo"),
            sol.clone(),
            serde_json::json!({}),
        );
        check_case("allow-delegate-opencode", &fx, "opencode", &input);
    }
    // 8. Workflow entry point: always deny (one route cannot validate it).
    {
        let fx = Fixture::new(&[SOL, OPUS], &healthy_jev(), None);
        let input = serde_json::json!({"tool_name": "Workflow",
            "tool_input": {"script": "agent('x')"}, "session_id": "root", "inT3": true});
        check_case("deny-workflow-opencode", &fx, "opencode", &input);
    }
    // 9. No adequate model with quota: deny:no-target.
    {
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
             "windows": [{"key": "weekly", "label": "Week", "remaining_percent": 0.05, "duration_mins": 10080, "resets_at": now + 3 * 86400}]},
        ]});
        let fx = Fixture::new(&[SOL, OPUS], &healthy_jev(), Some(usage));
        let input = serde_json::json!({"tool_name": "subagent",
            "tool_input": {"message": "Do the thing"}, "session_id": "root", "inT3": true});
        check_case("deny-no-target-opencode", &fx, "opencode", &input);
    }
    // 10. User-chosen quota-denied model with a naming transcript: allow.
    {
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
             "windows": [{"key": "weekly", "label": "Week", "remaining_percent": 0.05, "duration_mins": 10080, "resets_at": now + 3 * 86400}]},
        ]});
        let fx = Fixture::new(&[SOL, OPUS], &healthy_jev(), Some(usage));
        let t = fx.transcript(
            "transcript.jsonl",
            &[r#"{"type":"user","message":{"role":"user","content":"use sol for the audit"}}"#],
        );
        let mut input = delegate_input(
            "delegate_task",
            "[route:user-choice] Audit the auth flow",
            sol.clone(),
            serde_json::json!({}),
        );
        input["source"] = "codex".into();
        input["transcript_path"] = t.into();
        check_case("allow-user-choice", &fx, "shell", &input);
    }
    // 11. Same without a naming transcript: deny:user-choice.
    {
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
             "windows": [{"key": "weekly", "label": "Week", "remaining_percent": 0.05, "duration_mins": 10080, "resets_at": now + 3 * 86400}]},
        ]});
        let fx = Fixture::new(&[SOL, OPUS], &healthy_jev(), Some(usage));
        let t = fx.transcript(
            "transcript.jsonl",
            &[r#"{"type":"user","message":{"role":"user","content":"make it cheaper"}}"#],
        );
        let mut input = delegate_input(
            "delegate_task",
            "[route:user-choice] Audit the auth flow",
            sol.clone(),
            serde_json::json!({}),
        );
        input["source"] = "codex".into();
        input["transcript_path"] = t.into();
        check_case("deny-user-choice", &fx, "shell", &input);
    }
    // 12. Valid target with a foreign reasoning knob: deny:reasoning.
    {
        let fx = Fixture::new(&[SOL, OPUS], &healthy_jev(), None);
        let bad = serde_json::json!({"providerInstanceId": "codex", "model": "gpt-6.1-sol",
            "options": {"effort": "medium"}});
        let mut input = delegate_input(
            "delegate_task",
            &hinted("Fix the typo"),
            bad,
            serde_json::json!({}),
        );
        input["source"] = "codex".into();
        check_case("deny-reasoning", &fx, "shell", &input);
    }
    // 13. Review role on a no-review family: deny even with the hint.
    {
        let fx = Fixture::new(&[SOL, OPUS], &healthy_jev(), None);
        let argon = serde_json::json!({"providerInstanceId": "antigravity", "model": "gemini-4-argon-medium"});
        let mut input = delegate_input(
            "delegate_task",
            &hinted("Audit the auth flow"),
            argon,
            serde_json::json!({"role": "review"}),
        );
        input["source"] = "antigravity".into();
        check_case("deny-review-role", &fx, "shell", &input);
    }
    // 14. Malformed stdin: fail closed (legacy Node agreed byte-for-byte;
    // the golden pins the exact denial).
    {
        let fx = Fixture::new(&[SOL, OPUS], &healthy_jev(), None);
        check_case_raw("malformed", &fx, "shell", "{");
        let golden: serde_json::Value = serde_json::from_slice(
            &std::fs::read(golden_path("malformed")).unwrap(),
        )
        .unwrap();
        assert!(
            golden["stdout"].as_str().unwrap().starts_with("{\"decision\":\"deny\""),
            "must deny: {}",
            golden["stdout"].as_str().unwrap()
        );
    }
    // 15. Delegated child session: nested delegation is disabled.
    {
        let fx = Fixture::new(&[SOL, OPUS], &healthy_jev(), None);
        fx.sqlite_nested_child();
        let input = serde_json::json!({"tool_name": "subagent",
            "tool_input": {"message": "Do the thing"}, "session_id": "child-session",
            "threadId": "thread:child", "inT3": true});
        check_case("deny-nested-opencode", &fx, "opencode", &input);
    }
    // 16. Low Jev confidence is visible but still allows.
    {
        let mut jev = healthy_jev();
        jev["answers"]["size"] = serde_json::json!({"score": 1.0, "confidence": 0.3});
        let fx = Fixture::new(&[SOL, OPUS], &jev, None);
        let mut input = delegate_input(
            "delegate_task",
            &hinted("Fix the typo"),
            sol.clone(),
            serde_json::json!({}),
        );
        input["source"] = "codex".into();
        check_case("allow-low-confidence", &fx, "shell", &input);
    }
}
