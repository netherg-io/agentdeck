//! Jev-gated Checkpoint judge (Q12, Q21): a fresh context window is allowed
//! only above a context threshold or at a milestone boundary, and only when
//! the judge confirms a clean boundary. Hard invariants run before any judge
//! so a stale state or a quiet turn never pays for a reset.

use std::path::Path;
use std::time::Duration;

use agentdeck_core::settings::{DEFAULT_DECISION_MODEL, PROVIDER_OPENROUTER, PROVIDER_VERCEL};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::CoordinatorConfig;

/// What the daemon knows about the run since the last Checkpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckpointInput {
    pub run_count_since_checkpoint: u32,
    pub item_count_since_checkpoint: u32,
    pub milestone_just_completed: bool,
    pub state_stale: bool,
    pub active_children: usize,
    pub recent_events_tail: Vec<String>,
}

/// Judge verdict. `should_checkpoint == false` on stale state is an
/// invariant, so the Watchdog can rely on `should_checkpoint &&
/// confidence >= threshold` alone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckpointVerdict {
    pub should_checkpoint: bool,
    pub confidence: f64,
    pub reason: String,
}

impl CheckpointVerdict {
    pub fn refuse(reason: &str) -> Self {
        Self {
            should_checkpoint: false,
            confidence: 1.0,
            reason: reason.to_string(),
        }
    }
}

/// Hard invariants, evaluated before (and instead of) any judge call:
/// state must be fresh, children must finish, and a quiet turn keeps its
/// prompt cache. Returns `Some` verdict when the invariants decide alone.
pub fn precheck(input: &CheckpointInput, cfg: &CoordinatorConfig) -> Option<CheckpointVerdict> {
    if input.state_stale {
        return Some(CheckpointVerdict::refuse(
            "state snapshot is stale; rewrite state before checkpoint",
        ));
    }
    if input.active_children > 0 {
        return Some(CheckpointVerdict {
            should_checkpoint: false,
            confidence: 0.9,
            reason: format!(
                "{} child agents still running: checkpoint after they land",
                input.active_children
            ),
        });
    }
    if !input.milestone_just_completed
        && input.run_count_since_checkpoint < cfg.checkpoint_soft_runs
        && input.item_count_since_checkpoint < cfg.checkpoint_soft_items
    {
        return Some(CheckpointVerdict {
            should_checkpoint: false,
            confidence: 0.95,
            reason: format!(
                "below soft thresholds ({}/{} runs, {}/{} items) with no milestone: preserving prompt cache",
                input.run_count_since_checkpoint,
                cfg.checkpoint_soft_runs,
                input.item_count_since_checkpoint,
                cfg.checkpoint_soft_items
            ),
        });
    }
    None
}

/// Mid-flight hazard markers: a Checkpoint here would strand work
/// (half-applied migrations, ongoing deploys, conflicted merges).
fn hazard_in(text: &str) -> Option<&'static str> {
    const HAZARDS: &[&str] = &[
        "merge conflict",
        "mid-rollback",
        "in progress deploy",
        "in-progress deploy",
        "half-applied",
        "rollback failed",
        "migration running",
        "deploying now",
        "rebase in progress",
        "uncommitted migration",
    ];
    let lower = text.to_lowercase();
    HAZARDS.iter().find(|h| lower.contains(**h)).copied()
}

/// `Send` is deliberately unbounded: the daemon awaits these futures
/// in place and never spawns them.
#[allow(async_fn_in_trait)]
pub trait CheckpointJudge {
    async fn judge(
        &self,
        input: &CheckpointInput,
        cfg: &CoordinatorConfig,
    ) -> anyhow::Result<CheckpointVerdict>;
}

/// Deterministic fallback/pre-filter: enforces the invariants plus the
/// hazard scan, approves clean milestones and over-threshold turns, and
/// otherwise keeps the cache. Used alone when no judge key is configured.
pub struct HeuristicCheckpointJudge;

impl HeuristicCheckpointJudge {
    fn decide(input: &CheckpointInput, cfg: &CoordinatorConfig) -> CheckpointVerdict {
        if let Some(v) = precheck(input, cfg) {
            return v;
        }
        for event in &input.recent_events_tail {
            if let Some(hazard) = hazard_in(event) {
                return CheckpointVerdict {
                    should_checkpoint: false,
                    confidence: 0.95,
                    reason: format!("mid-flight hazard ({hazard}): checkpoint would strand work"),
                };
            }
        }
        CheckpointVerdict {
            should_checkpoint: true,
            confidence: 0.85,
            reason: if input.milestone_just_completed {
                "clean milestone boundary with fresh state".to_string()
            } else {
                format!(
                    "over soft thresholds ({}/{} runs, {}/{} items) with no hazard markers",
                    input.run_count_since_checkpoint,
                    cfg.checkpoint_soft_runs,
                    input.item_count_since_checkpoint,
                    cfg.checkpoint_soft_items
                )
            },
        }
    }
}

impl CheckpointJudge for HeuristicCheckpointJudge {
    async fn judge(
        &self,
        input: &CheckpointInput,
        cfg: &CoordinatorConfig,
    ) -> anyhow::Result<CheckpointVerdict> {
        Ok(Self::decide(input, cfg))
    }
}

const CHECKPOINT_INSTRUCTIONS: &str =
    "Is this a clean, safe boundary to checkpoint (persist state and start a fresh context)?";
const CHECKPOINT_CRITERIA: [&str; 4] = [
    "Unsafe: work is mid-flight, state is stale, or children are still running.",
    "Risky: minor loose ends, but checkpointing would not lose work.",
    "Clean: a quiet stretch with nothing in flight.",
    "Ideal: a milestone boundary with fresh state and no loose ends.",
];

/// Live checkpoint judge over OpenRouter or Vercel AI Gateway.
pub struct HttpCheckpointJudge {
    pub endpoint: String,
    pub api_key: String,
    pub model: String,
    provider: String,
    client: reqwest::Client,
}

impl HttpCheckpointJudge {
    pub fn from_env(cfg: &CoordinatorConfig) -> anyhow::Result<Self> {
        let provider = cfg.judge_provider.as_deref().unwrap_or(PROVIDER_OPENROUTER);
        let (endpoint, env_keys): (&str, &[&str]) = match provider {
            PROVIDER_OPENROUTER => (
                "https://openrouter.ai/api/alpha/decisions",
                &["OPENROUTER_API_KEY", "JEV_OPENROUTER_API_KEY"],
            ),
            PROVIDER_VERCEL => (
                "https://ai-gateway.vercel.sh/v1/decisions",
                &["AI_GATEWAY_API_KEY"],
            ),
            _ => anyhow::bail!("unsupported checkpoint judge provider {provider:?}"),
        };
        let model = match cfg
            .judge_model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
        {
            Some(model) => model,
            None if provider == PROVIDER_VERCEL => {
                anyhow::bail!("judge_provider \"vercel\" requires an explicit judge_model");
            }
            None => DEFAULT_DECISION_MODEL,
        };
        let api_key = env_keys
            .iter()
            .filter_map(|name| std::env::var(name).ok())
            .map(|key| key.trim().to_string())
            .find(|key| !key.is_empty())
            .or_else(|| {
                let path = Path::new(&std::env::var_os("HOME")?)
                    .join(".config/model-route")
                    .join(format!("{provider}.key"));
                let key = std::fs::read_to_string(path).ok()?;
                let key = key.trim();
                (!key.is_empty()).then(|| key.to_string())
            })
            .ok_or_else(|| {
                anyhow::anyhow!("no API key configured for the {provider} checkpoint judge")
            })?;
        Ok(Self {
            endpoint: endpoint.to_string(),
            api_key,
            model: model.to_string(),
            provider: provider.to_string(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()?,
        })
    }

    fn request_body(&self, input: &CheckpointInput) -> Value {
        let state = json!({
                "runCountSinceCheckpoint": input.run_count_since_checkpoint,
                "itemCountSinceCheckpoint": input.item_count_since_checkpoint,
                "milestoneJustCompleted": input.milestone_just_completed,
                "activeChildren": input.active_children,
                "recentEventsTail": input.recent_events_tail.join("\n"),
        });
        if self.provider == PROVIDER_VERCEL {
            let levels: Vec<_> = CHECKPOINT_CRITERIA
                .iter()
                .enumerate()
                .map(|(i, description)| json!({"label": i.to_string(), "description": description}))
                .collect();
            return json!({
                "model": self.model,
                "input": json!({"state": state}).to_string(),
                "questions": [{
                    "type": "score", "name": "checkpoint_safe",
                    "instructions": CHECKPOINT_INSTRUCTIONS, "levels": levels,
                }],
            });
        }
        json!({
            "model": self.model,
            "state": state,
            "questions": {"checkpoint_safe": {
                "type": "score",
                "instructions": CHECKPOINT_INSTRUCTIONS,
                "criteria": CHECKPOINT_CRITERIA,
            }},
        })
    }

    fn request(&self, endpoint: &str, body: &Value) -> reqwest::RequestBuilder {
        self.client
            .post(endpoint)
            .bearer_auth(&self.api_key)
            .json(body)
    }

    /// Accept both OpenRouter's `{name: {score, confidence}}` map and
    /// Vercel's `[{name, score, confidence}]` array (refusals are dropped, so
    /// a missing entry is malformed, not a default).
    fn normalize_answer<'a>(&self, answers: &'a Value) -> anyhow::Result<&'a Value> {
        if let Some(a) = answers.get("checkpoint_safe") {
            return Ok(a);
        }
        if let Some(list) = answers.as_array()
            && let Some(a) = list.iter().find(|a| {
                a.get("name").and_then(Value::as_str) == Some("checkpoint_safe")
                    && a.get("type").and_then(Value::as_str) == Some("score")
            })
        {
            return Ok(a);
        }
        anyhow::bail!("judge returned no checkpoint_safe answer")
    }

    fn parse_answers(&self, answers: &Value) -> anyhow::Result<CheckpointVerdict> {
        let a = self.normalize_answer(answers)?;
        let score = match a.get("score").and_then(Value::as_f64) {
            Some(0.0) => 0,
            Some(1.0) => 1,
            Some(2.0) => 2,
            Some(3.0) => 3,
            _ => anyhow::bail!("checkpoint_safe score must be an integer in 0..=3"),
        };
        let confidence = if let Some(confidence) = a.get("confidence") {
            confidence.as_f64()
        } else if let Some(probabilities) = a.get("probabilities") {
            if let Some(list) = probabilities.as_array() {
                list.iter()
                    .find(|p| p.get("value").and_then(Value::as_u64) == Some(score as u64))
                    .and_then(|p| p.get("probability"))
                    .or_else(|| list.get(score))
                    .and_then(Value::as_f64)
            } else {
                probabilities.get(score.to_string()).and_then(Value::as_f64)
            }
        } else {
            Some(0.5)
        }
        .filter(|c| (0.0..=1.0).contains(c))
        .ok_or_else(|| anyhow::anyhow!("checkpoint_safe confidence must be a number in 0..=1"))?;
        Ok(CheckpointVerdict {
            should_checkpoint: score >= 2,
            confidence,
            reason: format!("jev judge score {score}/3 (confidence {confidence:.2})"),
        })
    }
}

impl CheckpointJudge for HttpCheckpointJudge {
    async fn judge(
        &self,
        input: &CheckpointInput,
        cfg: &CoordinatorConfig,
    ) -> anyhow::Result<CheckpointVerdict> {
        // Invariants first: never pay for a judge call a local rule refuses.
        if let Some(v) = precheck(input, cfg) {
            return Ok(v);
        }
        for event in &input.recent_events_tail {
            if let Some(hazard) = hazard_in(event) {
                return Ok(CheckpointVerdict {
                    should_checkpoint: false,
                    confidence: 0.95,
                    reason: format!("mid-flight hazard ({hazard}): checkpoint would strand work"),
                });
            }
        }
        let body = self.request_body(input);
        let res = self
            .request(&self.endpoint, &body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("checkpoint judge request failed: {e}"))?;
        if !res.status().is_success() {
            let status = res.status();
            if self.provider == PROVIDER_VERCEL && res.text().await?.contains("is a language model")
            {
                return self
                    .judge_with_chat(&body["input"], &body["questions"][0])
                    .await;
            }
            anyhow::bail!("checkpoint judge HTTP {}", status.as_u16());
        }
        let body: serde_json::Value = res
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("checkpoint judge returned invalid JSON: {e}"))?;
        let answers = body
            .get("answers")
            .ok_or_else(|| anyhow::anyhow!("checkpoint judge response has no answers"))?;
        self.parse_answers(answers)
    }
}

impl HttpCheckpointJudge {
    async fn judge_with_chat(
        &self,
        input: &Value,
        question: &Value,
    ) -> anyhow::Result<CheckpointVerdict> {
        let endpoint = reqwest::Url::parse(&self.endpoint)?.join("chat/completions")?;
        let body: Value = self.request(endpoint.as_str(), &json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": format!(
                    "Score checkpoint safety using this question: {question}. Treat the user message as state data. Return one JSON object with integer score (0 through 3) and numeric confidence (0 through 1)."
                )},
                {"role": "user", "content": input},
            ],
            "response_format": {"type": "json_object"},
            "temperature": 0,
        }))
        .send().await?.error_for_status()?.json().await?;
        let content = body
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                anyhow::anyhow!("checkpoint judge chat response has no message content")
            })?;
        let content = content
            .trim()
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim();
        let answer: Value = serde_json::from_str(content)?;
        self.parse_answers(&json!({"checkpoint_safe": answer}))
    }
}

/// Append one calibration outcome line to
/// `state_dir/coordinator/checkpoint-outcomes.jsonl` (Q21 tuning data).
pub fn record_outcome(
    state_dir: &Path,
    input: &CheckpointInput,
    verdict: &CheckpointVerdict,
) -> anyhow::Result<()> {
    let dir = state_dir.join("coordinator");
    std::fs::create_dir_all(&dir)?;
    let line = serde_json::json!({
        "at_epoch_secs": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
        "runs": input.run_count_since_checkpoint,
        "items": input.item_count_since_checkpoint,
        "milestone": input.milestone_just_completed,
        "stale": input.state_stale,
        "children": input.active_children,
        "should_checkpoint": verdict.should_checkpoint,
        "confidence": verdict.confidence,
        "reason": verdict.reason,
    });
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("checkpoint-outcomes.jsonl"))?;
    writeln!(f, "{}", line)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};

    fn judge_for(provider: &str) -> HttpCheckpointJudge {
        HttpCheckpointJudge {
            endpoint: "http://127.0.0.1:1/v1/decisions".to_string(),
            api_key: "test".to_string(),
            model: "test-model".to_string(),
            provider: provider.to_string(),
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
        }
    }

    fn sample_input() -> CheckpointInput {
        CheckpointInput {
            run_count_since_checkpoint: 90,
            item_count_since_checkpoint: 10,
            milestone_just_completed: false,
            state_stale: false,
            active_children: 0,
            recent_events_tail: Vec::new(),
        }
    }

    #[test]
    fn request_body_matches_provider_shape() {
        let openrouter = judge_for(PROVIDER_OPENROUTER).request_body(&sample_input());
        assert!(openrouter["questions"].is_object());
        assert!(openrouter["questions"]["checkpoint_safe"].is_object());
        let vercel = judge_for(PROVIDER_VERCEL).request_body(&sample_input());
        let input: serde_json::Value =
            serde_json::from_str(vercel["input"].as_str().expect("vercel input is a string"))
                .unwrap();
        assert_eq!(input["state"], openrouter["state"]);
        assert!(vercel.get("state").is_none());
        let list = vercel["questions"]
            .as_array()
            .expect("vercel questions are an array");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["name"], "checkpoint_safe");
        assert_eq!(list[0]["type"], "score");
    }

    #[test]
    fn vercel_requires_an_explicit_model() {
        let cfg = CoordinatorConfig {
            judge_provider: Some(PROVIDER_VERCEL.to_string()),
            judge_model: Some(" ".to_string()),
            ..Default::default()
        };
        let error = HttpCheckpointJudge::from_env(&cfg)
            .err()
            .expect("missing model");
        assert!(
            error
                .to_string()
                .contains("requires an explicit judge_model")
        );
    }

    #[tokio::test]
    async fn vercel_language_model_uses_chat_completions() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut judge = judge_for(PROVIDER_VERCEL);
        judge.endpoint = format!("http://{}/v1/decisions", listener.local_addr().unwrap());
        let (seen, requests) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            for (status, response) in [
                (
                    "400 Bad Request",
                    json!({"error": "test-model is a language model"}),
                ),
                (
                    "200 OK",
                    json!({"choices": [{"message": {"content": r#"{"score": 3, "confidence": 0.9}"#}}]}),
                ),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(&mut stream);
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                let mut len = None;
                loop {
                    let mut line = String::new();
                    assert_ne!(
                        reader.read_line(&mut line).unwrap(),
                        0,
                        "incomplete headers"
                    );
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.strip_prefix("content-length:") {
                        len = Some(value.trim().parse().unwrap());
                    }
                }
                let mut bytes = vec![0; len.unwrap()];
                reader.read_exact(&mut bytes).unwrap();
                seen.send((request, serde_json::from_slice::<Value>(&bytes).unwrap()))
                    .unwrap();
                let response = response.to_string();
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response}",
                    response.len()
                ).unwrap();
            }
        });
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            judge.judge(&sample_input(), &CoordinatorConfig::default()),
        )
        .await
        .unwrap()
        .unwrap();
        server.join().unwrap();
        assert!(result.should_checkpoint);
        assert_eq!(result.confidence, 0.9);
        let (route, decision) = requests.recv().unwrap();
        assert!(route.starts_with("POST /v1/decisions "));
        let (route, chat) = requests.recv().unwrap();
        assert!(route.starts_with("POST /v1/chat/completions "));
        assert_eq!(chat["model"], decision["model"]);
        assert_eq!(chat["messages"][1]["content"], decision["input"]);
    }

    #[test]
    fn parse_answers_uses_probability_at_the_selected_score() {
        let j = judge_for(PROVIDER_VERCEL);
        for probabilities in [
            serde_json::json!({"0": 0.05, "1": 0.05, "2": 0.9, "3": 0.0}),
            serde_json::json!([0.05, 0.05, 0.9, 0.0]),
            serde_json::json!([{"value": 2, "probability": 0.9}, {"value": 1, "probability": 0.1}]),
        ] {
            let verdict = j.parse_answers(&serde_json::json!([
                {"name": "checkpoint_safe", "type": "score", "score": 2, "probabilities": probabilities}
            ])).unwrap();
            assert_eq!(verdict.confidence, 0.9);
        }
    }

    #[test]
    fn parse_answers_accepts_both_provider_shapes() {
        let j = judge_for(PROVIDER_OPENROUTER);
        // OpenRouter map shape.
        let v = j
            .parse_answers(&serde_json::json!({
                "checkpoint_safe": {"score": 3.0, "confidence": 0.9}
            }))
            .unwrap();
        assert!(v.should_checkpoint);
        assert_eq!(v.confidence, 0.9);
        // Vercel array shape.
        let v = j
            .parse_answers(&serde_json::json!([
                {"name": "checkpoint_safe", "type": "score", "score": 1.0, "confidence": 0.8}
            ]))
            .unwrap();
        assert!(!v.should_checkpoint);
        // Missing confidence falls back to 0.5, never to a clamped garbage value.
        let v = j
            .parse_answers(&serde_json::json!({"checkpoint_safe": {"score": 2.0}}))
            .unwrap();
        assert!(v.should_checkpoint);
        assert_eq!(v.confidence, 0.5);
    }

    #[test]
    fn parse_answers_rejects_malformed_and_out_of_range() {
        let j = judge_for(PROVIDER_OPENROUTER);
        for bad in [
            // No answer at all (and refusals dropped from an array).
            serde_json::json!({}),
            serde_json::json!([]),
            serde_json::json!([{"name": "other", "score": 3.0}]),
            serde_json::json!([{"name": "checkpoint_safe", "type": "refusal", "score": 3.0, "confidence": 0.9}]),
            // Score of the wrong type or outside 0..=3.
            serde_json::json!({"checkpoint_safe": {"confidence": 0.9}}),
            serde_json::json!({"checkpoint_safe": {"score": "high", "confidence": 0.9}}),
            serde_json::json!({"checkpoint_safe": {"score": 5.0, "confidence": 0.9}}),
            serde_json::json!({"checkpoint_safe": {"score": -1.0, "confidence": 0.9}}),
            serde_json::json!({"checkpoint_safe": {"score": 1.4, "confidence": 2.0}}),
            serde_json::json!({"checkpoint_safe": {"score": 2.4, "confidence": 0.9}}),
            // Confidence of the wrong type or outside 0..=1.
            serde_json::json!({"checkpoint_safe": {"score": 3.0, "confidence": 1.5}}),
            serde_json::json!({"checkpoint_safe": {"score": 3.0, "confidence": "sure"}}),
        ] {
            assert!(j.parse_answers(&bad).is_err(), "{bad}");
        }
    }
}
