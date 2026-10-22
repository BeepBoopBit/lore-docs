//! Assemble complete source blocks; preserve retrieval focus and bound the entire response.
use crate::{
    cosine,
    index::Index,
    retrieval::Candidate,
    search::{Brief, BriefOptions, FileMatch, estimate_tokens, in_scope, query_terms, text_terms},
};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};
type NeighborScores = BTreeMap<(String, Option<String>), Option<(i64, f64)>>;
impl Index {
    fn assemble_ranked(
        &self,
        brief: &mut Brief,
        candidates: &mut [Candidate],
        query: &str,
        vector: Option<&[f32]>,
        options: &BriefOptions,
    ) -> Result<()> {
        let budget = if options.connections {
            options.max_tokens * 4 / 5
        } else {
            options.max_tokens
        };
        let mut files: Vec<(String, Vec<usize>)> = Vec::new();
        for (i, c) in candidates.iter().enumerate() {
            if let Some((_, ids)) = files.iter_mut().find(|(p, _)| p == &c.path) {
                ids.push(i);
            } else {
                files.push((c.path.clone(), vec![i]));
            }
        }
        let mut used = BTreeSet::new();
        // First pass: a useful block from each ranked file. Second: complementary sections.
        for round in 0..3 {
            for (path, ids) in &files {
                let existing = brief.files.iter().position(|f| f.path == *path);
                if existing.is_none() && (round > 0 || brief.files.len() >= options.max_files) {
                    for &i in ids {
                        if candidates[i].selected_passages.is_empty()
                            && candidates[i].decision == "not_selected"
                        {
                            candidates[i].decision = "file_limit".into();
                        }
                    }
                    continue;
                }
                if existing.is_some_and(|i| brief.files[i].passages.len() >= 3) {
                    continue;
                }
                let mut inserted = false;
                for &i in ids {
                    let c = &mut candidates[i];
                    // Prefer another section once this one has supplied evidence.
                    if round == 1 && !c.selected_passages.is_empty() {
                        continue;
                    }
                    let mut scores: BTreeMap<i64, (f64, Vec<String>, Option<f32>)> =
                        BTreeMap::new();
                    for h in &c.hits {
                        let entry = scores.entry(h.passage_id).or_default();
                        let old = entry.0;
                        entry.0 = old.max(h.contribution) + 0.25 * old.min(h.contribution);
                        if !entry.1.contains(&h.channel) {
                            entry.1.push(h.channel.clone());
                        }
                        if let Some(sim) = h.similarity {
                            entry.2 = Some(entry.2.map_or(sim, |v| v.max(sim)));
                        }
                    }
                    let mut ranked = scores.into_iter().collect::<Vec<_>>();
                    ranked.sort_by(|a, b| b.1.0.total_cmp(&a.1.0).then(a.0.cmp(&b.0)));
                    // Heading hits discover the section; body blocks provide its evidence.
                    let has_body = ranked.iter().any(|(id, _)| {
                        self.passage(*id).is_ok_and(|p| p.passage.kind != "heading")
                    });
                    for (id, (_, channels, similarity)) in ranked {
                        if used.contains(&id) {
                            continue;
                        }
                        let mut p = self.passage(id)?;
                        if has_body && p.passage.kind == "heading" {
                            for h in c.hits.iter_mut().filter(|h| h.passage_id == id) {
                                h.selection = "heading_context".into();
                            }
                            continue;
                        }
                        p.passage.matched_by = channels;
                        p.passage.semantic_similarity = similarity;
                        if brief
                            .files
                            .iter()
                            .flat_map(|f| &f.passages)
                            .any(|b| b.text == p.passage.text)
                        {
                            continue;
                        }
                        let previous = brief.files.clone();
                        if let Some(idx) = brief.files.iter().position(|f| f.path == *path) {
                            brief.files[idx].passages.push(p.passage.clone());
                        } else {
                            brief.files.push(FileMatch {
                                rank: brief.files.len() + 1,
                                path: path.clone(),
                                title: p.title,
                                source_hash: p.hash,
                                score: c.file_score,
                                role: c.role.clone(),
                                passages: vec![p.passage.clone()],
                                connections: Vec::new(),
                            });
                        }
                        if estimate_tokens(&serde_json::to_string(&brief)?) + 32 > budget {
                            brief.files = previous;
                            c.decision = "budget".into();
                            for h in c.hits.iter_mut().filter(|h| h.passage_id == id) {
                                h.selection = "budget".into();
                            }
                            brief.truncated = true;
                            continue;
                        }
                        used.insert(id);
                        for h in c.hits.iter_mut().filter(|h| h.passage_id == id) {
                            h.selection = "selected".into();
                        }
                        c.selected_passages.push(id);
                        c.decision = "selected".into();
                        brief.truncated |= p.passage.excerpted;
                        inserted = true;
                        break;
                    }
                    if inserted {
                        break;
                    }
                }
            }
        }
        for c in candidates.iter_mut() {
            if c.selected_passages.is_empty() && c.decision == "not_selected" {
                c.decision = if brief.files.iter().any(|f| f.path == c.path) {
                    "lower_ranked_evidence"
                } else {
                    "file_limit"
                }
                .into();
            }
            if !c.selected_passages.is_empty() {
                c.decision = "selected".into();
            }
        }
        brief.truncated |= candidates.iter().any(|c| c.selected_passages.is_empty());
        if options.connections {
            let terms = query_terms(query);
            let mut pending = Vec::new();
            let mut neighbor_scores: NeighborScores = BTreeMap::new();
            for (i, f) in brief.files.iter().enumerate() {
                let ids = f.passages.iter().map(|p| p.id).collect::<Vec<_>>();
                for connection in self.connections(&f.path, Some(&ids), 24)? {
                    if !in_scope(&connection.path, &options.scope) {
                        continue;
                    }
                    let mut relevance = 0.0f64;
                    for e in &connection.evidence {
                        let words = text_terms(&e.text);
                        relevance = relevance.max(
                            terms.iter().filter(|t| words.contains(*t)).count() as f64
                                / terms.len().max(1) as f64,
                        );
                    }
                    // Compare target content to the query, rather than rewarding popular links.
                    let key = (connection.path.clone(), connection.anchor.clone());
                    let best = if let Some(best) = neighbor_scores.get(&key) {
                        *best
                    } else {
                        let mut stmt=self.db.prepare("SELECT p.id,p.text,e.vector FROM passages p JOIN sections s ON s.id=p.section_id JOIN embeddings e ON e.key=p.embedding_key WHERE p.path=? AND (? IS NULL OR s.anchor=?) AND p.view!='identity' ORDER BY p.start_byte LIMIT 64")?;
                        let mut best = None;
                        for row in stmt.query_map(
                            rusqlite::params![
                                connection.path,
                                connection.anchor,
                                connection.anchor
                            ],
                            |r| {
                                Ok((
                                    r.get::<_, i64>(0)?,
                                    r.get::<_, String>(1)?,
                                    r.get::<_, Vec<u8>>(2)?,
                                ))
                            },
                        )? {
                            let (id, text, blob) = row?;
                            let words = text_terms(&text);
                            let score = vector.map_or_else(
                                || {
                                    terms.iter().filter(|t| words.contains(*t)).count() as f64
                                        / terms.len().max(1) as f64
                                },
                                |v| cosine(v, &crate::vector_from_blob(&blob)) as f64,
                            );
                            if best.is_none_or(|(_, s)| score > s) {
                                best = Some((id, score));
                            }
                        }
                        neighbor_scores.insert(key, best);
                        best
                    };
                    if let Some((id, score)) = best {
                        relevance = relevance.max(score);
                        if connection.kind == "references"
                            && !connection
                                .evidence
                                .iter()
                                .any(|e| e.path == connection.path)
                        {
                            let mut connection = connection;
                            connection.evidence.push(self.evidence(id, None)?);
                            pending.push((relevance, i, connection));
                            continue;
                        }
                    }
                    pending.push((relevance, i, connection));
                }
            }
            pending.sort_by(|a, b| {
                b.0.total_cmp(&a.0)
                    .then(a.2.path.cmp(&b.2.path))
                    .then(a.1.cmp(&b.1))
            });
            let mut seen = BTreeSet::new();
            let mut served = BTreeSet::new();
            let mut added = 0;
            while !pending.is_empty() && added < 6 {
                let next = pending
                    .iter()
                    .position(|(_, i, _)| !served.contains(i))
                    .unwrap_or(0);
                let (_, i, connection) = pending.remove(next);
                if !seen.insert((
                    brief.files[i].path.clone(),
                    connection.path.clone(),
                    connection.kind.clone(),
                )) {
                    continue;
                }
                brief.files[i].connections.push(connection);
                if estimate_tokens(&serde_json::to_string(&brief)?) + 32 > options.max_tokens {
                    brief.files[i].connections.pop();
                    brief.truncated = true;
                    continue;
                }
                added += 1;
                served.insert(i);
            }
            brief.truncated |= !pending.is_empty();
        }
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn assemble(
        &self,
        brief: &mut Brief,
        candidates: &mut [Candidate],
        query: &str,
        vector: Option<&[f32]>,
        options: &BriefOptions,
        reranking: &mut crate::reranking::RerankSession<'_>,
    ) -> Result<Vec<crate::selection::SelectionDecision>> {
        let trace = match options.selection {
            crate::search::Selection::Ranked => {
                self.assemble_ranked(brief, candidates, query, vector, options)?;
                Vec::new()
            }
            crate::search::Selection::Evidence => {
                self.select_evidence(brief, candidates, query, vector, options, reranking)?
            }
        };
        for f in &brief.files {
            self.verify_source(&f.path, &f.source_hash)?;
            for connection in &f.connections {
                self.verify_connection(connection)?;
            }
        }
        // Graph seeds influence selected results even when their context did not fit the brief.
        for c in candidates
            .iter()
            .filter(|c| !c.selected_passages.is_empty())
        {
            for support in &c.graph_support {
                let hash: String = self.db.query_row(
                    "SELECT hash FROM documents WHERE path=?",
                    [&support.source_path],
                    |r| r.get(0),
                )?;
                self.verify_source(&support.source_path, &hash)?;
            }
        }
        Ok(trace)
    }
}
