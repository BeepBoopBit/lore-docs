use crate::{
    embedding::Embedder,
    index::Index,
    reranking::{RerankMode, Reranker},
    search::{BriefOptions, Diagnostics, Mode, Passage},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Deserialize)]
pub struct EvaluationCase {
    pub id: String,
    pub category: String,
    pub query: String,
    pub expected: Vec<Target>,
}
#[derive(Debug, Deserialize)]
pub struct Target {
    /// Any one of these files can satisfy this evidence requirement.
    pub paths: Vec<String>,
    /// A short verbatim phrase expected in a returned passage.
    pub contains: String,
    #[serde(default)]
    pub section: Option<String>,
    #[serde(default)]
    pub contains_any: Vec<String>,
}
#[derive(Debug, Serialize)]
pub struct CaseResult {
    pub id: String,
    pub category: String,
    pub returned_paths: Vec<String>,
    pub candidate_recall: Option<f64>,
    pub section_recall: Option<f64>,
    pub target_outcomes: Vec<String>,
    pub evidence_stages: Vec<EvidenceStage>,
    pub target_recall: Option<f64>,
    pub evidence_recall: Option<f64>,
    pub reciprocal_rank: Option<f64>,
    pub correctly_empty: Option<bool>,
    pub elapsed_ms: f64,
    pub estimated_tokens: usize,
}
#[derive(Debug, Serialize)]
pub struct EvidenceStage {
    pub matching_passage_ids: Vec<i64>,
    pub file_discovered: bool,
    pub section_discovered: bool,
    pub block_discovered: bool,
    pub selection_pool_reached: bool,
    pub rerank_shortlist_reached: Option<bool>,
    pub evidence_returned: bool,
    pub loss_stage: String,
    pub omission_reasons: Vec<String>,
}
#[derive(Debug, Serialize)]
pub struct Summary {
    pub cases: usize,
    pub mean_target_recall_at_5: Option<f64>,
    pub mean_candidate_recall: Option<f64>,
    pub mean_section_recall: Option<f64>,
    pub mean_evidence_recall: Option<f64>,
    pub mean_reciprocal_rank: Option<f64>,
    pub unanswered_abstention_rate: Option<f64>,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub mean_estimated_tokens: f64,
    pub evidence_loss_stages: BTreeMap<String, usize>,
}
#[derive(Debug, Serialize)]
pub struct ModeReport {
    pub mode: Mode,
    pub summary: Summary,
    pub categories: BTreeMap<String, Summary>,
    pub cases: Vec<CaseResult>,
}
#[derive(Debug, Serialize)]
pub struct EvaluationReport {
    pub root: String,
    pub index_revision: u64,
    pub model: String,
    pub max_tokens: usize,
    pub ranking: crate::search::Ranking,
    pub selection: crate::search::Selection,
    pub passage_scoring: crate::search::PassageScoring,
    pub qualifiers: crate::preferences::QualifierMode,
    pub prefer: Vec<crate::preferences::Qualifier>,
    pub alternatives: usize,
    pub recover_sections: bool,
    pub rerank: RerankMode,
    pub reranker_model: Option<String>,
    pub candidates_per_channel: usize,
    pub roles: Vec<crate::search::RoleRule>,
    pub graph: bool,
    pub scope: String,
    pub min_similarity: f32,
    pub notes: Vec<String>,
    pub modes: Vec<ModeReport>,
}

pub fn evaluate(
    index: &Index,
    embedder: &mut dyn Embedder,
    cases: &[EvaluationCase],
    max_tokens: usize,
    min_similarity: f32,
) -> Result<EvaluationReport> {
    evaluate_with_options(
        index,
        embedder,
        cases,
        &BriefOptions {
            max_tokens,
            min_similarity,
            ..Default::default()
        },
    )
}

pub fn evaluate_with_options(
    index: &Index,
    embedder: &mut dyn Embedder,
    cases: &[EvaluationCase],
    settings: &BriefOptions,
) -> Result<EvaluationReport> {
    evaluate_with_reranker(index, embedder, None, cases, settings)
}

pub fn evaluate_with_reranker(
    index: &Index,
    embedder: &mut dyn Embedder,
    mut reranker: Option<&mut dyn Reranker>,
    cases: &[EvaluationCase],
    settings: &BriefOptions,
) -> Result<EvaluationReport> {
    ensure!(!cases.is_empty(), "Evaluation dataset is empty");
    // Passage IDs are snapshot-local. Never match labels from one revision against
    // a later query after a concurrent writer has refreshed the index.
    let annotation_revision = index.revision()?;
    ensure!(
        settings.max_files == 5,
        "Evaluation uses fixed max-files=5; use brief/diagnose for other limits"
    );
    // Check annotations up front instead of silently scoring missing paths as retrieval failures.
    let mut matches = Vec::new();
    for case in cases {
        let mut case_matches = Vec::new();
        for target in &case.expected {
            ensure!(
                !target.paths.is_empty() && !target.contains.trim().is_empty(),
                "{} has an empty target",
                case.id
            );
            ensure!(
                target.contains_any.iter().all(|s| !s.trim().is_empty()),
                "{} has an empty alternative evidence phrase",
                case.id
            );
            ensure!(
                target.section.as_ref().is_none_or(|s| !s.trim().is_empty()),
                "{} has an empty section label",
                case.id
            );
            let mut found = false;
            let mut section_found = target.section.is_none();
            for path in &target.paths {
                crate::search::validate_scope(path)?;
                let hash: String =
                    index
                        .db
                        .query_row("SELECT hash FROM documents WHERE path=?", [path], |r| {
                            r.get(0)
                        })?;
                index.verify_source(path, &hash)?;
                let source = std::fs::read_to_string(index.root()?.join(path))?;
                if let Some(section) = &target.section {
                    section_found |= source.contains(section);
                }
                if std::iter::once(&target.contains)
                    .chain(&target.contains_any)
                    .any(|phrase| normalized(&source).contains(&normalized(phrase)))
                {
                    found = true;
                }
            }
            ensure!(
                section_found,
                "Section label for {} does not occur in its target files",
                case.id
            );
            ensure!(
                found,
                "Annotation {} does not occur in any target file: {}",
                case.id,
                target.contains
            );
            let mut ids = BTreeMap::new();
            for path in &target.paths {
                let mut stmt = index
                    .db
                    .prepare("SELECT id FROM passages WHERE path=? ORDER BY id")?;
                for id in stmt.query_map([path], |r| r.get::<_, i64>(0))? {
                    let p = index.passage(id?)?;
                    if matches_phrase(target, &p.passage) {
                        ids.insert(p.passage.id, p.passage.section_id);
                    }
                }
            }
            case_matches.push(ids);
        }
        matches.push(case_matches);
    }
    ensure!(
        index.revision()? == annotation_revision,
        "Index changed while resolving evaluation annotations; rerun against a stable snapshot"
    );
    let mut modes = Vec::new();
    for mode in [Mode::Keyword, Mode::Semantic, Mode::Hybrid] {
        let options = BriefOptions {
            mode,
            connections: false,
            ..settings.clone()
        };
        let mut results = Vec::new();
        for (case, matching_ids) in cases.iter().zip(&matches) {
            let diagnostic = index.diagnose_with_reranker(
                &case.query,
                Some(embedder),
                reranker.as_mut().map(|r| &mut **r as &mut dyn Reranker),
                &options,
            )?;
            let brief = &diagnostic.brief;
            ensure!(
                brief.index_revision == annotation_revision,
                "Index changed during evaluation; rerun against a stable snapshot"
            );
            let mut discovered = 0;
            let mut section_hits = 0;
            let mut section_targets = 0;
            let mut target_outcomes = Vec::new();
            let mut evidence_stages = Vec::new();
            let mut file_hits = 0;
            let mut evidence_hits = 0;
            let mut first_rank = None;
            for (target, ids) in case.expected.iter().zip(matching_ids) {
                let stage = evidence_stage(target, ids, &diagnostic);
                let present = diagnostic
                    .candidates
                    .iter()
                    .any(|c| target.paths.contains(&c.path));
                discovered += usize::from(present);
                let matching = brief
                    .files
                    .iter()
                    .filter(|f| target.paths.contains(&f.path))
                    .min_by_key(|f| f.rank);
                let mut outcome = if present {
                    "file_not_returned"
                } else {
                    "absent_from_candidate_pool"
                };
                if let Some(file) = matching {
                    file_hits += 1;
                    first_rank = Some(first_rank.unwrap_or(usize::MAX).min(file.rank));
                    if stage.evidence_returned {
                        evidence_hits += 1;
                        outcome = "evidence_returned";
                    } else {
                        outcome = "evidence_not_returned";
                    }
                    if let Some(section) = &target.section {
                        let found = brief
                            .files
                            .iter()
                            .filter(|f| target.paths.contains(&f.path))
                            .flat_map(|f| &f.passages)
                            .any(|p| {
                                p.heading
                                    .rsplit(" > ")
                                    .next()
                                    .is_some_and(|h| normalized(h) == normalized(section))
                            });
                        section_hits += usize::from(found);
                        if !found {
                            outcome = "section_not_returned";
                        }
                    }
                }
                section_targets += usize::from(target.section.is_some());
                target_outcomes.push(outcome.into());
                evidence_stages.push(stage);
            }
            let count = case.expected.len();
            results.push(CaseResult {
                id: case.id.clone(),
                category: case.category.clone(),
                candidate_recall: (count > 0).then(|| discovered as f64 / count as f64),
                section_recall: (section_targets > 0)
                    .then(|| section_hits as f64 / section_targets as f64),
                target_outcomes,
                evidence_stages,
                returned_paths: brief.files.iter().map(|f| f.path.clone()).collect(),
                target_recall: (count > 0).then(|| file_hits as f64 / count as f64),
                evidence_recall: (count > 0).then(|| evidence_hits as f64 / count as f64),
                reciprocal_rank: (count > 0)
                    .then(|| first_rank.map(|r| 1.0 / r as f64).unwrap_or(0.0)),
                correctly_empty: (count == 0)
                    .then_some(brief.files.is_empty() && brief.alternative_sections.is_empty()),
                elapsed_ms: brief.elapsed_ms,
                estimated_tokens: brief.estimated_tokens,
            });
        }
        let mut categories = BTreeMap::new();
        for result in &results {
            categories
                .entry(result.category.clone())
                .or_insert_with(|| {
                    summarize(
                        &results
                            .iter()
                            .filter(|r| r.category == result.category)
                            .collect::<Vec<_>>(),
                    )
                });
        }
        modes.push(ModeReport {
            mode,
            summary: summarize(&results.iter().collect::<Vec<_>>()),
            categories,
            cases: results,
        });
    }
    Ok(EvaluationReport {recover_sections:settings.recover_sections,rerank:settings.rerank,reranker_model:reranker.as_ref().map(|r| r.identity().into()),qualifiers:settings.qualifiers,prefer:settings.prefer.clone(),alternatives:settings.alternatives,root:index.root()?.display().to_string(),index_revision:index.revision()?,model:embedder.identity().into(),max_tokens:settings.max_tokens,min_similarity:settings.min_similarity,ranking:settings.ranking,selection:settings.selection,passage_scoring:settings.passage_scoring,candidates_per_channel:settings.candidates,roles:settings.roles.clone(),graph:settings.graph,scope:settings.scope.clone(),
        notes:vec!["Manually annotated development set; not an independent held-out benchmark.".into(),
            "Latencies include query inference, retrieval, packing, and source checks; exclude model loading and connections. First query in each mode may be cold.".into(),
            "Recall uses annotated alternative files; unannotated results are not automatically incorrect. Empty-result rate is not calibrated answerability.".into()],modes})
}

fn matches_phrase(target: &Target, passage: &Passage) -> bool {
    std::iter::once(&target.contains)
        .chain(&target.contains_any)
        .any(|phrase| {
            normalized(&passage.text).contains(&normalized(phrase))
                || passage
                    .context
                    .as_ref()
                    .is_some_and(|c| normalized(&c.text).contains(&normalized(phrase)))
        })
}

fn evidence_stage(
    target: &Target,
    ids: &BTreeMap<i64, i64>,
    diagnostic: &Diagnostics,
) -> EvidenceStage {
    let relevant: Vec<_> = diagnostic
        .candidates
        .iter()
        .filter(|c| target.paths.contains(&c.path))
        .collect();
    let file_discovered = !relevant.is_empty();
    let section_discovered = relevant
        .iter()
        .any(|c| ids.values().any(|s| *s == c.section_id));
    let block_discovered = relevant
        .iter()
        .flat_map(|c| &c.hits)
        .any(|h| h.channel != "adjacent_context" && ids.contains_key(&h.passage_id));
    let traces: Vec<_> = diagnostic
        .selection_trace
        .iter()
        .filter(|t| t.passage_ids.iter().any(|id| ids.contains_key(id)))
        .collect();
    let selection_pool_reached = !traces.is_empty()
        || (diagnostic.selection == crate::search::Selection::Ranked
            && relevant
                .iter()
                .flat_map(|c| &c.hits)
                .any(|h| ids.contains_key(&h.passage_id) && h.selection != "heading_context"));
    let rerank_shortlist_reached = diagnostic
        .reranking
        .as_ref()
        .filter(|r| !r.bypassed_for_exact)
        .map(|_| {
            traces
                .iter()
                .any(|t| t.decision != "rerank_shortlist_limit")
        });
    let evidence_returned = diagnostic
        .brief
        .files
        .iter()
        .filter(|f| target.paths.contains(&f.path))
        .flat_map(|f| &f.passages)
        .any(|p| matches_phrase(target, p));
    let mut omission_reasons: BTreeSet<_> = traces
        .iter()
        .filter(|t| t.decision != "selected")
        .map(|t| t.decision.clone())
        .collect();
    if traces.is_empty() {
        omission_reasons.extend(
            relevant
                .iter()
                .flat_map(|c| &c.hits)
                .filter(|h| ids.contains_key(&h.passage_id))
                .map(|h| h.selection.clone()),
        );
    }
    let loss_stage = if evidence_returned {
        "returned"
    } else if ids.is_empty() {
        "annotation_spans_blocks"
    } else if !file_discovered {
        "file_discovery"
    } else if !section_discovered {
        "section_discovery"
    } else if !block_discovered && !selection_pool_reached {
        "block_discovery"
    } else if !selection_pool_reached {
        "selection_pool"
    } else if rerank_shortlist_reached == Some(false) {
        "rerank_shortlist"
    } else {
        "packing"
    };
    EvidenceStage {
        matching_passage_ids: ids.keys().copied().collect(),
        file_discovered,
        section_discovered,
        block_discovered,
        selection_pool_reached,
        rerank_shortlist_reached,
        evidence_returned,
        loss_stage: loss_stage.into(),
        omission_reasons: omission_reasons.into_iter().collect(),
    }
}

fn normalized(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}
fn summarize(cases: &[&CaseResult]) -> Summary {
    let mean = |values: Vec<f64>| {
        (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
    };
    let mut times: Vec<_> = cases.iter().map(|r| r.elapsed_ms).collect();
    times.sort_by(f64::total_cmp);
    Summary {
        evidence_loss_stages: cases.iter().flat_map(|c| &c.evidence_stages).fold(
            BTreeMap::new(),
            |mut counts, s| {
                *counts.entry(s.loss_stage.clone()).or_default() += 1;
                counts
            },
        ),
        cases: cases.len(),
        mean_candidate_recall: mean(cases.iter().filter_map(|r| r.candidate_recall).collect()),
        mean_section_recall: mean(cases.iter().filter_map(|r| r.section_recall).collect()),
        mean_target_recall_at_5: mean(cases.iter().filter_map(|r| r.target_recall).collect()),
        mean_evidence_recall: mean(cases.iter().filter_map(|r| r.evidence_recall).collect()),
        mean_reciprocal_rank: mean(cases.iter().filter_map(|r| r.reciprocal_rank).collect()),
        unanswered_abstention_rate: mean(
            cases
                .iter()
                .filter_map(|r| r.correctly_empty.map(|v| u8::from(v) as f64))
                .collect(),
        ),
        p50_ms: times[(times.len() - 1) / 2],
        p95_ms: times[((times.len() as f64 * 0.95).ceil() as usize - 1).min(times.len() - 1)],
        mean_estimated_tokens: cases.iter().map(|r| r.estimated_tokens).sum::<usize>() as f64
            / cases.len() as f64,
    }
}
