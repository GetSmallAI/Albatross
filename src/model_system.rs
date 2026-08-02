use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::backends::BackendName;
use crate::catalog;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskComplexity {
    Low,
    Medium,
    High,
}

impl TaskComplexity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "low" => Some(Self::Low),
            "medium" | "med" => Some(Self::Medium),
            "high" => Some(Self::High),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReviewTier {
    Play,
    Production,
}

impl ReviewTier {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Play => "play",
            Self::Production => "production",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "play" | "mvp" | "prototype" => Some(Self::Play),
            "production" | "prod" => Some(Self::Production),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EffortLevel {
    None,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl EffortLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" | "off" | "disabled" => Some(Self::None),
            "minimal" | "min" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" | "med" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" | "x-high" | "extra-high" | "extra_high" | "extra high" => Some(Self::XHigh),
            "max" | "maximum" | "highest" => Some(Self::Max),
            _ => None,
        }
    }

    pub fn openrouter_reasoning_effort(&self) -> &'static str {
        match self {
            Self::Max => "xhigh",
            other => other.as_str(),
        }
    }

    pub fn openai_reasoning_effort(&self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Minimal => Some("minimal"),
            Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High | Self::XHigh | Self::Max => Some("high"),
        }
    }
}

impl Serialize for EffortLevel {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for EffortLevel {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).ok_or_else(|| {
            serde::de::Error::custom("expected effort level none|minimal|low|medium|high|xhigh|max")
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRef {
    pub backend: BackendName,
    pub model: String,
    #[serde(default)]
    pub effort: Option<EffortLevel>,
    #[serde(rename = "thinkingDepth", default)]
    pub thinking_depth: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

impl ModelRef {
    pub fn parse_spec(spec: &str) -> Option<Self> {
        let (backend, model) = spec.trim().split_once(':')?;
        let backend = BackendName::parse(backend.trim())?;
        let model = model.trim();
        if model.is_empty() {
            return None;
        }
        Some(Self {
            backend,
            model: model.to_string(),
            effort: None,
            thinking_depth: None,
            notes: None,
        })
    }

    pub fn label(&self) -> String {
        format!("{}:{}", self.backend.as_str(), self.model)
    }

    pub fn detail(&self) -> String {
        self.detail_with_effort(None)
    }

    pub fn detail_with_effort(&self, effort: Option<EffortLevel>) -> String {
        let mut bits = vec![self.label()];
        if let Some(effort) = effort.or(self.effort) {
            bits.push(format!("effort={}", effort.as_str()));
        }
        if let Some(depth) = self.thinking_depth.as_deref().filter(|s| !s.is_empty()) {
            bits.push(format!("thinking={depth}"));
        }
        bits.join(" · ")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelTierSet {
    #[serde(default)]
    pub low: Option<ModelRef>,
    #[serde(default)]
    pub medium: Option<ModelRef>,
    #[serde(default)]
    pub high: Option<ModelRef>,
}

impl ModelTierSet {
    pub fn get(&self, complexity: TaskComplexity) -> Option<&ModelRef> {
        match complexity {
            TaskComplexity::Low => self.low.as_ref(),
            TaskComplexity::Medium => self.medium.as_ref(),
            TaskComplexity::High => self.high.as_ref(),
        }
    }

    pub fn any_configured(&self) -> bool {
        self.low.is_some() || self.medium.is_some() || self.high.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ReviewModelSet {
    #[serde(default)]
    pub play: Option<ModelRef>,
    #[serde(default)]
    pub production: Option<ModelRef>,
}

impl ReviewModelSet {
    pub fn get(&self, tier: ReviewTier) -> Option<&ModelRef> {
        match tier {
            ReviewTier::Play => self.play.as_ref(),
            ReviewTier::Production => self.production.as_ref(),
        }
    }

    pub fn any_configured(&self) -> bool {
        self.play.is_some() || self.production.is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RoutingObjective {
    Quality,
    Cost,
    #[default]
    Balanced,
}

impl RoutingObjective {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Quality => "quality",
            Self::Cost => "cost",
            Self::Balanced => "balanced",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum UnknownCostPolicy {
    Allow,
    #[default]
    Warn,
    Deny,
}

impl UnknownCostPolicy {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Warn => "warn",
            Self::Deny => "deny",
        }
    }
}

fn default_estimated_output_tokens() -> u32 {
    2_000
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutingPolicy {
    #[serde(default)]
    pub objective: RoutingObjective,
    #[serde(default)]
    pub max_turn_usd: Option<f64>,
    #[serde(default)]
    pub unknown_cost: UnknownCostPolicy,
    #[serde(default)]
    pub local_only: bool,
    #[serde(default)]
    pub min_confidence: Option<u8>,
    #[serde(default)]
    pub require_effort_support: bool,
    #[serde(default = "default_estimated_output_tokens")]
    pub estimated_output_tokens: u32,
}

impl Default for RoutingPolicy {
    fn default() -> Self {
        Self {
            objective: RoutingObjective::Balanced,
            max_turn_usd: None,
            unknown_cost: UnknownCostPolicy::Warn,
            local_only: false,
            min_confidence: None,
            require_effort_support: false,
            estimated_output_tokens: default_estimated_output_tokens(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteCandidate {
    pub complexity: TaskComplexity,
    pub model: ModelRef,
    pub eligible: bool,
    pub estimated_input_tokens: u32,
    pub estimated_output_tokens: u32,
    #[serde(default)]
    pub estimated_cost_usd: Option<f64>,
    #[serde(default)]
    pub selector_score: Option<u8>,
    #[serde(default)]
    pub exclusions: Vec<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl RouteCandidate {
    pub fn detail(&self) -> String {
        let cost = self
            .estimated_cost_usd
            .map(catalog::format_usd)
            .unwrap_or_else(|| "$?".into());
        let state = if self.eligible {
            "eligible"
        } else {
            "excluded"
        };
        format!(
            "{} · {state} · est. {cost}",
            self.model.detail_with_effort(None)
        )
    }
}

pub fn backend_supports_effort(backend: BackendName) -> bool {
    matches!(
        backend,
        BackendName::Openrouter | BackendName::OpenAi | BackendName::Grok
    )
}

pub fn evaluate_coder_candidates(
    stack: &ModelTierSet,
    policy: &RoutingPolicy,
    estimated_input_tokens: u32,
) -> Vec<RouteCandidate> {
    [
        TaskComplexity::Low,
        TaskComplexity::Medium,
        TaskComplexity::High,
    ]
    .into_iter()
    .filter_map(|complexity| {
        let model = stack.get(complexity)?.clone();
        let estimated_cost_usd = if model.backend.is_local() {
            Some(0.0)
        } else {
            catalog::turn_cost_usd(
                model.backend,
                &model.model,
                estimated_input_tokens,
                policy.estimated_output_tokens,
            )
        };
        let mut exclusions = Vec::new();
        let mut warnings = Vec::new();
        if policy.local_only && !model.backend.is_local() {
            exclusions.push("policy requires a local backend".into());
        }
        if model.effort.is_some() && !backend_supports_effort(model.backend) {
            let message = format!("{} does not apply requested effort", model.backend.as_str());
            if policy.require_effort_support {
                exclusions.push(message);
            } else {
                warnings.push(message);
            }
        }
        match estimated_cost_usd {
            Some(cost) => {
                if let Some(cap) = policy.max_turn_usd.filter(|cap| cost > *cap) {
                    exclusions.push(format!(
                        "estimated cost {} exceeds {} cap",
                        catalog::format_usd(cost),
                        catalog::format_usd(cap)
                    ));
                }
            }
            None => match policy.unknown_cost {
                UnknownCostPolicy::Allow => {}
                UnknownCostPolicy::Warn => warnings.push("estimated cost is unknown".into()),
                UnknownCostPolicy::Deny => {
                    exclusions.push("policy denies models with unknown cost".into())
                }
            },
        }
        Some(RouteCandidate {
            complexity,
            model,
            eligible: exclusions.is_empty(),
            estimated_input_tokens,
            estimated_output_tokens: policy.estimated_output_tokens,
            estimated_cost_usd,
            selector_score: None,
            exclusions,
            warnings,
        })
    })
    .collect()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelSystemConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub planner: Option<ModelRef>,
    #[serde(default)]
    pub selector: Option<ModelRef>,
    /// Model used to summarize/compact the conversation transcript. When unset,
    /// compaction inherits the main conversation model.
    #[serde(default)]
    pub compaction: Option<ModelRef>,
    #[serde(default)]
    pub orchestrators: ModelTierSet,
    #[serde(default)]
    pub coders: ModelTierSet,
    #[serde(default)]
    pub reviewers: ReviewModelSet,
    #[serde(rename = "securityReviewer", default)]
    pub security_reviewer: Option<ModelRef>,
    #[serde(default)]
    pub policy: RoutingPolicy,
}

impl ModelSystemConfig {
    pub fn any_configured(&self) -> bool {
        self.planner.is_some()
            || self.selector.is_some()
            || self.compaction.is_some()
            || self.orchestrators.any_configured()
            || self.coders.any_configured()
            || self.reviewers.any_configured()
            || self.security_reviewer.is_some()
    }

    pub fn compaction(&self) -> Option<&ModelRef> {
        self.compaction.as_ref()
    }

    pub fn coder(&self, complexity: TaskComplexity) -> Option<&ModelRef> {
        self.coders.get(complexity)
    }

    pub fn orchestrator(&self, complexity: TaskComplexity) -> Option<&ModelRef> {
        self.orchestrators.get(complexity)
    }

    pub fn reviewer(&self, tier: ReviewTier) -> Option<&ModelRef> {
        self.reviewers.get(tier)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteDecision {
    pub complexity: TaskComplexity,
    #[serde(rename = "coderEffort", default)]
    pub coder_effort: Option<EffortLevel>,
    #[serde(default)]
    pub review: Option<ReviewTier>,
    #[serde(rename = "reviewEffort", default)]
    pub review_effort: Option<EffortLevel>,
    #[serde(default)]
    pub security_review: bool,
    #[serde(rename = "securityEffort", default)]
    pub security_effort: Option<EffortLevel>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub confidence: Option<u8>,
    #[serde(rename = "candidateScores", default)]
    pub candidate_scores: BTreeMap<String, u8>,
    #[serde(rename = "policyAdjustments", default)]
    pub policy_adjustments: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_ref_parses_backend_prefixed_specs() {
        let m = ModelRef::parse_spec("ollama:qwen2.5-coder:7b").unwrap();
        assert_eq!(m.backend, BackendName::Ollama);
        assert_eq!(m.model, "qwen2.5-coder:7b");
        assert!(ModelRef::parse_spec("openrouter:anthropic/claude-sonnet-4.5").is_some());
        assert!(ModelRef::parse_spec("bad:model").is_none());
    }

    #[test]
    fn effort_level_parses_common_aliases() {
        assert_eq!(EffortLevel::parse("low"), Some(EffortLevel::Low));
        assert_eq!(EffortLevel::parse("extra-high"), Some(EffortLevel::XHigh));
        assert_eq!(EffortLevel::parse("maximum"), Some(EffortLevel::Max));
        assert_eq!(EffortLevel::Max.openrouter_reasoning_effort(), "xhigh");
        assert_eq!(EffortLevel::Max.openai_reasoning_effort(), Some("high"));
    }

    #[test]
    fn model_system_config_detects_any_configured_model() {
        let empty = ModelSystemConfig::default();
        assert!(!empty.any_configured());

        let configured = ModelSystemConfig {
            enabled: true,
            coders: ModelTierSet {
                low: ModelRef::parse_spec("ollama:qwen2.5-coder:7b"),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(configured.any_configured());
        assert_eq!(
            configured
                .coder(TaskComplexity::Low)
                .map(|m| m.model.as_str()),
            Some("qwen2.5-coder:7b")
        );
    }

    #[test]
    fn compaction_model_is_configurable_and_detected() {
        let empty = ModelSystemConfig::default();
        assert!(empty.compaction().is_none());

        let configured = ModelSystemConfig {
            compaction: ModelRef::parse_spec("openrouter:anthropic/claude-3.5-haiku"),
            ..Default::default()
        };
        assert!(configured.any_configured());
        let compaction = configured.compaction().expect("compaction set");
        assert_eq!(compaction.backend, BackendName::Openrouter);
        assert_eq!(compaction.model, "anthropic/claude-3.5-haiku");
    }

    #[test]
    fn compaction_model_round_trips_through_json() {
        let json =
            r#"{"compaction":{"backend":"openrouter","model":"anthropic/claude-3.5-haiku"}}"#;
        let cfg: ModelSystemConfig = serde_json::from_str(json).expect("parse");
        assert_eq!(
            cfg.compaction().map(|m| m.model.as_str()),
            Some("anthropic/claude-3.5-haiku")
        );
    }

    #[test]
    fn routing_policy_round_trips_through_json() {
        let json = r#"{
            "policy": {
                "objective": "cost",
                "maxTurnUsd": 0.05,
                "unknownCost": "deny",
                "localOnly": true,
                "minConfidence": 80,
                "requireEffortSupport": true,
                "estimatedOutputTokens": 4096
            }
        }"#;
        let cfg: ModelSystemConfig = serde_json::from_str(json).expect("parse");
        assert_eq!(cfg.policy.objective, RoutingObjective::Cost);
        assert_eq!(cfg.policy.max_turn_usd, Some(0.05));
        assert_eq!(cfg.policy.unknown_cost, UnknownCostPolicy::Deny);
        assert!(cfg.policy.local_only);
        assert_eq!(cfg.policy.min_confidence, Some(80));
        assert!(cfg.policy.require_effort_support);
        assert_eq!(cfg.policy.estimated_output_tokens, 4096);
    }

    #[test]
    fn candidate_evaluation_explains_cost_and_eligibility() {
        let stack = ModelTierSet {
            low: ModelRef::parse_spec("openai:gpt-4o-mini"),
            medium: ModelRef::parse_spec("openai:gpt-4o"),
            high: ModelRef::parse_spec("openrouter:unpriced/model"),
        };
        let policy = RoutingPolicy {
            max_turn_usd: Some(0.005),
            unknown_cost: UnknownCostPolicy::Deny,
            estimated_output_tokens: 2_000,
            ..Default::default()
        };
        let candidates = evaluate_coder_candidates(&stack, &policy, 1_000);
        assert_eq!(candidates.len(), 3);
        assert!(candidates[0].eligible);
        assert!(candidates[0].estimated_cost_usd.is_some());
        assert!(!candidates[1].eligible);
        assert!(candidates[1]
            .exclusions
            .iter()
            .any(|reason| reason.contains("exceeds")));
        assert!(!candidates[2].eligible);
        assert!(candidates[2]
            .exclusions
            .iter()
            .any(|reason| reason.contains("unknown cost")));
    }

    #[test]
    fn candidate_evaluation_enforces_local_and_effort_policies() {
        let mut local = ModelRef::parse_spec("ollama:qwen2.5-coder:7b").unwrap();
        local.effort = Some(EffortLevel::High);
        let stack = ModelTierSet {
            low: Some(local),
            medium: ModelRef::parse_spec("openai:gpt-4o-mini"),
            high: None,
        };
        let policy = RoutingPolicy {
            local_only: true,
            require_effort_support: true,
            ..Default::default()
        };
        let candidates = evaluate_coder_candidates(&stack, &policy, 500);
        assert_eq!(candidates.len(), 2);
        assert!(!candidates[0].eligible);
        assert!(candidates[0]
            .exclusions
            .iter()
            .any(|reason| reason.contains("does not apply requested effort")));
        assert!(!candidates[1].eligible);
        assert!(candidates[1]
            .exclusions
            .iter()
            .any(|reason| reason.contains("local backend")));
    }
}
