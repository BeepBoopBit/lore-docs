//! Evidence and bounded reading guidance from a single retrieval.
use crate::{
    api_context::Activation,
    embedding::Embedder,
    guidance::{BroaderRead, CompactDiagnostics, Gaps, Probe},
    index::Index,
    reading::{TOKEN_METHOD, cost},
    recommendation::ReadRecommendation,
    reranking::Reranker,
    search::{Brief, BriefOptions},
};
use anyhow::{Result, ensure};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct GuidedBrief {
    pub brief: Brief,
    pub guidance: BriefGuidance,
    /// Counts the complete wrapper, evidence, and guidance together.
    pub estimated_tokens: usize,
    pub token_count_method: &'static str,
}

#[derive(Debug, Serialize)]
pub struct BriefGuidance {
    pub scope: String,
    pub answer_completeness: &'static str,
    pub api_context: Activation,
    pub next_reads: Vec<GuidedRead>,
    pub omitted_suggestions: usize,
    pub consolidated_suggestions: usize,
    pub probe: Probe,
    pub limitations: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<ReadRecommendation>,
}

#[derive(Debug, Serialize)]
pub struct GuidedRead {
    pub read_target: String,
    pub context: bool,
    /// Counts overlap and describe the focused target, not every consolidated target.
    pub gaps: Gaps,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub broader_read: Option<BroaderRead>,
}

impl BriefGuidance {
    fn from_compact(value: &CompactDiagnostics<'_>) -> Self {
        Self {
            scope: value.scope.into(),
            answer_completeness: value.answer_completeness,
            api_context: value.api_context.clone(),
            next_reads: value
                .next_reads
                .iter()
                .map(|r| GuidedRead {
                    read_target: r.read_target.clone(),
                    context: r.context,
                    gaps: r.gaps.clone(),
                    broader_read: r.broader_read.clone(),
                })
                .collect(),
            omitted_suggestions: value.omitted_suggestions,
            consolidated_suggestions: value.consolidated_suggestions,
            probe: value.probe.clone(),
            limitations: "Bounded structural gaps, not required answer facts. Empty hints do not establish completeness; matches do not prove capability. No cross-file dependency check. Follow read continuations.",
            recommendation: None,
        }
    }
}

impl Index {
    pub fn guided_brief(
        &self,
        query: &str,
        embedder: Option<&mut dyn Embedder>,
        options: &BriefOptions,
    ) -> Result<GuidedBrief> {
        self.guided_brief_with_reranker(query, embedder, None, options)
    }

    /// Reserve bounded guidance space before evidence selection. API bundles
    /// remain atomic; guidance never removes passages from a finished brief.
    pub fn guided_brief_with_reranker(
        &self,
        query: &str,
        embedder: Option<&mut dyn Embedder>,
        reranker: Option<&mut dyn Reranker>,
        options: &BriefOptions,
    ) -> Result<GuidedBrief> {
        ensure!(
            (128..=100_000).contains(&options.max_tokens),
            "max-tokens must be between 128 and 100000"
        );
        let guidance_budget = (options.max_tokens / 4).clamp(256, 600);
        // Null placeholders conservatively account for braces, separators and
        // the largest token-count field, independently of root/query lengths.
        let overhead = cost(&serde_json::json!({
            "brief": null, "guidance": null,
            "estimated_tokens": options.max_tokens,
            "token_count_method": TOKEN_METHOD,
        }))? + 2;
        ensure!(
            options.max_tokens >= guidance_budget + overhead + 128,
            "max-tokens too small for a guided brief; increase --max-tokens"
        );
        let mut evidence_options = options.clone();
        evidence_options.max_tokens = options.max_tokens - guidance_budget - overhead;
        let diagnostic =
            self.diagnose_with_reranker(query, embedder, reranker, &evidence_options)?;
        let compact = self.pack_guidance(&diagnostic, guidance_budget, 3, |c| {
            cost(&BriefGuidance::from_compact(c))
        })?;
        let mut guidance = BriefGuidance::from_compact(&compact);
        if let Some(first) = guidance.next_reads.first() {
            guidance.recommendation = Some(self.recommend_read(&diagnostic, first)?);
        }
        let mut result = GuidedBrief {
            brief: diagnostic.brief,
            guidance,
            estimated_tokens: options.max_tokens,
            token_count_method: TOKEN_METHOD,
        };
        // Annotate only after packing. Recommendations spend remaining response
        // space; they cannot displace evidence, focused hints or parent options.
        if cost(&result)? > options.max_tokens
            && let Some(choice) = &mut result.guidance.recommendation
        {
            choice.focus_terms.clear();
            choice.added_terms.clear();
        }
        if cost(&result)? > options.max_tokens {
            result.guidance.recommendation = None;
        }
        loop {
            let measured = cost(&result)?;
            if result.estimated_tokens == measured {
                break;
            }
            result.estimated_tokens = measured;
        }
        ensure!(
            cost(&result)? <= options.max_tokens,
            "Internal guided brief budget exceeded"
        );
        Ok(result)
    }
}
