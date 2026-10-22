//! Compact pointers to retrieved sections whose evidence was omitted.
use crate::{
    index::Index,
    preferences::Qualifier,
    retrieval::Candidate,
    search::{Brief, BriefOptions},
    selection::SelectionDecision,
};
use anyhow::Result;
use serde::Serialize;
use std::collections::BTreeSet;

#[derive(Debug, Clone, Serialize)]
pub struct Alternative {
    /// Exact input to `lore read`, not a generated search query.
    pub target: String,
    pub title: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub qualifiers: Vec<Qualifier>,
    #[serde(skip)]
    pub(crate) section_id: i64,
    #[serde(skip)]
    pub(crate) path: String,
}

fn bytes(alternatives: &[Alternative]) -> Result<usize> {
    Ok(if alternatives.is_empty() {
        0
    } else {
        serde_json::to_vec(alternatives)?.len() + ",\"alternative_sections\":".len()
    })
}

impl Index {
    fn navigation_leads(
        &self,
        brief: &Brief,
        candidates: &[Candidate],
        count: usize,
        budget: usize,
    ) -> Result<Vec<Alternative>> {
        let selected_paths: BTreeSet<_> = brief.files.iter().map(|f| &f.path).collect();
        let mut ordered: Vec<_> = candidates
            .iter()
            .filter(|c| c.selected_passages.is_empty())
            .collect();
        // Preserve retrieval order within each group; expose undisplayed files first.
        ordered.sort_by_key(|c| selected_paths.contains(&c.path));
        let mut seen = BTreeSet::new();
        let mut result = Vec::new();
        for c in ordered {
            if seen.contains(&c.path) {
                continue;
            }
            let title: String = self.db.query_row(
                "SELECT title FROM sections WHERE id=?",
                [c.section_id],
                |r| r.get(0),
            )?;
            let alternative = Alternative {
                target: if c.anchor.is_empty() {
                    c.path.clone()
                } else {
                    format!("{}#{}", c.path, c.anchor)
                },
                title: title.chars().take(120).collect(),
                qualifiers: c.qualifier_matches.iter().map(|m| m.qualifier).collect(),
                section_id: c.section_id,
                path: c.path.clone(),
            };
            result.push(alternative);
            if bytes(&result)? > budget {
                result.pop();
                continue;
            }
            seen.insert(c.path.clone());
            if result.len() == count {
                break;
            }
        }
        Ok(result)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn add_alternatives(
        &self,
        brief: &mut Brief,
        candidates: &mut Vec<Candidate>,
        original_candidates: Vec<Candidate>,
        query: &str,
        vector: Option<&[f32]>,
        options: &BriefOptions,
        trace: Vec<SelectionDecision>,
        reranking: &mut crate::reranking::RerankSession<'_>,
    ) -> Result<Vec<SelectionDecision>> {
        if candidates.iter().any(|c| c.exact) {
            return Ok(trace);
        }
        let mut envelope = brief.clone();
        envelope.files.clear();
        let available =
            (options.max_tokens * 3).saturating_sub(serde_json::to_vec(&envelope)?.len() + 96);
        let cap = (options.max_tokens * 3 * 15 / 100).min(available);
        let planned = self.navigation_leads(brief, candidates, options.alternatives, cap)?;
        if planned.is_empty() {
            return Ok(trace);
        }
        let reserved = bytes(&planned)?;
        let previous = brief.clone();
        let previous_candidates = std::mem::replace(candidates, original_candidates);
        brief.files.clear();
        brief.truncated = false;
        brief.alternative_sections = planned;
        let next_trace = self.assemble(brief, candidates, query, vector, options, reranking)?;
        // Rebuild against the actual final selections, without exceeding the reservation.
        brief.alternative_sections =
            self.navigation_leads(brief, candidates, options.alternatives, reserved)?;
        if brief.alternative_sections.is_empty() {
            *brief = previous;
            *candidates = previous_candidates;
            return Ok(trace);
        }
        for a in &brief.alternative_sections {
            let hash: String =
                self.db
                    .query_row("SELECT hash FROM documents WHERE path=?", [&a.path], |r| {
                        r.get(0)
                    })?;
            self.verify_source(&a.path, &hash)?;
            // Graph support is evidence even when only a navigation reference is returned.
            if let Some(c) = candidates.iter().find(|c| c.section_id == a.section_id) {
                for support in &c.graph_support {
                    let hash: String = self.db.query_row(
                        "SELECT hash FROM documents WHERE path=?",
                        [&support.source_path],
                        |r| r.get(0),
                    )?;
                    self.verify_source(&support.source_path, &hash)?;
                }
            }
        }
        brief.truncated = true;
        Ok(next_trace)
    }
}
