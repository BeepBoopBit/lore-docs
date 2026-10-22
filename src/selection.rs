//! Bounded greedy selection of source evidence under the complete JSON budget.
//! Scores are heuristic utilities, not probabilities or a knapsack optimum.
pub use crate::content::PassageScore;
use crate::{
    cosine,
    index::Index,
    reranking::{RerankMode, RerankSession},
    retrieval::{Candidate, Hit},
    search::{
        Brief, BriefOptions, Connection, FileMatch, Mode, PassageScoring, StoredPassage,
        estimate_tokens, in_scope, query_terms, text_terms,
    },
};
use anyhow::Result;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashSet};

const MAX_SECTIONS: usize = 128;
const MAX_BLOCKS: usize = 512;
const MAX_CONTEXT_SEEDS: usize = 32;
const MAX_STEPS: usize = 32;

#[derive(Debug, Serialize)]
pub struct SelectionDecision {
    pub kind: String,
    pub path: String,
    pub passage_ids: Vec<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection_path: Option<String>,
    pub quality: f64,
    pub marginal_gain: f64,
    pub incremental_tokens: usize,
    pub decision: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<usize>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub passage_scores: Vec<PassageScore>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub api_dependencies: Vec<crate::api_context::Dependency>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub original_passage_ids: Vec<i64>,
}

struct Block {
    stored: StoredPassage,
    candidate: usize,
    quality: f64,
    words: HashSet<String>,
    context_only: bool,
}
enum Payload {
    Blocks(Vec<usize>),
    Connection(usize, Box<Connection>, Vec<f32>, HashSet<String>),
}
struct Choice {
    payload: Payload,
    trace: SelectionDecision,
    exact: bool,
}

fn relevance(similarity: f32, cutoff: f32) -> f64 {
    ((similarity - cutoff) / (1.0 - cutoff).max(0.01)).clamp(0.0, 1.0) as f64
}
fn overlap(words: &HashSet<String>, terms: &[String]) -> f64 {
    terms.iter().filter(|t| words.contains(*t)).count() as f64 / terms.len().max(1) as f64
}
fn duplicate(a: &HashSet<String>, b: &HashSet<String>, av: &[f32], bv: &[f32]) -> f64 {
    let common = a.intersection(b).count();
    let jaccard = common as f64 / (a.len() + b.len() - common).max(1) as f64;
    // Related paragraphs are expected. Penalize only very close vectors, plus token repetition.
    let semantic = ((cosine(av, bv) as f64 - 0.85) / 0.15).clamp(0.0, 1.0);
    jaccard.max(semantic)
}
fn adjacent(a: &StoredPassage, b: &StoredPassage) -> bool {
    a.passage.section_id == b.passage.section_id
        && a.passage.bytes[1] <= b.passage.bytes[0]
        && b.passage.lines[0].saturating_sub(a.passage.lines[1]) <= 3
        && a.passage.kind != "heading"
        && b.passage.kind != "heading"
        && a.passage.kind != "table_row"
        && b.passage.kind != "table_row"
}
fn choice(blocks: &[Block], ids: Vec<usize>, candidates: &[Candidate]) -> Choice {
    let first = &blocks[ids[0]];
    Choice {
        exact: ids.iter().any(|&i| candidates[blocks[i].candidate].exact),
        trace: SelectionDecision {
            kind: if ids.len() > 1 { "bundle" } else { "block" }.into(),
            path: first.stored.path.clone(),
            passage_ids: ids.iter().map(|&i| blocks[i].stored.passage.id).collect(),
            connection_path: None,
            quality: ids.iter().map(|&i| blocks[i].quality).sum(),
            marginal_gain: 0.0,
            incremental_tokens: 0,
            decision: "lower_utility".into(),
            step: None,
            passage_scores: Vec::new(),
            api_dependencies: Vec::new(),
            original_passage_ids: Vec::new(),
        },
        payload: Payload::Blocks(ids),
    }
}

impl Index {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn select_evidence(
        &self,
        brief: &mut Brief,
        candidates: &mut [Candidate],
        query: &str,
        vector: Option<&[f32]>,
        options: &BriefOptions,
        reranking: &mut RerankSession<'_>,
    ) -> Result<Vec<SelectionDecision>> {
        let terms = query_terms(query);
        let lexical_max = candidates
            .iter()
            .flat_map(|c| &c.hits)
            .filter_map(|h| h.lexical_score)
            .fold(0.0f64, f64::max)
            .max(f64::EPSILON);
        let section_max = candidates
            .iter()
            .map(|c| c.score)
            .fold(0.0f64, f64::max)
            .max(f64::EPSILON);
        let mut blocks = Vec::new();
        for (ci, c) in candidates.iter().enumerate().take(MAX_SECTIONS) {
            let ids: BTreeSet<_> = c.hits.iter().map(|h| h.passage_id).collect();
            let has_body = c.hits.iter().any(|h| h.view != "identity");
            let role = if c.exact {
                1.0
            } else {
                let strongest = candidates
                    .iter()
                    .filter(|other| other.path == c.path)
                    .map(|other| other.score)
                    .fold(0.0f64, f64::max);
                c.file_score / strongest.max(f64::EPSILON)
            };
            for id in ids {
                if blocks.len() >= MAX_BLOCKS {
                    break;
                }
                let mut p = self.passage(id)?;
                if has_body && p.passage.kind == "heading" {
                    continue;
                }
                let hits: Vec<_> = c.hits.iter().filter(|h| h.passage_id == id).collect();
                let words = text_terms(&p.passage.text);
                let lex = hits
                    .iter()
                    .filter_map(|h| h.lexical_score)
                    .fold(0.0f64, f64::max)
                    / lexical_max;
                let sim = vector.map(|v| cosine(v, &p.vector));
                let dense = sim.map_or(0.0, |s| relevance(s, options.min_similarity));
                let rank = (c.score / section_max).min(1.0);
                let raw = match options.mode {
                    Mode::Keyword => 0.85 * lex.max(overlap(&words, &terms) * 0.5) + 0.15 * rank,
                    Mode::Semantic => 0.85 * dense + 0.15 * rank,
                    Mode::Hybrid => 0.6 * dense + 0.25 * lex + 0.15 * rank,
                };
                let mut contributions: Vec<_> = hits.iter().map(|h| h.contribution).collect();
                contributions.sort_by(|a, b| b.total_cmp(a));
                let fused = contributions.first().copied().unwrap_or(0.0)
                    + 0.25 * contributions.get(1).copied().unwrap_or(0.0);
                let quality = (0.7 * (fused * 61.0 / 1.25).min(1.0) + 0.3 * raw) * role;
                p.passage.matched_by = hits
                    .iter()
                    .map(|h| h.channel.clone())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect();
                p.passage.semantic_similarity = sim;
                blocks.push(Block {
                    stored: p,
                    candidate: ci,
                    quality: if c.exact {
                        quality.max(1.0)
                    } else {
                        quality * c.qualifier_boost
                    },
                    words,
                    context_only: false,
                });
            }
        }
        // Only immediate explanation/example neighbors within the same section can enter
        // without a retrieval hit. Such context cannot stand alone or trigger further expansion.
        let mut seeds: Vec<_> = (0..blocks.len()).collect();
        seeds.sort_by(|&a, &b| {
            blocks[b].quality.total_cmp(&blocks[a].quality).then(
                blocks[a]
                    .stored
                    .passage
                    .id
                    .cmp(&blocks[b].stored.passage.id),
            )
        });
        let mut pairs = BTreeSet::new();
        let mut context_parents = BTreeMap::new();
        for i in seeds.into_iter().take(MAX_CONTEXT_SEEDS) {
            let p = &blocks[i].stored.passage;
            let mut stmt = self.db.prepare("SELECT id FROM passages WHERE section_id=? AND (start_byte=(SELECT max(start_byte) FROM passages WHERE section_id=? AND start_byte<?) OR start_byte=(SELECT min(start_byte) FROM passages WHERE section_id=? AND start_byte>?)) ORDER BY start_byte")?;
            let ids = stmt
                .query_map(
                    rusqlite::params![
                        p.section_id,
                        p.section_id,
                        p.bytes[0] as i64,
                        p.section_id,
                        p.bytes[0] as i64
                    ],
                    |r| r.get::<_, i64>(0),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for id in ids {
                let other = self.passage(id)?;
                if !(adjacent(&blocks[i].stored, &other) || adjacent(&other, &blocks[i].stored)) {
                    continue;
                }
                let j = if let Some(j) = blocks.iter().position(|b| b.stored.passage.id == id) {
                    j
                } else {
                    if blocks.len() >= MAX_BLOCKS + MAX_CONTEXT_SEEDS * 2 {
                        continue;
                    }
                    let a = blocks[i].stored.passage.kind.as_str();
                    let b = other.passage.kind.as_str();
                    if !((a == "prose" && b == "code") || (a == "code" && b == "prose")) {
                        continue;
                    }
                    let words = text_terms(&other.passage.text);
                    let mut stored = other;
                    stored.passage.matched_by = vec!["adjacent_context".into()];
                    stored.passage.semantic_similarity = vector.map(|v| cosine(v, &stored.vector));
                    blocks.push(Block {
                        stored,
                        candidate: blocks[i].candidate,
                        quality: blocks[i].quality * 0.4,
                        words,
                        context_only: true,
                    });
                    context_parents.insert(blocks.len() - 1, i);
                    blocks.len() - 1
                };
                // Direct-hit neighbors must themselves be relevant; don't bundle weak side topics.
                if blocks[j].context_only
                    || blocks[j].quality.min(blocks[i].quality)
                        >= 0.6 * blocks[j].quality.max(blocks[i].quality)
                {
                    pairs.insert((i.min(j), i.max(j)));
                }
            }
        }
        // Freeze the entire block/bundle pool using the original scores before this ablation.
        // Only block quality changes; discovery, redundancy vectors and packing stay identical.
        let mut scores = BTreeMap::new();
        if options.passage_scoring != PassageScoring::Contextual {
            let ids: Vec<_> = blocks.iter().map(|b| b.stored.passage.id).collect();
            let signals = self.content_scores(&ids, query, vector, options)?;
            for i in 0..blocks.len() {
                let b = &blocks[i];
                let c = &candidates[b.candidate];
                let strongest = candidates
                    .iter()
                    .filter(|other| other.path == c.path)
                    .map(|other| other.score)
                    .fold(0.0f64, f64::max)
                    .max(f64::EPSILON);
                let weight = if c.exact {
                    1.0
                } else {
                    c.file_score / strongest
                };
                let signal = &signals[&b.stored.passage.id];
                let contextual = b.quality;
                let content = if c.exact {
                    (signal.quality * weight).max(1.0)
                } else {
                    signal.quality * weight * c.qualifier_boost
                };
                let quality = if let Some(parent) = context_parents.get(&i) {
                    // Neighbor context retains the same dependency/discount as the control.
                    blocks[*parent].quality * 0.4
                } else {
                    match options.passage_scoring {
                        PassageScoring::Content => content,
                        PassageScoring::Blended => 0.5 * contextual + 0.5 * content,
                        PassageScoring::Contextual => unreachable!(),
                    }
                };
                scores.insert(
                    b.stored.passage.id,
                    crate::content::PassageScore {
                        passage_id: b.stored.passage.id,
                        contextual_quality: contextual,
                        content_quality: content,
                        selected_quality: quality,
                        content_similarity: signal.similarity,
                        content_bm25: signal.bm25,
                    },
                );
                blocks[i].quality = quality;
            }
        }
        let use_shortlist =
            options.rerank != RerankMode::Off && !candidates.iter().any(|c| c.exact);
        reranking.bypassed = options.rerank != RerankMode::Off && !use_shortlist;
        let mut allowed = BTreeSet::new();
        if use_shortlist {
            let mut ordered: Vec<_> = (0..blocks.len())
                .filter(|&i| !blocks[i].context_only)
                .collect();
            ordered.sort_by(|&a, &b| {
                blocks[b].quality.total_cmp(&blocks[a].quality).then(
                    blocks[a]
                        .stored
                        .passage
                        .id
                        .cmp(&blocks[b].stored.passage.id),
                )
            });
            let mut picked: Vec<_> = ordered.iter().take(32).copied().collect();
            let mut remaining: Vec<_> = ordered.into_iter().skip(32).collect();
            while picked.len() < crate::reranking::LIMIT && !remaining.is_empty() {
                let mut served = BTreeSet::new();
                remaining.retain(|&i| {
                    if picked.len() < crate::reranking::LIMIT
                        && served.insert(blocks[i].stored.path.clone())
                    {
                        picked.push(i);
                        false
                    } else {
                        true
                    }
                });
            }
            reranking.shortlist = picked
                .iter()
                .map(|&i| blocks[i].stored.passage.id)
                .collect();
            allowed.extend(picked.iter().copied());
            if options.rerank == RerankMode::Local {
                let inputs: Vec<_> = picked
                    .iter()
                    .map(|&i| {
                        let p = &blocks[i].stored.passage;
                        let heading: String = p.heading.chars().take(240).collect();
                        let context: String = p
                            .context
                            .as_ref()
                            .map(|c| c.text.chars().take(240).collect())
                            .unwrap_or_default();
                        (p.id, format!("{heading}\n{}\n{context}", p.text))
                    })
                    .collect();
                reranking.score(query, &inputs)?;
                for &i in &picked {
                    let b = &blocks[i];
                    let c = &candidates[b.candidate];
                    let strongest = candidates
                        .iter()
                        .filter(|other| other.path == c.path)
                        .map(|other| other.score)
                        .fold(0.0f64, f64::max)
                        .max(f64::EPSILON);
                    let logit = reranking.cache[&b.stored.passage.id].logit as f64;
                    blocks[i].quality = (1.0 / (1.0 + (-logit).exp())) * c.file_score / strongest
                        * c.qualifier_boost;
                }
            }
            for (&child, &parent) in &context_parents {
                if allowed.contains(&parent) {
                    allowed.insert(child);
                    if options.rerank == RerankMode::Local {
                        blocks[child].quality = blocks[parent].quality * 0.4;
                    }
                }
            }
        }
        let mut api_owners = BTreeMap::new();
        let mut api_links = Vec::new();
        if crate::api_context::enabled(options, candidates.iter().any(|c| c.exact)) {
            let ids: Vec<_> = blocks.iter().map(|b| b.stored.passage.id).collect();
            api_links = self.api_dependencies(&ids)?;
            for link in &api_links {
                let child = blocks
                    .iter()
                    .position(|b| b.stored.passage.id == link.dependent)
                    .expect("existing dependency seed");
                let owner = if let Some(i) = blocks
                    .iter()
                    .position(|b| b.stored.passage.id == link.declaration)
                {
                    i
                } else {
                    let mut stored = self.passage(link.declaration)?;
                    stored.passage.matched_by = vec!["api_context".into()];
                    stored.passage.semantic_similarity = vector.map(|v| cosine(v, &stored.vector));
                    let words = text_terms(&stored.passage.text);
                    blocks.push(Block {
                        stored,
                        candidate: blocks[child].candidate,
                        quality: 0.0,
                        words,
                        context_only: true,
                    });
                    blocks.len() - 1
                };
                api_owners.insert(child, owner);
            }
        }
        let mut choices: Vec<_> = blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| !b.context_only)
            .map(|(i, _)| choice(&blocks, vec![i], candidates))
            .collect();
        for (a, b) in pairs {
            let mut ids = vec![a, b];
            ids.sort_by_key(|&i| blocks[i].stored.passage.bytes[0]);
            choices.push(choice(&blocks, ids, candidates));
        }
        for action in &mut choices {
            if let Payload::Blocks(original) = &action.payload {
                let dependencies: Vec<_> = api_links
                    .iter()
                    .filter(|link| {
                        original
                            .iter()
                            .any(|&i| blocks[i].stored.passage.id == link.dependent)
                    })
                    .cloned()
                    .collect();
                if dependencies.is_empty() {
                    continue;
                }
                let original_ids = original
                    .iter()
                    .map(|&i| blocks[i].stored.passage.id)
                    .collect();
                let mut ids = original.clone();
                for &i in original {
                    if let Some(&owner) = api_owners.get(&i) {
                        ids.push(owner);
                    }
                }
                ids.sort_by_key(|&i| {
                    (
                        blocks[i].stored.passage.bytes[0],
                        blocks[i].stored.passage.id,
                    )
                });
                ids.dedup();
                *action = choice(&blocks, ids, candidates);
                action.trace.kind = "api_bundle".into();
                action.trace.api_dependencies = dependencies;
                action.trace.original_passage_ids = original_ids;
            }
        }
        for choice in &mut choices {
            if use_shortlist
                && let Payload::Blocks(ids) = &choice.payload
                && ids.iter().any(|i| !allowed.contains(i))
            {
                choice.trace.decision = "rerank_shortlist_limit".into();
            }
            choice.trace.passage_scores = choice
                .trace
                .passage_ids
                .iter()
                .filter_map(|id| scores.get(id).cloned())
                .collect();
        }
        let mut selected = BTreeSet::new();
        let mut covered = HashSet::new();
        let mut connected = BTreeSet::new();
        let mut connection_keys = BTreeSet::new();
        let mut connection_count = 0;
        let mut target_cache = BTreeMap::new();
        for step in 1..=MAX_STEPS {
            let before = estimate_tokens(&serde_json::to_string(brief)?);
            let mut best: Option<(usize, f64, bool, Vec<FileMatch>)> = None;
            for (ci, action) in choices.iter_mut().enumerate() {
                if action.trace.step.is_some() || action.trace.decision == "rerank_shortlist_limit"
                {
                    continue;
                }
                let mut files = brief.files.clone();
                let gain;
                let mut eligible = true;
                let mut exact = false;
                match &action.payload {
                    Payload::Blocks(ids) => {
                        if ids.iter().all(|i| selected.contains(i)) {
                            action.trace.decision = "already_covered".into();
                            continue;
                        }
                        let mut within_bundle = Vec::new();
                        let mut gains = Vec::new();
                        for &i in ids {
                            if selected.contains(&i) {
                                continue;
                            }
                            let block = &blocks[i];
                            let p = &block.stored;
                            if files
                                .iter()
                                .flat_map(|f| &f.passages)
                                .any(|b| b.text == p.passage.text)
                            {
                                eligible = false;
                                action.trace.decision = "duplicate".into();
                                break;
                            }
                            let existing = files.iter().position(|f| f.path == p.path);
                            if existing.is_none() && files.len() >= options.max_files {
                                eligible = false;
                                action.trace.decision = "file_limit".into();
                                break;
                            }
                            exact |= action.exact
                                && !brief.files.iter().flat_map(|f| &f.passages).any(|p| {
                                    p.section_id == candidates[block.candidate].section_id
                                });
                            let redundancy = selected
                                .iter()
                                .chain(within_bundle.iter())
                                .map(|&j: &usize| {
                                    duplicate(
                                        &block.words,
                                        &blocks[j].words,
                                        &p.vector,
                                        &blocks[j].stored.vector,
                                    )
                                })
                                .fold(0.0f64, f64::max);
                            let fresh = terms
                                .iter()
                                .filter(|t| block.words.contains(*t) && !covered.contains(*t))
                                .count() as f64
                                / terms.len().max(1) as f64;
                            gains.push(
                                block.quality.powi(2)
                                    * (1.0 - 0.75 * redundancy)
                                    * (1.0 + 0.15 * fresh),
                            );
                            within_bundle.push(i);
                            if let Some(fi) = existing {
                                files[fi].passages.push(p.passage.clone());
                            } else {
                                files.push(FileMatch {
                                    rank: files.len() + 1,
                                    path: p.path.clone(),
                                    title: p.title.clone(),
                                    source_hash: p.hash.clone(),
                                    score: candidates[block.candidate].file_score,
                                    role: candidates[block.candidate].role.clone(),
                                    passages: vec![p.passage.clone()],
                                    connections: Vec::new(),
                                });
                            }
                        }
                        gains.sort_by(|a, b| b.total_cmp(a));
                        gain = gains.first().copied().unwrap_or(0.0)
                            + 0.25 * gains.iter().skip(1).sum::<f64>();
                    }
                    Payload::Connection(fi, conn, cv, words) => {
                        if connection_count >= 6 {
                            action.trace.decision = "connection_limit".into();
                            continue;
                        }
                        if brief.files.iter().any(|f| {
                            f.path == conn.path
                                && conn
                                    .anchor
                                    .as_ref()
                                    .is_none_or(|a| f.passages.iter().any(|p| &p.anchor == a))
                        }) {
                            action.trace.decision = "already_covered".into();
                            continue;
                        }
                        let redundancy = selected
                            .iter()
                            .map(|&j: &usize| {
                                duplicate(words, &blocks[j].words, cv, &blocks[j].stored.vector)
                            })
                            .fold(0.0f64, f64::max);
                        gain = action.trace.quality.powi(2) * (1.0 - 0.75 * redundancy);
                        files[*fi].connections.push(conn.as_ref().clone());
                    }
                }
                if !eligible {
                    continue;
                }
                // Measure the actual added serialized bytes, including new file metadata.
                let previous = std::mem::replace(&mut brief.files, files);
                let after = estimate_tokens(&serde_json::to_string(brief)?);
                let files = std::mem::replace(&mut brief.files, previous);
                let cost = after.saturating_sub(before).max(1);
                action.trace.incremental_tokens = cost;
                action.trace.marginal_gain = gain;
                if after + 32 > options.max_tokens {
                    action.trace.decision = "budget".into();
                    continue;
                }
                action.trace.decision = "lower_utility".into();
                // Sublinear cost avoids equating the shortest fragment with the best evidence.
                let utility = gain / (cost.max(256) as f64).powf(0.35);
                if best
                    .as_ref()
                    .is_none_or(|(_, s, e, _)| (exact && !e) || (exact == *e && utility > *s))
                {
                    best = Some((ci, utility, exact, files));
                }
            }
            let Some((ci, _, _, files)) = best else {
                break;
            };
            brief.files = files;
            choices[ci].trace.decision = "selected".into();
            choices[ci].trace.step = Some(step);
            match &choices[ci].payload {
                Payload::Blocks(ids) => {
                    for &i in ids {
                        selected.insert(i);
                        covered.extend(blocks[i].words.iter().cloned());
                    }
                }
                Payload::Connection(..) => connection_count += 1,
            }
            if options.connections {
                // One bounded relationship discovery per selected file, from its first evidence.
                for (fi, f) in brief.files.iter().enumerate() {
                    if !connected.insert(f.path.clone()) {
                        continue;
                    }
                    let ids: Vec<_> = f.passages.iter().map(|p| p.id).collect();
                    for mut conn in self.connections(&f.path, Some(&ids), 8)? {
                        if !in_scope(&conn.path, &options.scope) {
                            continue;
                        }
                        let key = (conn.path.clone(), conn.anchor.clone());
                        if !connection_keys.insert(key.clone()) {
                            continue;
                        }
                        let target = if let Some(target) = target_cache.get(&key) {
                            target
                        } else {
                            let mut stmt = self.db.prepare("SELECT p.id FROM passages p JOIN sections s ON s.id=p.section_id WHERE p.path=? AND (? IS NULL OR s.anchor=?) AND p.view!='identity' ORDER BY p.start_byte LIMIT 64")?;
                            let ids = stmt
                                .query_map(
                                    rusqlite::params![conn.path, conn.anchor, conn.anchor],
                                    |r| r.get::<_, i64>(0),
                                )?
                                .collect::<rusqlite::Result<Vec<_>>>()?;
                            let mut best: Option<(f64, StoredPassage)> = None;
                            for id in ids {
                                let p = self.passage(id)?;
                                let score = vector.map_or_else(
                                    || overlap(&text_terms(&p.passage.text), &terms),
                                    |v| relevance(cosine(v, &p.vector), options.min_similarity),
                                );
                                if best.as_ref().is_none_or(|(s, _)| score > *s) {
                                    best = Some((score, p));
                                }
                            }
                            target_cache.entry(key).or_insert(best)
                        };
                        let Some((score, p)) = target else {
                            continue;
                        };
                        if *score <= 0.0 {
                            continue;
                        }
                        if conn.kind == "references"
                            && !conn.evidence.iter().any(|e| e.path == conn.path)
                        {
                            conn.evidence.push(self.evidence(p.passage.id, None)?);
                        }
                        let quality = 0.7 * score;
                        choices.push(Choice {
                            exact: false,
                            trace: SelectionDecision {
                                kind: "connection".into(),
                                path: f.path.clone(),
                                passage_ids: Vec::new(),
                                connection_path: Some(conn.path.clone()),
                                quality,
                                marginal_gain: 0.0,
                                incremental_tokens: 0,
                                decision: "lower_utility".into(),
                                step: None,
                                passage_scores: Vec::new(),
                                api_dependencies: Vec::new(),
                                original_passage_ids: Vec::new(),
                            },
                            payload: Payload::Connection(
                                fi,
                                Box::new(conn),
                                p.vector.clone(),
                                text_terms(&p.passage.text),
                            ),
                        });
                    }
                }
            }
        }
        for (ci, c) in candidates.iter_mut().enumerate() {
            for &i in &selected {
                if blocks[i].candidate != ci {
                    continue;
                }
                let p = &blocks[i].stored.passage;
                // API context can belong to an ancestor section that was not retrieved.
                // Keep its actual source section; do not invent a hit in the child's section.
                if p.matched_by.iter().any(|s| s == "api_context") {
                    continue;
                }
                c.selected_passages.push(p.id);
                if !c.hits.iter().any(|h| h.passage_id == p.id) {
                    c.hits.push(Hit {
                        passage_id: p.id,
                        view: "context".into(),
                        channel: "adjacent_context".into(),
                        rank: 0,
                        similarity: p.semantic_similarity,
                        lexical_score: None,
                        lines: p.lines,
                        contribution: 0.0,
                        selection: "selected".into(),
                    });
                }
            }
            for h in &mut c.hits {
                h.selection = if c.selected_passages.contains(&h.passage_id) {
                    "selected"
                } else if h.view == "identity" {
                    "heading_context"
                } else if ci >= MAX_SECTIONS
                    || !blocks.iter().any(|b| b.stored.passage.id == h.passage_id)
                {
                    "selection_pool_limit"
                } else if use_shortlist
                    && !blocks
                        .iter()
                        .enumerate()
                        .any(|(i, b)| b.stored.passage.id == h.passage_id && allowed.contains(&i))
                {
                    "rerank_shortlist_limit"
                } else if choices.iter().any(|ch| {
                    (ch.trace.kind == "block" || ch.trace.kind == "api_bundle")
                        && ch.trace.passage_ids.contains(&h.passage_id)
                        && ch.trace.decision == "budget"
                }) {
                    "budget"
                } else {
                    "lower_utility"
                }
                .into();
            }
            c.decision = if !c.selected_passages.is_empty() {
                "selected"
            } else if ci >= MAX_SECTIONS {
                "selection_pool_limit"
            } else if !brief.files.iter().any(|f| f.path == c.path)
                && brief.files.len() >= options.max_files
            {
                "file_limit"
            } else if c.hits.iter().any(|h| h.selection == "budget") {
                "budget"
            } else {
                "lower_utility"
            }
            .into();
        }
        if choices.iter().filter(|c| c.trace.step.is_some()).count() == MAX_STEPS {
            for c in &mut choices {
                if c.trace.decision == "lower_utility" {
                    c.trace.decision = "selection_step_limit".into();
                }
            }
        }
        brief.truncated |= candidates.iter().any(|c| c.selected_passages.is_empty())
            || choices
                .iter()
                .any(|c| matches!(c.trace.decision.as_str(), "budget" | "selection_step_limit"))
            || brief
                .files
                .iter()
                .flat_map(|f| &f.passages)
                .any(|p| p.excerpted);
        Ok(choices.into_iter().map(|c| c.trace).collect())
    }
}
