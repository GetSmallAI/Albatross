use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::backends::BackendName;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
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
}
