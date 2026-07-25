//! Grading rubric and evaluation verdict for the `critique` evaluator.
//!
//! The rubric defines weighted, named criteria the critic scores 0-10. The
//! harness — not the model — computes the weighted total and pass/fail against
//! the threshold, so a critic that over-rates its own totals can't push weak
//! work over the bar. A project can override the criteria by dropping a
//! `.albatross/rubric.md` with `## Name (weight: N)` sections; otherwise
//! [`default_criteria`] is used. The pass threshold always comes from config.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Default pass threshold (0-10) when none is configured.
pub const DEFAULT_PASS_THRESHOLD: f32 = 7.0;

/// One weighted grading criterion.
#[derive(Debug, Clone, PartialEq)]
pub struct RubricCriterion {
    pub name: String,
    pub weight: f32,
    pub description: String,
}

/// A set of weighted criteria, the pass threshold (0-10), and optional freeform
/// guidance embedded verbatim into the critic prompt.
#[derive(Debug, Clone, PartialEq)]
pub struct Rubric {
    pub criteria: Vec<RubricCriterion>,
    pub pass_threshold: f32,
    pub guidance: Option<String>,
}

impl Rubric {
    pub fn total_weight(&self) -> f32 {
        self.criteria.iter().map(|c| c.weight).sum()
    }

    /// Weighted average of the per-criterion scores, normalized to 0-10. Scores
    /// are matched to criteria by name (case-insensitive); a criterion with no
    /// matching score counts as 0 (a missing grade is not a free pass).
    pub fn weighted_total(&self, scores: &[CriterionScore]) -> f32 {
        let total_weight = self.total_weight();
        if total_weight <= 0.0 {
            return 0.0;
        }
        let mut acc = 0.0;
        for c in &self.criteria {
            let s = scores
                .iter()
                .find(|s| s.name.eq_ignore_ascii_case(&c.name))
                .map(|s| s.score.clamp(0.0, 10.0))
                .unwrap_or(0.0);
            acc += s * c.weight;
        }
        acc / total_weight
    }
}

/// Built-in criteria, weighted to favor quality and functionality while
/// explicitly rewarding originality over generic output (the article's "AI
/// slop" penalty). Weights are relative; totals are normalized to 0-10.
pub fn default_criteria() -> Vec<RubricCriterion> {
    vec![
        RubricCriterion {
            name: "Quality".into(),
            weight: 3.0,
            description: "Coherence, correctness, and overall execution.".into(),
        },
        RubricCriterion {
            name: "Originality".into(),
            weight: 2.0,
            description: "Custom, deliberate decisions; avoids generic boilerplate / AI slop."
                .into(),
        },
        RubricCriterion {
            name: "Craft".into(),
            weight: 2.0,
            description: "Attention to detail: naming, structure, edge cases, polish.".into(),
        },
        RubricCriterion {
            name: "Functionality".into(),
            weight: 3.0,
            description: "Actually works and satisfies the stated goal.".into(),
        },
    ]
}

pub fn default_rubric(pass_threshold: f32) -> Rubric {
    Rubric {
        criteria: default_criteria(),
        pass_threshold,
        guidance: None,
    }
}

/// Load the rubric for a workspace. Reads `path` (or
/// `<workspace>/.albatross/rubric.md` when `path` is None); falls back to
/// [`default_criteria`] when the file is missing or has no parseable
/// `## Name (weight: N)` sections. The full file text, when present, is kept as
/// `guidance` and embedded into the critic prompt. `pass_threshold` always
/// comes from config, never the file.
pub fn load_rubric(workspace_root: &str, pass_threshold: f32, path: Option<&str>) -> Rubric {
    let rubric_path: PathBuf = match path {
        Some(p) => PathBuf::from(p),
        None => Path::new(workspace_root)
            .join(".albatross")
            .join("rubric.md"),
    };
    let Ok(raw) = std::fs::read_to_string(&rubric_path) else {
        return default_rubric(pass_threshold);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return default_rubric(pass_threshold);
    }
    let parsed = parse_rubric_markdown(trimmed);
    let criteria = if parsed.is_empty() {
        default_criteria()
    } else {
        parsed
    };
    Rubric {
        criteria,
        pass_threshold,
        guidance: Some(trimmed.to_string()),
    }
}

/// Parse `## Name (weight: N)` sections; lines under each heading become its
/// description. Headings without a `(weight: N)` marker are ignored.
fn parse_rubric_markdown(md: &str) -> Vec<RubricCriterion> {
    let mut out = Vec::new();
    let mut current: Option<(String, f32, String)> = None;
    for line in md.lines() {
        let trimmed = line.trim();
        if let Some((name, weight)) = parse_criterion_heading(trimmed) {
            if let Some((n, w, d)) = current.take() {
                out.push(RubricCriterion {
                    name: n,
                    weight: w,
                    description: d.trim().to_string(),
                });
            }
            current = Some((name, weight, String::new()));
        } else if let Some((_, _, desc)) = current.as_mut() {
            if !trimmed.is_empty() {
                if !desc.is_empty() {
                    desc.push(' ');
                }
                desc.push_str(trimmed);
            }
        }
    }
    if let Some((n, w, d)) = current.take() {
        out.push(RubricCriterion {
            name: n,
            weight: w,
            description: d.trim().to_string(),
        });
    }
    out
}

fn parse_criterion_heading(line: &str) -> Option<(String, f32)> {
    if !line.starts_with('#') {
        return None;
    }
    let after_hash = line.trim_start_matches('#').trim();
    // Match the ASCII marker case-insensitively on the original bytes:
    // to_lowercase() can change byte lengths for some Unicode chars, which
    // would make offsets from a lowercased copy invalid in `after_hash`.
    const MARKER: &[u8] = b"(weight:";
    let open = after_hash
        .as_bytes()
        .windows(MARKER.len())
        .rposition(|w| w.eq_ignore_ascii_case(MARKER))?;
    let name = after_hash[..open].trim();
    if name.is_empty() {
        return None;
    }
    let rest = &after_hash[open + MARKER.len()..];
    let close = rest.find(')')?;
    let weight: f32 = rest[..close].trim().parse().ok()?;
    if weight <= 0.0 {
        return None;
    }
    Some((name.to_string(), weight))
}

/// A single criterion's score as reported by the critic.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CriterionScore {
    pub name: String,
    #[serde(default)]
    pub score: f32,
    #[serde(default)]
    pub justification: String,
}

/// The parsed, harness-computed verdict. `weighted_total` and `pass` are always
/// computed from the rubric, never read from the model's output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalVerdict {
    pub scores: Vec<CriterionScore>,
    pub weighted_total: f32,
    pub pass: bool,
    #[serde(default)]
    pub feedback: Vec<String>,
    #[serde(default)]
    pub verdict: String,
    /// False when the critic's output couldn't be parsed (a deterministic
    /// hard-fail fallback) or the run was refused/errored.
    pub parsed: bool,
}

impl EvalVerdict {
    /// A deterministic hard-fail verdict for refusals and run errors.
    pub fn failed(reason: &str) -> Self {
        EvalVerdict {
            scores: Vec::new(),
            weighted_total: 0.0,
            pass: false,
            feedback: vec![reason.to_string()],
            verdict: reason.to_string(),
            parsed: false,
        }
    }
}

/// What the critic is asked to emit (no total/pass — the harness computes those).
#[derive(Deserialize)]
struct RawVerdict {
    #[serde(default)]
    scores: Vec<CriterionScore>,
    #[serde(default)]
    feedback: Vec<String>,
    #[serde(default)]
    verdict: String,
}

/// Parse the critic's raw text into a verdict, computing the weighted total and
/// pass/fail from the rubric. Falls back to a hard-fail verdict when no JSON
/// object can be extracted.
pub fn parse_verdict(raw: &str, rubric: &Rubric) -> EvalVerdict {
    let Some(json) = extract_json_object(raw) else {
        return unparseable_verdict(raw);
    };
    let Ok(parsed) = serde_json::from_str::<RawVerdict>(&json) else {
        return unparseable_verdict(raw);
    };
    let scores: Vec<CriterionScore> = parsed
        .scores
        .into_iter()
        .map(|s| CriterionScore {
            score: s.score.clamp(0.0, 10.0),
            name: s.name,
            justification: s.justification,
        })
        .collect();
    let weighted_total = rubric.weighted_total(&scores);
    EvalVerdict {
        pass: weighted_total >= rubric.pass_threshold,
        weighted_total,
        scores,
        feedback: parsed.feedback,
        verdict: parsed.verdict,
        parsed: true,
    }
}

fn unparseable_verdict(raw: &str) -> EvalVerdict {
    let snippet: String = raw.trim().chars().take(200).collect();
    EvalVerdict {
        scores: Vec::new(),
        weighted_total: 0.0,
        pass: false,
        feedback: vec![format!("Evaluator returned unparseable output: {snippet}")],
        verdict: "Could not parse evaluator output.".into(),
        parsed: false,
    }
}

/// Pull a single JSON object out of the critic's text, tolerating surrounding
/// prose and ```json fences.
fn extract_json_object(raw: &str) -> Option<String> {
    let mut trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("```json") {
        trimmed = rest.trim();
    } else if let Some(rest) = trimmed.strip_prefix("```") {
        trimmed = rest.trim();
    }
    let trimmed = trimmed.trim_end_matches("```").trim();
    if serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
        return Some(trimmed.to_string());
    }
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    if end <= start {
        return None;
    }
    let candidate = &trimmed[start..=end];
    serde_json::from_str::<serde_json::Value>(candidate)
        .ok()
        .map(|_| candidate.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_rubric_has_four_weighted_criteria() {
        let r = default_rubric(DEFAULT_PASS_THRESHOLD);
        assert_eq!(r.criteria.len(), 4);
        assert_eq!(r.total_weight(), 10.0);
        assert_eq!(r.pass_threshold, 7.0);
        assert!(r
            .criteria
            .iter()
            .any(|c| c.name.eq_ignore_ascii_case("originality")));
    }

    #[test]
    fn weighted_total_normalizes_and_treats_missing_as_zero() {
        let r = default_rubric(7.0);
        // Perfect across all criteria => 10.
        let all_ten: Vec<CriterionScore> = r
            .criteria
            .iter()
            .map(|c| CriterionScore {
                name: c.name.clone(),
                score: 10.0,
                justification: String::new(),
            })
            .collect();
        assert!((r.weighted_total(&all_ten) - 10.0).abs() < 1e-5);

        // Only Quality (weight 3 of 10) scored => 0.3 * 10 = 3.0.
        let partial = vec![CriterionScore {
            name: "Quality".into(),
            score: 10.0,
            justification: String::new(),
        }];
        assert!((r.weighted_total(&partial) - 3.0).abs() < 1e-5);
    }

    #[test]
    fn parse_rubric_markdown_reads_weighted_sections() {
        let md = "# My Rubric\n## Taste (weight: 4)\nIs it tasteful?\nmore detail\n## Correctness (weight: 6)\nDoes it work?";
        let criteria = parse_rubric_markdown(md);
        assert_eq!(criteria.len(), 2);
        assert_eq!(criteria[0].name, "Taste");
        assert_eq!(criteria[0].weight, 4.0);
        assert_eq!(criteria[0].description, "Is it tasteful? more detail");
        assert_eq!(criteria[1].name, "Correctness");
        assert_eq!(criteria[1].weight, 6.0);
    }

    #[test]
    fn load_rubric_falls_back_to_default_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let r = load_rubric(dir.path().to_str().unwrap(), 8.0, None);
        assert_eq!(r.criteria, default_criteria());
        assert_eq!(r.pass_threshold, 8.0);
        assert!(r.guidance.is_none());
    }

    #[test]
    fn load_rubric_reads_custom_file_and_keeps_guidance() {
        let dir = tempfile::tempdir().unwrap();
        let sh = dir.path().join(".albatross");
        std::fs::create_dir_all(&sh).unwrap();
        std::fs::write(
            sh.join("rubric.md"),
            "## Taste (weight: 5)\nTasteful?\n## Rigor (weight: 5)\nRigorous?",
        )
        .unwrap();
        let r = load_rubric(dir.path().to_str().unwrap(), 6.5, None);
        assert_eq!(r.criteria.len(), 2);
        assert_eq!(r.pass_threshold, 6.5);
        assert!(r.guidance.as_deref().unwrap().contains("Taste"));
    }

    #[test]
    fn parse_verdict_computes_total_and_pass_from_rubric() {
        let r = default_rubric(7.0);
        let raw = r#"{"scores":[
            {"name":"Quality","score":8},
            {"name":"Originality","score":7},
            {"name":"Craft","score":7},
            {"name":"Functionality","score":9}
        ],"feedback":["tighten naming"],"verdict":"solid"}"#;
        let v = parse_verdict(raw, &r);
        assert!(v.parsed);
        // (8*3 + 7*2 + 7*2 + 9*3) / 10 = (24+14+14+27)/10 = 7.9
        assert!((v.weighted_total - 7.9).abs() < 1e-4);
        assert!(v.pass);
        assert_eq!(v.feedback, vec!["tighten naming".to_string()]);
    }

    #[test]
    fn parse_verdict_ignores_model_reported_total_and_handles_fences() {
        let r = default_rubric(7.0);
        // Model claims a high total/pass in fenced JSON, but real scores are low.
        let raw = "```json\n{\"scores\":[{\"name\":\"Quality\",\"score\":2},{\"name\":\"Functionality\",\"score\":1}],\"weighted_total\":10,\"pass\":true,\"feedback\":[\"broken\"],\"verdict\":\"bad\"}\n```";
        let v = parse_verdict(raw, &r);
        assert!(v.parsed);
        assert!(v.weighted_total < 7.0);
        assert!(!v.pass);
    }

    #[test]
    fn parse_verdict_falls_back_when_unparseable() {
        let r = default_rubric(7.0);
        let v = parse_verdict("the work looks pretty good to me!", &r);
        assert!(!v.parsed);
        assert!(!v.pass);
        assert_eq!(v.weighted_total, 0.0);
        assert!(v.feedback[0].contains("unparseable"));
    }
}
