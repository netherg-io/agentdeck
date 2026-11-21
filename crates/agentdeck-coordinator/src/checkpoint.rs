//! Jev-gated Checkpoint judge (Q12, Q21): a fresh context window is allowed
//! only above a context threshold or at a milestone boundary, and only when
//! the judge confirms a clean boundary. Hard invariants run before any judge
//! so a stale state or a quiet turn never pays for a reset.

use std::path::Path;

use serde::{Deserialize, Serialize};

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
/// stale state must be rewritten first, and a quiet turn keeps its prompt
/// cache. Returns `Some` verdict when the invariants decide alone.
pub fn precheck(input: &CheckpointInput, cfg: &CoordinatorConfig) -> Option<CheckpointVerdict> {
    if input.state_stale {
        return Some(CheckpointVerdict::refuse(
            "state snapshot is stale; rewrite state before checkpoint",
        ));
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
                    reason: format!(
                        "mid-flight hazard ({hazard}): checkpoint would strand work"
                    ),
                };
            }
        }
        if input.active_children > 0 {
            return CheckpointVerdict {
                should_checkpoint: false,
                confidence: 0.9,
                reason: format!(
                    "{} child agents still running: checkpoint after they land",
                    input.active_children
                ),
            };
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

/// Live Jev judge over OpenRouter (`JEV_OPENROUTER_API_KEY`) or the Vercel
/// AI Gateway (`AI_GATEWAY_API_KEY`), mirroring `agentdeck-router`'s
/// decision endpoint. No model settings are added: one score question over
/// the already size-checked input summary.
pub struct HttpCheckpointJudge {
    pub endpoint: String,
    pub api_key: String,
    pub model: String,
}

impl HttpCheckpointJudge {
    pub fn from_env(cfg: &CoordinatorConfig) -> anyhow::Result<Self> {
        let provider = cfg.judge_provider.as_deref().unwrap_or("openrouter");
        let model = cfg
            .judge_model
            .clone()
            .unwrap_or_else(|| "jev-decision-1".to_string());
        if provider == "vercel" {
            let key = std::env::var("AI_GATEWAY_API_KEY")
                .ok()
                .filter(|k| !k.is_empty())
                .ok_or_else(|| anyhow::anyhow!("AI_GATEWAY_API_KEY is not set"))?;
            return Ok(Self {
                endpoint: "https://ai-gateway.vercel.sh/v1/decisions".to_string(),
                api_key: key,
                model,
            });
        }
        let key = std::env::var("JEV_OPENROUTER_API_KEY")
            .ok()
            .filter(|k| !k.is_empty())
            .ok_or_else(|| anyhow::anyhow!("JEV_OPENROUTER_API_KEY is not set"))?;
        Ok(Self {
            endpoint: "https://openrouter.ai/api/alpha/decisions".to_string(),
            api_key: key,
            model,
        })
    }

    fn request_body(&self, input: &CheckpointInput) -> serde_json::Value {
        let question = serde_json::json!({
            "type": "score",
            "instructions": "Is this a clean, safe boundary to checkpoint (persist state and start a fresh context)?",
            "criteria": [
                "Unsafe: work is mid-flight, state is stale, or children are still running.",
                "Risky: minor loose ends, but checkpointing would not lose work.",
                "Clean: a quiet stretch with nothing in flight.",
                "Ideal: a milestone boundary with fresh state and no loose ends.",
            ],
        });
        // Vercel's decisions endpoint takes an array of named questions (same
        // shape `agentdeck-router` sends); OpenRouter takes the question map.
        let questions = if self.endpoint.contains("vercel") {
            let levels: Vec<serde_json::Value> = question
                .get("criteria")
                .and_then(serde_json::Value::as_array)
                .map(|cs| {
                    cs.iter()
                        .enumerate()
                        .map(|(i, c)| serde_json::json!({"label": i.to_string(), "description": c}))
                        .collect()
                })
                .unwrap_or_default();
            serde_json::json!([{
                "type": "score",
                "name": "checkpoint_safe",
                "instructions": question.get("instructions"),
                "levels": levels,
            }])
        } else {
            serde_json::json!({ "checkpoint_safe": question })
        };
        serde_json::json!({
            "model": self.model,
            "state": {
                "runCountSinceCheckpoint": input.run_count_since_checkpoint,
                "itemCountSinceCheckpoint": input.item_count_since_checkpoint,
                "milestoneJustCompleted": input.milestone_just_completed,
                "activeChildren": input.active_children,
                "recentEventsTail": input.recent_events_tail.join("\n"),
            },
            "questions": questions,
        })
    }

    /// Accept both OpenRouter's `{name: {score, confidence}}` map and
    /// Vercel's `[{name, score, confidence}]` array (refusals are dropped, so
    /// a missing entry is malformed, not a default).
    fn normalize_answer<'a>(&self, answers: &'a serde_json::Value) -> anyhow::Result<&'a serde_json::Value> {
        if let Some(a) = answers.get("checkpoint_safe") {
            return Ok(a);
        }
        if let Some(list) = answers.as_array()
            && let Some(a) = list
                .iter()
                .find(|a| a.get("name").and_then(serde_json::Value::as_str) == Some("checkpoint_safe"))
        {
            return Ok(a);
        }
        anyhow::bail!("judge returned no checkpoint_safe answer: {answers}")
    }

    fn parse_answers(&self, answers: &serde_json::Value) -> anyhow::Result<CheckpointVerdict> {
        let a = self.normalize_answer(answers)?;
        // Raw ranges are validated, never clamped: a 1.4 binary-style score
        // or an out-of-range confidence is malformed and must fail loudly.
        let score = a.get("score").and_then(serde_json::Value::as_f64).ok_or_else(|| {
            anyhow::anyhow!("checkpoint_safe answer has no score: {a}")
        })?;
        if !score.is_finite() || !(0.0..=3.0).contains(&score) {
            anyhow::bail!("checkpoint_safe score {score} out of range 0..=3: {a}");
        }
        let confidence = match a.get("confidence") {
            None => 0.5,
            Some(v) => v.as_f64().filter(|c| c.is_finite() && (0.0..=1.0).contains(c)).ok_or_else(|| {
                anyhow::anyhow!("checkpoint_safe confidence out of range 0..=1: {a}")
            })?,
        };
        Ok(CheckpointVerdict {
            should_checkpoint: score >= 2.0,
            confidence,
            reason: format!("jev judge score {score:.0}/3 (confidence {confidence:.2})"),
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
                    reason: format!(
                        "mid-flight hazard ({hazard}): checkpoint would strand work"
                    ),
                });
            }
        }
        let res = reqwest::Client::new()
            .post(&self.endpoint)
            .timeout(std::time::Duration::from_secs(30))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .body(self.request_body(input).to_string())
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("checkpoint judge request failed: {e}"))?;
        if !res.status().is_success() {
            anyhow::bail!("checkpoint judge HTTP {}", res.status().as_u16());
        }
        let body: serde_json::Value = res
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("checkpoint judge returned invalid JSON: {e}"))?;
        let answers = body.get("answers").ok_or_else(|| {
            anyhow::anyhow!("checkpoint judge response has no answers: {body}")
        })?;
        self.parse_answers(answers)
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

    fn judge_for(endpoint: &str) -> HttpCheckpointJudge {
        HttpCheckpointJudge {
            endpoint: endpoint.to_string(),
            api_key: "test".to_string(),
            model: "jev-decision-1".to_string(),
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
        let openrouter = judge_for("https://openrouter.ai/api/alpha/decisions")
            .request_body(&sample_input());
        assert!(openrouter["questions"].is_object());
        assert!(openrouter["questions"]["checkpoint_safe"].is_object());
        let vercel = judge_for("https://ai-gateway.vercel.sh/v1/decisions").request_body(&sample_input());
        let list = vercel["questions"].as_array().expect("vercel questions are an array");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["name"], "checkpoint_safe");
        assert_eq!(list[0]["type"], "score");
    }

    #[test]
    fn parse_answers_accepts_both_provider_shapes() {
        let j = judge_for("https://openrouter.ai/api/alpha/decisions");
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
        let j = judge_for("https://openrouter.ai/api/alpha/decisions");
        for bad in [
            // No answer at all (and refusals dropped from an array).
            serde_json::json!({}),
            serde_json::json!([]),
            serde_json::json!([{"name": "other", "score": 3.0}]),
            // Score of the wrong type or outside 0..=3.
            serde_json::json!({"checkpoint_safe": {"confidence": 0.9}}),
            serde_json::json!({"checkpoint_safe": {"score": "high", "confidence": 0.9}}),
            serde_json::json!({"checkpoint_safe": {"score": 5.0, "confidence": 0.9}}),
            serde_json::json!({"checkpoint_safe": {"score": -1.0, "confidence": 0.9}}),
            serde_json::json!({"checkpoint_safe": {"score": 1.4, "confidence": 2.0}}),
            // Confidence of the wrong type or outside 0..=1.
            serde_json::json!({"checkpoint_safe": {"score": 3.0, "confidence": 1.5}}),
            serde_json::json!({"checkpoint_safe": {"score": 3.0, "confidence": "sure"}}),
        ] {
            assert!(j.parse_answers(&bad).is_err(), "{bad}");
        }
    }
}
