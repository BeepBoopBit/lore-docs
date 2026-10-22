//! Bounded search within already discovered files, using cached representations.
use crate::{
    cosine,
    index::Index,
    retrieval::{Candidate, Hit},
    search::{BriefOptions, query_terms, text_terms},
    vector_from_blob,
};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};

impl Index {
    pub(crate) fn recover_sections(
        &self,
        candidates: &mut Vec<Candidate>,
        query: &str,
        vector: Option<&[f32]>,
        options: &BriefOptions,
    ) -> Result<()> {
        if !options.recover_sections || candidates.iter().any(|c| c.exact) {
            return Ok(());
        }
        let mut ordered: Vec<_> = candidates.iter().collect();
        ordered.sort_by(|a, b| {
            b.file_score
                .total_cmp(&a.file_score)
                .then(b.score.total_cmp(&a.score))
                .then(a.path.cmp(&b.path))
                .then(a.section_id.cmp(&b.section_id))
        });
        let mut paths = BTreeSet::new();
        let seeds: Vec<_> = ordered
            .into_iter()
            .filter(|c| paths.insert(c.path.clone()))
            .take(5)
            .cloned()
            .collect();
        let terms = query_terms(query);
        for seed in seeds {
            let mut stmt = self.db.prepare("SELECT p.id,p.section_id,p.view,p.text,e.vector,p.start_line,p.end_line FROM passages p JOIN embeddings e ON e.key=p.embedding_key WHERE p.path=? AND p.view!='identity' ORDER BY p.start_byte,p.id LIMIT 512")?;
            let mut ranked = Vec::new();
            for row in stmt.query_map([&seed.path], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Vec<u8>>(4)?,
                    [r.get::<_, u32>(5)? as usize, r.get::<_, u32>(6)? as usize],
                ))
            })? {
                let (id, section, view, text, blob, lines) = row?;
                let sim = vector.map(|v| cosine(v, &vector_from_blob(&blob)));
                let words = text_terms(&text);
                let overlap = terms.iter().filter(|t| words.contains(*t)).count() as f64
                    / terms.len().max(1) as f64;
                let score = sim.map_or(overlap, f64::from);
                if sim.is_some_and(|v| v >= options.min_similarity)
                    || (sim.is_none() && overlap > 0.0)
                {
                    ranked.push((score, id, section, view, lines, sim));
                }
            }
            ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
            let maximum = ranked.first().map_or(1.0, |r| r.0).max(f64::EPSILON);
            let existing: BTreeSet<_> = candidates
                .iter()
                .flat_map(|c| c.hits.iter().map(|h| h.passage_id))
                .collect();
            let mut sections = BTreeMap::<i64, usize>::new();
            let mut added = 0;
            for (score, id, section, view, lines, sim) in ranked {
                if existing.contains(&id)
                    || sections.get(&section).copied().unwrap_or(0) >= 2
                    || (!sections.contains_key(&section) && sections.len() >= 2)
                {
                    continue;
                }
                let ci = if let Some(i) = candidates.iter().position(|c| c.section_id == section) {
                    i
                } else {
                    let mut c = self.section_candidate(section)?;
                    c.score = seed.score;
                    c.file_score = seed.file_score;
                    c.role = seed.role.clone();
                    c.graph_support = seed.graph_support.clone();
                    candidates.push(c);
                    candidates.len() - 1
                };
                for support in &seed.graph_support {
                    if !candidates[ci].graph_support.iter().any(|s| {
                        s.source_section == support.source_section
                            && s.source_line == support.source_line
                    }) {
                        candidates[ci].graph_support.push(support.clone());
                    }
                }
                candidates[ci].hits.push(Hit {
                    passage_id: id,
                    view,
                    channel: "file_recovery".into(),
                    rank: 0,
                    similarity: sim,
                    lexical_score: None,
                    lines,
                    contribution: seed.score * (score / maximum).clamp(0.0, 1.0),
                    selection: "lower_ranked_evidence".into(),
                });
                *sections.entry(section).or_default() += 1;
                added += 1;
                if added == 4 {
                    break;
                }
            }
        }
        Ok(())
    }
}
