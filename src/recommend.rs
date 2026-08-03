use regex::Regex;
use std::collections::HashMap;

use crate::backends::BackendName;
use crate::capabilities::{record_score, CapabilityRecord};
use crate::config::{AgentConfig, ToolSelection};
use crate::hardware::{HardwareSpec, HardwareTier};

#[derive(Debug, Clone, PartialEq)]
pub struct ModelCandidate {
    pub backend: BackendName,
    pub base_url: String,
    pub model: String,
    pub installed: bool,
    pub is_default: bool,
    pub is_current: bool,
    pub capability: Option<CapabilityRecord>,
}

impl ModelCandidate {
    pub fn new(
        backend: BackendName,
        base_url: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            backend,
            base_url: base_url.into(),
            model: model.into(),
            installed: false,
            is_default: false,
            is_current: false,
            capability: None,
        }
    }

    fn merge(&mut self, other: Self) {
        self.installed |= other.installed;
        self.is_default |= other.is_default;
        self.is_current |= other.is_current;
        if other.capability.is_some() {
            self.capability = other.capability;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryFit {
    Good,
    Tight,
    TooLarge,
    Unknown,
}

impl MemoryFit {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryFit::Good => "good",
            MemoryFit::Tight => "tight",
            MemoryFit::TooLarge => "too-large",
            MemoryFit::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecommendationConfidence {
    High,
    Medium,
    Low,
}

impl RecommendationConfidence {
    pub fn as_str(&self) -> &'static str {
        match self {
            RecommendationConfidence::High => "high",
            RecommendationConfidence::Medium => "medium",
            RecommendationConfidence::Low => "low",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelMetadata {
    pub parameters_b: Option<f32>,
    pub quant_bits: Option<u8>,
    pub estimated_memory_gb: Option<f32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelRecommendation {
    pub backend: BackendName,
    pub base_url: String,
    pub model: String,
    pub score: i64,
    pub confidence: RecommendationConfidence,
    pub memory_fit: MemoryFit,
    pub metadata: ModelMetadata,
    pub installed: bool,
    pub tool_path: String,
    pub benchmark_label: Option<String>,
    pub rationale: Vec<String>,
}

pub fn apply_recommendation_to_config(config: &mut AgentConfig, rec: &ModelRecommendation) {
    config.backend = rec.backend;
    config.model_override = Some(rec.model.clone());
    config.tool_selection = ToolSelection::Auto;
}

pub fn recommend_models(
    spec: &HardwareSpec,
    candidates: Vec<ModelCandidate>,
    include_cloud: bool,
) -> Vec<ModelRecommendation> {
    let mut recommendations: Vec<ModelRecommendation> = merge_candidates(candidates)
        .into_values()
        .filter(|candidate| include_cloud || candidate.backend.is_local())
        .map(|candidate| score_candidate(spec, candidate))
        .collect();
    recommendations.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| confidence_rank(b.confidence).cmp(&confidence_rank(a.confidence)))
            .then_with(|| a.backend.as_str().cmp(b.backend.as_str()))
            .then_with(|| a.model.cmp(&b.model))
    });
    recommendations
}

pub fn parse_model_metadata(model: &str) -> ModelMetadata {
    let parameters_b = parse_parameters_b(model);
    let quant_bits = parse_quant_bits(model);
    let estimated_memory_gb = parameters_b.map(|params| estimate_memory_gb(params, quant_bits));
    ModelMetadata {
        parameters_b,
        quant_bits,
        estimated_memory_gb,
    }
}

pub fn memory_fit(spec: &HardwareSpec, metadata: &ModelMetadata) -> MemoryFit {
    let Some(memory_gb) = spec.memory_gb() else {
        return MemoryFit::Unknown;
    };
    let Some(estimated_gb) = metadata.estimated_memory_gb else {
        return MemoryFit::Unknown;
    };
    if estimated_gb > memory_gb * 0.80 {
        MemoryFit::TooLarge
    } else if estimated_gb > memory_gb * 0.60 {
        MemoryFit::Tight
    } else {
        MemoryFit::Good
    }
}

fn score_candidate(spec: &HardwareSpec, candidate: ModelCandidate) -> ModelRecommendation {
    let metadata = parse_model_metadata(&candidate.model);
    let fit = memory_fit(spec, &metadata);
    let mut score = 0i64;
    let mut rationale = Vec::new();

    match fit {
        MemoryFit::Good => {
            score += 300;
            rationale.push("fits available memory comfortably".into());
        }
        MemoryFit::Tight => {
            score += 100;
            rationale.push("fits, but memory headroom may be tight".into());
        }
        MemoryFit::TooLarge => {
            score -= 500;
            rationale.push("likely too large for available memory".into());
        }
        MemoryFit::Unknown => rationale.push("memory fit is unknown".into()),
    }

    if candidate.installed {
        score += 220;
        rationale.push("installed on a reachable provider".into());
    } else if candidate.is_default {
        score += 70;
        rationale.push("default model for your detected hardware".into());
    }
    if candidate.is_current {
        score += 25;
        rationale.push("currently selected model".into());
    }

    let tier_score = tier_model_score(spec.tier(), metadata.parameters_b);
    score += tier_score;
    if tier_score > 0 {
        rationale.push(format!(
            "good size match for {} hardware",
            spec.tier().as_str()
        ));
    } else if tier_score < 0 {
        rationale.push(format!(
            "size is not ideal for {} hardware",
            spec.tier().as_str()
        ));
    }

    let (tool_path, benchmark_label) = if let Some(record) = &candidate.capability {
        score += record_score(record) / 2;
        if record.tool_calls.ok {
            score += 120;
            rationale.push("native tool calls observed".into());
        } else if record.inline_tool_json.ok {
            score += 60;
            rationale.push("inline JSON tool fallback observed".into());
        } else {
            score -= 40;
            rationale.push("tool-call behavior is unproven".into());
        }
        if let Some(bench) = &record.benchmark {
            score += bench.estimated_tokens_per_sec.round().min(120.0) as i64;
            rationale.push(format!(
                "{:.1} estimated tokens/sec",
                bench.estimated_tokens_per_sec
            ));
        }
        (record.tool_path().into(), bench_label(record))
    } else {
        ("unknown".into(), None)
    };

    if !candidate.backend.is_local() {
        score -= 40;
        rationale.push("cloud provider".into());
    }

    let confidence = confidence_for(&candidate, fit);
    ModelRecommendation {
        backend: candidate.backend,
        base_url: candidate.base_url,
        model: candidate.model,
        score,
        confidence,
        memory_fit: fit,
        metadata,
        installed: candidate.installed,
        tool_path,
        benchmark_label,
        rationale,
    }
}

fn confidence_for(candidate: &ModelCandidate, fit: MemoryFit) -> RecommendationConfidence {
    let has_probe = candidate
        .capability
        .as_ref()
        .map(|record| record.streaming.ok || record.benchmark.is_some())
        .unwrap_or(false);
    if candidate.installed && has_probe && fit != MemoryFit::TooLarge {
        RecommendationConfidence::High
    } else if (candidate.installed || candidate.capability.is_some()) && fit != MemoryFit::TooLarge
    {
        RecommendationConfidence::Medium
    } else {
        RecommendationConfidence::Low
    }
}

fn confidence_rank(confidence: RecommendationConfidence) -> u8 {
    match confidence {
        RecommendationConfidence::High => 3,
        RecommendationConfidence::Medium => 2,
        RecommendationConfidence::Low => 1,
    }
}

fn tier_model_score(tier: HardwareTier, params: Option<f32>) -> i64 {
    let Some(params) = params else {
        return 0;
    };
    match tier {
        HardwareTier::Tiny => {
            if params <= 4.0 {
                120
            } else if params <= 8.0 {
                30
            } else {
                -250
            }
        }
        HardwareTier::Small => {
            if params <= 8.5 {
                150
            } else if params <= 14.5 {
                25
            } else {
                -250
            }
        }
        HardwareTier::Medium => {
            if (10.0..=18.0).contains(&params) {
                150
            } else if params <= 8.5 {
                90
            } else if params <= 32.5 {
                20
            } else {
                -120
            }
        }
        HardwareTier::Large => {
            if params >= 14.0 {
                140
            } else if params <= 8.5 {
                60
            } else {
                100
            }
        }
        HardwareTier::Unknown => 0,
    }
}

fn merge_candidates(candidates: Vec<ModelCandidate>) -> HashMap<String, ModelCandidate> {
    let mut out = HashMap::new();
    for candidate in candidates {
        let key = candidate_key(&candidate);
        out.entry(key)
            .and_modify(|existing: &mut ModelCandidate| existing.merge(candidate.clone()))
            .or_insert(candidate);
    }
    out
}

fn candidate_key(candidate: &ModelCandidate) -> String {
    format!(
        "{}\n{}\n{}",
        candidate.backend.as_str(),
        candidate.base_url,
        candidate.model
    )
}

fn parse_parameters_b(model: &str) -> Option<f32> {
    let re = Regex::new(r"(?i)(?:^|[^a-z0-9])(\d+(?:\.\d+)?)\s*b(?:[^a-z0-9]|$)").ok()?;
    re.captures(model)
        .and_then(|cap| cap.get(1))
        .and_then(|m| m.as_str().parse::<f32>().ok())
}

fn parse_quant_bits(model: &str) -> Option<u8> {
    let q_re = Regex::new(r"(?i)(?:^|[^a-z0-9])q([2-8])(?:[^a-z0-9]|$)").ok()?;
    if let Some(bits) = q_re
        .captures(model)
        .and_then(|cap| cap.get(1))
        .and_then(|m| m.as_str().parse::<u8>().ok())
    {
        return Some(bits);
    }
    let bit_re = Regex::new(r"(?i)([2-8])\s*bit").ok()?;
    bit_re
        .captures(model)
        .and_then(|cap| cap.get(1))
        .and_then(|m| m.as_str().parse::<u8>().ok())
}

fn estimate_memory_gb(params_b: f32, quant_bits: Option<u8>) -> f32 {
    let bytes_per_param = match quant_bits {
        Some(2) => 0.35,
        Some(3) => 0.45,
        Some(4) => 0.65,
        Some(5) => 0.80,
        Some(6) => 0.95,
        Some(8) => 1.20,
        _ => 0.80,
    };
    let overhead = if params_b >= 30.0 { 4.0 } else { 2.5 };
    params_b * bytes_per_param + overhead
}

fn bench_label(record: &CapabilityRecord) -> Option<String> {
    record.benchmark.as_ref().map(|bench| {
        format!(
            "{:.1} tok/s, {:.2}s first token",
            bench.estimated_tokens_per_sec,
            bench.first_token_ms.unwrap_or(0) as f64 / 1000.0
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::BackendDescriptor;
    use crate::capabilities::{BenchmarkStats, CapabilityStatus};
    use crate::hardware::HardwareSpec;

    fn gb(n: u64) -> u64 {
        n * 1024 * 1024 * 1024
    }

    fn spec(memory_gb: u64) -> HardwareSpec {
        HardwareSpec {
            os: "macos".into(),
            arch: "arm64".into(),
            chip_name: Some("Apple M4".into()),
            machine_name: Some("Mac mini".into()),
            memory_bytes: Some(gb(memory_gb)),
            physical_cpus: Some(10),
            logical_cpus: Some(10),
        }
    }

    fn candidate(model: &str) -> ModelCandidate {
        let mut candidate =
            ModelCandidate::new(BackendName::Ollama, "http://localhost:11434/v1", model);
        candidate.installed = true;
        candidate
    }

    fn capability(model: &str, native_tools: bool) -> CapabilityRecord {
        let backend = BackendDescriptor {
            name: BackendName::Ollama,
            base_url: "http://localhost:11434/v1".into(),
            api_key: "ollama".into(),
            is_local: true,
            openrouter: crate::backends::OpenRouterConfig::default(),
        };
        let mut record = CapabilityRecord::minimal(&backend, model);
        record.models = CapabilityStatus::ok("1 model");
        record.streaming = CapabilityStatus::ok("stream ok");
        record.tool_calls = if native_tools {
            CapabilityStatus::ok("native tool_calls")
        } else {
            CapabilityStatus::fail("no tool calls")
        };
        record.benchmark = Some(BenchmarkStats::new(Some(100), Some(800), 1_000, 80));
        record
    }

    #[test]
    fn parses_model_size_and_quantization() {
        let meta = parse_model_metadata("mlx-community/Qwen2.5-Coder-14B-Instruct-4bit");
        assert_eq!(meta.parameters_b, Some(14.0));
        assert_eq!(meta.quant_bits, Some(4));
        assert!(meta.estimated_memory_gb.unwrap() > 10.0);

        let meta = parse_model_metadata("qwen2.5-coder:7b-q5_k_m");
        assert_eq!(meta.parameters_b, Some(7.0));
        assert_eq!(meta.quant_bits, Some(5));
    }

    #[test]
    fn installed_7b_beats_oversized_32b_on_16gb() {
        let recs = recommend_models(
            &spec(16),
            vec![candidate("qwen2.5-coder:7b"), candidate("qwen-coder:32b")],
            false,
        );
        assert_eq!(
            recs.first().map(|r| r.model.as_str()),
            Some("qwen2.5-coder:7b")
        );
        assert_eq!(recs[1].memory_fit, MemoryFit::TooLarge);
    }

    #[test]
    fn cached_native_tool_model_beats_uncached_equivalent() {
        let mut cached = candidate("qwen2.5-coder:7b");
        cached.capability = Some(capability("qwen2.5-coder:7b", true));
        let uncached = candidate("other-coder:7b");

        let recs = recommend_models(&spec(16), vec![uncached, cached], false);
        assert_eq!(
            recs.first().map(|r| r.model.as_str()),
            Some("qwen2.5-coder:7b")
        );
        assert_eq!(recs.first().map(|r| r.tool_path.as_str()), Some("native"));
    }

    #[test]
    fn cloud_is_excluded_unless_requested() {
        let local = candidate("local-coder:7b");
        let mut cloud = ModelCandidate::new(
            BackendName::Openrouter,
            "https://openrouter.ai/api/v1",
            "qwen/qwen-2.5-coder-32b-instruct",
        );
        cloud.installed = true;

        let local_only = recommend_models(&spec(16), vec![local.clone(), cloud.clone()], false);
        assert!(local_only
            .iter()
            .all(|r| r.backend != BackendName::Openrouter));

        let with_cloud = recommend_models(&spec(16), vec![local, cloud], true);
        assert!(with_cloud
            .iter()
            .any(|r| r.backend == BackendName::Openrouter));
    }

    #[test]
    fn openai_is_filtered_with_other_cloud_backends() {
        let local = candidate("local-coder:7b");
        let mut openai = ModelCandidate::new(
            BackendName::OpenAi,
            "https://api.openai.com/v1",
            "gpt-4o-mini",
        );
        openai.installed = true;

        let local_only = recommend_models(&spec(16), vec![local.clone(), openai.clone()], false);
        assert!(local_only.iter().all(|r| r.backend.is_local()));

        let with_cloud = recommend_models(&spec(16), vec![local, openai], true);
        assert!(with_cloud.iter().any(|r| r.backend == BackendName::OpenAi));
    }

    #[test]
    fn apply_recommendation_updates_config_only() {
        let rec = ModelRecommendation {
            backend: BackendName::LmStudio,
            base_url: "http://localhost:1234/v1".into(),
            model: "qwen2.5-coder-14b-instruct".into(),
            score: 100,
            confidence: RecommendationConfidence::High,
            memory_fit: MemoryFit::Good,
            metadata: parse_model_metadata("qwen2.5-coder-14b-instruct"),
            installed: true,
            tool_path: "native".into(),
            benchmark_label: None,
            rationale: Vec::new(),
        };
        let mut config = AgentConfig::default();

        apply_recommendation_to_config(&mut config, &rec);

        assert_eq!(config.backend, BackendName::LmStudio);
        assert_eq!(
            config.model_override.as_deref(),
            Some("qwen2.5-coder-14b-instruct")
        );
        assert_eq!(config.tool_selection, ToolSelection::Auto);
    }
}
