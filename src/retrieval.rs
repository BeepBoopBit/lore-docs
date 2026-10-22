//! Retrieve section candidates while retaining the exact source block behind every hit.
use crate::{
    cosine,
    embedding::Embedder,
    index::Index,
    search::{BriefOptions, Mode, Ranking, in_scope, query_terms},
    vector_from_blob,
};
use anyhow::{Context, Result, ensure};
use rusqlite::params;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Serialize)]
pub struct Hit {
    pub passage_id: i64,
    pub view: String,
    pub channel: String,
    pub rank: usize,
    pub similarity: Option<f32>,
    pub lexical_score: Option<f64>,
    pub lines: [usize; 2],
    pub contribution: f64,
    pub selection: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct Support {
    pub kind: String,
    pub source_section: i64,
    pub source_path: String,
    pub source_line: usize,
}
#[derive(Clone, Debug, Serialize)]
pub struct Candidate {
    pub section_id: i64,
    pub path: String,
    pub heading: String,
    pub anchor: String,
    pub keyword_rank: Option<usize>,
    pub semantic_rank: Option<usize>,
    pub score: f64,
    pub file_score: f64,
    pub exact: bool,
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub qualifier_matches: Vec<crate::preferences::QualifierMatch>,
    #[serde(skip_serializing_if = "crate::preferences::unboosted")]
    pub qualifier_boost: f64,
    pub hits: Vec<Hit>,
    pub graph_support: Vec<Support>,
    pub selected_passages: Vec<i64>,
    pub decision: String,
}
type FileRanks = (Option<usize>, Option<usize>, Vec<f64>);
struct Row {
    id: i64,
    section: i64,
    view: String,
    value: f64,
    lines: [usize; 2],
}
fn order(rows: &mut [Row]) {
    rows.sort_by(|a, b| {
        b.value
            .total_cmp(&a.value)
            .then(a.section.cmp(&b.section))
            .then(a.id.cmp(&b.id))
    });
}
fn rrf(rank: usize) -> f64 {
    1.0 / (60.0 + rank as f64)
}
fn combine(a: Option<usize>, b: Option<usize>) -> f64 {
    let a = a.map(rrf).unwrap_or(0.0);
    let b = b.map(rrf).unwrap_or(0.0);
    a.max(b) + 0.25 * a.min(b)
}
impl Index {
    pub(crate) fn retrieve(
        &self,
        query: &str,
        embedder: Option<&mut dyn Embedder>,
        options: &BriefOptions,
    ) -> Result<(Vec<Candidate>, Option<Vec<f32>>)> {
        let mut candidates: BTreeMap<i64, Candidate> = BTreeMap::new();
        let terms = query_terms(query);
        if options.mode != Mode::Semantic && !terms.is_empty() {
            let expr = terms
                .iter()
                .map(|t| format!("\"{t}\""))
                .collect::<Vec<_>>()
                .join(" OR ");
            let mut stmt=self.db.prepare("SELECT p.id,p.section_id,p.view,-bm25(passages_fts,0.5,2.0,1.0),p.start_line,p.end_line FROM passages_fts JOIN passages p ON p.id=passages_fts.rowid WHERE passages_fts MATCH ? AND (?='' OR p.path=? OR substr(p.path,1,length(?)+1)=?||'/') ORDER BY bm25(passages_fts,0.5,2.0,1.0),p.id")?;
            let rows = stmt
                .query_map(
                    params![
                        expr,
                        options.scope,
                        options.scope,
                        options.scope,
                        options.scope
                    ],
                    |r| {
                        Ok(Row {
                            id: r.get(0)?,
                            section: r.get(1)?,
                            view: r.get(2)?,
                            value: r.get(3)?,
                            lines: [r.get::<_, u32>(4)? as usize, r.get::<_, u32>(5)? as usize],
                        })
                    },
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            self.merge_channel(&mut candidates, rows, "keyword", options.candidates)?;
        }
        let vector = if options.mode != Mode::Keyword {
            let model = embedder.context("Semantic and hybrid search require the local model")?;
            ensure!(
                self.meta("model")?.as_deref() == Some(model.identity()),
                "Embedding model differs from index; reindex required"
            );
            let vector = model.embed_query(query)?;
            ensure!(
                vector.len() == 384 && vector.iter().all(|v| v.is_finite()),
                "Invalid query vector"
            );
            // Exact scan applies scope BEFORE per-section/per-view ranking. A global KNN cutoff
            // would starve a narrow scope and let repeated blocks consume its candidate budget.
            let mut stmt=self.db.prepare("SELECT p.id,p.section_id,p.view,e.vector,p.start_line,p.end_line FROM passages p JOIN embeddings e ON e.key=p.embedding_key WHERE (?='' OR p.path=? OR substr(p.path,1,length(?)+1)=?||'/')")?;
            let mut rows = Vec::new();
            for row in stmt.query_map(
                params![options.scope, options.scope, options.scope, options.scope],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Vec<u8>>(3)?,
                        [r.get::<_, u32>(4)? as usize, r.get::<_, u32>(5)? as usize],
                    ))
                },
            )? {
                let (id, section, view, blob, lines) = row?;
                let sim = cosine(&vector, &vector_from_blob(&blob));
                if sim >= options.min_similarity {
                    rows.push(Row {
                        id,
                        section,
                        view,
                        value: sim as f64,
                        lines,
                    });
                }
            }
            // Each view gets its own section shortlist, then a best-view merge. No repeated-block voting.
            for view in ["identity", "prose", "example"] {
                let mut subset = rows
                    .iter()
                    .filter(|r| r.view == view)
                    .map(|r| Row {
                        id: r.id,
                        section: r.section,
                        view: r.view.clone(),
                        value: r.value,
                        lines: r.lines,
                    })
                    .collect::<Vec<_>>();
                order(&mut subset);
                self.merge_channel(&mut candidates, subset, "semantic", options.candidates)?;
            }
            Some(vector)
        } else {
            None
        };
        // View ranks are independent retrieval lists, not interchangeable global semantic ranks.
        // Merge them once, then rank sections. Identity-only evidence receives less weight
        // than a source block explaining the behavior; literal identity has its own lookup.
        let mut dense_order = Vec::new();
        for c in candidates.values() {
            let mut views: BTreeMap<&str, f64> = BTreeMap::new();
            for h in c.hits.iter().filter(|h| h.channel == "semantic") {
                let weight = if h.view == "identity" { 0.5 } else { 1.0 };
                let value = weight * rrf(h.rank);
                let slot = views.entry(&h.view).or_insert(0.0);
                *slot = slot.max(value);
            }
            let mut values = views.into_values().collect::<Vec<_>>();
            values.sort_by(|a, b| b.total_cmp(a));
            let score = values
                .iter()
                .zip([1.0, 0.25, 0.125])
                .map(|(s, w)| s * w)
                .sum::<f64>();
            if score > 0.0 {
                dense_order.push((c.section_id, score));
            }
        }
        dense_order.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        for (i, (id, _)) in dense_order.into_iter().enumerate() {
            candidates.get_mut(&id).unwrap().semantic_rank = Some(i + 1);
        }
        for c in candidates.values_mut() {
            let lexical_max = c
                .hits
                .iter()
                .filter_map(|h| h.lexical_score)
                .fold(0.0f64, f64::max);
            let semantic_max = c
                .hits
                .iter()
                .filter_map(|h| h.similarity)
                .fold(0.0f32, f32::max);
            for h in &mut c.hits {
                if let Some(value) = h.lexical_score {
                    h.contribution =
                        rrf(c.keyword_rank.unwrap()) * value / lexical_max.max(f64::EPSILON);
                } else if let Some(similarity) = h.similarity {
                    h.contribution = rrf(c.semantic_rank.unwrap())
                        * (similarity / semantic_max.max(f32::EPSILON)) as f64;
                }
            }
        }
        // A literal path/anchor lookup is independent of FTS/model vocabulary and candidate budgets.
        let (literal, anchor) = query
            .trim()
            .split_once('#')
            .map_or((query.trim(), None), |(p, a)| (p, Some(a)));
        if in_scope(literal, &options.scope) {
            let mut stmt=self.db.prepare("SELECT s.id FROM sections s WHERE s.path=? AND (? IS NULL OR s.anchor=?) AND EXISTS(SELECT 1 FROM passages p WHERE p.section_id=s.id) ORDER BY s.start_byte,s.id LIMIT 1")?;
            for id in stmt.query_map(params![literal, anchor, anchor], |r| r.get::<_, i64>(0))? {
                let id = id?;
                if let std::collections::btree_map::Entry::Vacant(e) = candidates.entry(id) {
                    e.insert(self.section_candidate(id)?);
                }
                let c = candidates.get_mut(&id).unwrap();
                c.exact = true;
                self.add_section_focus(c, query, vector.as_deref(), "exact")?;
            }
        }
        let mut candidates: Vec<_> = candidates.into_values().collect();
        for c in &mut candidates {
            c.score = combine(c.keyword_rank, c.semantic_rank);
            if c.hits.iter().all(|h| h.view == "identity") {
                self.add_section_focus(c, query, vector.as_deref(), "section_context")?;
            }
        }
        candidates.sort_by(|a, b| {
            b.exact
                .cmp(&a.exact)
                .then(b.score.total_cmp(&a.score))
                .then(a.path.cmp(&b.path))
                .then(a.section_id.cmp(&b.section_id))
        });
        if options.graph {
            self.expand(&mut candidates, query, vector.as_deref(), options)?;
        }
        let mut files: BTreeMap<String, FileRanks> = BTreeMap::new();
        for c in &candidates {
            let entry = files.entry(c.path.clone()).or_default();
            for (current, new) in [
                (&mut entry.0, c.keyword_rank),
                (&mut entry.1, c.semantic_rank),
            ] {
                if let Some(new) = new {
                    *current = Some(current.map_or(new, |old| old.min(new)));
                }
            }
            entry.2.push(c.score);
        }
        for c in &mut candidates {
            let f = files.get_mut(&c.path).unwrap();
            f.2.sort_by(|a, b| b.total_cmp(a));
            let score = match options.ranking {
                Ranking::Document => {
                    combine(f.0, f.1).max(f.2[0]) + 0.1 * f.2.get(1).copied().unwrap_or(0.0)
                }
                Ranking::Section => f.2[0],
            };
            let role = options
                .roles
                .iter()
                .filter(|r| in_scope(&c.path, &r.prefix))
                .max_by_key(|r| r.prefix.len());
            c.role = role.map(|r| r.role.clone());
            c.file_score = if c.exact {
                1.0
            } else {
                score * role.map_or(1.0, |r| r.weight)
            };
        }
        self.recover_sections(&mut candidates, query, vector.as_deref(), options)?;
        if !candidates.iter().any(|c| c.exact) {
            let requested =
                crate::preferences::requested(query, options.qualifiers, &options.prefer);
            self.apply_preferences(&mut candidates, &requested)?;
        }
        candidates.sort_by(|a, b| {
            b.exact
                .cmp(&a.exact)
                .then(
                    (b.file_score * b.qualifier_boost)
                        .total_cmp(&(a.file_score * a.qualifier_boost)),
                )
                .then(b.score.total_cmp(&a.score))
                .then(a.path.cmp(&b.path))
                .then(a.section_id.cmp(&b.section_id))
        });
        Ok((candidates, vector))
    }
    pub(crate) fn section_candidate(&self, id: i64) -> Result<Candidate> {
        Ok(self.db.query_row(
            "SELECT path,heading,anchor FROM sections WHERE id=?",
            [id],
            |r| {
                Ok(Candidate {
                    section_id: id,
                    path: r.get(0)?,
                    heading: r.get(1)?,
                    anchor: r.get(2)?,
                    keyword_rank: None,
                    semantic_rank: None,
                    score: 0.0,
                    file_score: 0.0,
                    exact: false,
                    role: None,
                    qualifier_matches: Vec::new(),
                    qualifier_boost: 1.0,
                    hits: Vec::new(),
                    graph_support: Vec::new(),
                    selected_passages: Vec::new(),
                    decision: "not_selected".into(),
                })
            },
        )?)
    }
    fn merge_channel(
        &self,
        candidates: &mut BTreeMap<i64, Candidate>,
        rows: Vec<Row>,
        channel: &str,
        limit: usize,
    ) -> Result<()> {
        let mut ranks = BTreeMap::new();
        let mut counts = BTreeMap::new();
        for row in rows {
            let next = ranks.len() + 1;
            let rank = *ranks.entry(row.section).or_insert(next);
            if rank > limit {
                continue;
            }
            let count = counts
                .entry((row.section, row.view.clone()))
                .or_insert(0usize);
            if *count >= 3 {
                continue;
            }
            *count += 1;
            if let std::collections::btree_map::Entry::Vacant(e) = candidates.entry(row.section) {
                e.insert(self.section_candidate(row.section)?);
            }
            let c = candidates.get_mut(&row.section).unwrap();
            let r = if channel == "keyword" {
                &mut c.keyword_rank
            } else {
                &mut c.semantic_rank
            };
            *r = Some(r.map_or(rank, |old| old.min(rank)));
            c.hits.push(Hit {
                passage_id: row.id,
                view: row.view,
                channel: channel.into(),
                rank,
                similarity: (channel == "semantic").then_some(row.value as f32),
                lexical_score: (channel == "keyword").then_some(row.value),
                lines: row.lines,
                selection: "lower_ranked_evidence".into(),
                contribution: rrf(rank) * 0.95f64.powi((*count - 1) as i32),
            });
        }
        Ok(())
    }
    fn add_section_focus(
        &self,
        c: &mut Candidate,
        query: &str,
        vector: Option<&[f32]>,
        channel: &str,
    ) -> Result<()> {
        let terms = query_terms(query);
        let mut stmt=self.db.prepare("SELECT p.id,p.view,p.text,e.vector,p.start_line,p.end_line FROM passages p JOIN embeddings e ON e.key=p.embedding_key WHERE p.section_id=? AND p.view!='identity' ORDER BY p.start_byte")?;
        let mut rows = Vec::new();
        for row in stmt.query_map([c.section_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Vec<u8>>(3)?,
                [r.get::<_, u32>(4)? as usize, r.get::<_, u32>(5)? as usize],
            ))
        })? {
            let (id, view, text, blob, lines) = row?;
            let sim = vector.map(|v| cosine(v, &vector_from_blob(&blob)));
            let words = crate::search::text_terms(&text);
            let overlap = terms.iter().filter(|t| words.contains(*t)).count() as f64
                / terms.len().max(1) as f64;
            rows.push((sim.map_or(overlap, |s| s as f64), id, view, lines, sim));
        }
        rows.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        for (_, id, view, lines, similarity) in rows.into_iter().take(2) {
            c.hits.push(Hit {
                passage_id: id,
                view,
                channel: channel.into(),
                rank: 1,
                similarity,
                lexical_score: None,
                lines,
                contribution: rrf(1),
                selection: "lower_ranked_evidence".into(),
            });
        }
        Ok(())
    }
    fn expand(
        &self,
        candidates: &mut Vec<Candidate>,
        query: &str,
        vector: Option<&[f32]>,
        options: &BriefOptions,
    ) -> Result<()> {
        let seeds = candidates.iter().take(16).cloned().collect::<Vec<_>>();
        let mut expanded = BTreeSet::new();
        for seed in seeds {
            let mut stmt=self.db.prepare("SELECT DISTINCT l.target,l.destination,l.line FROM links l JOIN passages p ON p.path=l.source AND p.start_line<=l.line AND p.end_line>=l.line WHERE p.section_id=? ORDER BY l.target,l.line LIMIT 8")?;
            let links = stmt
                .query_map([seed.section_id], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, u32>(2)? as usize,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (path, destination, line) in links {
                if !in_scope(&path, &options.scope) {
                    continue;
                }
                let anchor = destination.split_once('#').and_then(|(_, s)| {
                    percent_encoding::percent_decode_str(s)
                        .decode_utf8()
                        .ok()
                        .map(|s| s.into_owned())
                });
                let mut stmt=self.db.prepare("SELECT s.id FROM sections s WHERE s.path=? AND (? IS NULL OR s.anchor=?) AND EXISTS(SELECT 1 FROM passages p WHERE p.section_id=s.id) ORDER BY s.start_byte,s.id LIMIT 16")?;
                let ids = stmt
                    .query_map(params![path, anchor, anchor], |r| r.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let mut neighbors = Vec::new();
                for id in ids {
                    if id == seed.section_id {
                        continue;
                    }
                    let mut c = self.section_candidate(id)?;
                    self.add_section_focus(&mut c, query, vector, "graph")?;
                    let relevance = c
                        .hits
                        .iter()
                        .filter_map(|h| h.similarity)
                        .fold(0.0f32, f32::max);
                    let lexical = c.hits.iter().any(|h| {
                        self.passage(h.passage_id).is_ok_and(|p| {
                            query_terms(query)
                                .iter()
                                .any(|t| text_contains(&p.passage.text, t))
                        })
                    });
                    if (vector.is_some() && relevance < options.min_similarity)
                        || (vector.is_none() && !lexical)
                    {
                        continue;
                    }
                    c.score = rrf(1)
                        * if vector.is_some() {
                            relevance as f64
                        } else {
                            0.5
                        };
                    neighbors.push(c);
                }
                neighbors.sort_by(|a, b| {
                    b.score
                        .total_cmp(&a.score)
                        .then(a.section_id.cmp(&b.section_id))
                });
                if let Some(mut c) = neighbors.into_iter().next() {
                    if !expanded.insert((seed.section_id, c.section_id)) {
                        continue;
                    }
                    let support = Support {
                        kind: "references".into(),
                        source_section: seed.section_id,
                        source_path: seed.path.clone(),
                        source_line: line,
                    };
                    if let Some(existing) =
                        candidates.iter_mut().find(|x| x.section_id == c.section_id)
                    {
                        if existing.graph_support.is_empty() {
                            existing.score += 0.15 * seed.score;
                        }
                        existing.graph_support.push(support);
                    } else {
                        c.score += 0.15 * seed.score;
                        c.graph_support.push(support);
                        candidates.push(c);
                    }
                }
            }
        }
        Ok(())
    }
}
fn text_contains(text: &str, term: &str) -> bool {
    crate::search::text_terms(text).contains(term)
}
