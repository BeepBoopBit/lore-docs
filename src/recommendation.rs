//! A conservative reading choice, without a semantic completeness claim.
use crate::{
    briefing::GuidedRead,
    index::Index,
    procedure::{covered, visible_spans},
    search::{Diagnostics, query_terms, text_terms},
};
use anyhow::{Result, ensure};
use rusqlite::params;
use serde::Serialize;
use std::collections::BTreeSet;

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadChoice {
    Focused,
    Broader,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadReason {
    DistinctQueryTerms,
    InsufficientBodyEvidence,
    NoBoundedParent,
    ParentCostLimit,
    ExactLookup,
    ProbeLimit,
}

impl ReadReason {
    pub fn explanation(&self) -> &'static str {
        match self {
            Self::DistinctQueryTerms => "A sibling's omitted body adds distinct query terms.",
            Self::InsufficientBodyEvidence => {
                "The bounded body probe did not establish separate query terms in focused and sibling source."
            }
            Self::NoBoundedParent => "No bounded parent option is present.",
            Self::ParentCostLimit => "The parent exceeds the recommendation's source-size limit.",
            Self::ExactLookup => "An exact lookup does not trigger broader recommendations.",
            Self::ProbeLimit => "The body probe reached its limit; keep the focused read.",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ReadRecommendation {
    /// Zero-based index into guidance.next_reads. Current policy keeps its first entry.
    pub next_read_index: usize,
    pub choice: ReadChoice,
    pub reason: ReadReason,
    /// Raw UTF-8 source, including context introductions; excludes JSON metadata.
    pub source_bytes: usize,
    /// Extra source relative to the focused read, not relative to the brief.
    pub additional_source_bytes: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub focus_terms: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub added_terms: Vec<String>,
}

impl Index {
    pub(crate) fn recommend_read(
        &self,
        diagnostic: &Diagnostics,
        hint: &GuidedRead,
    ) -> Result<ReadRecommendation> {
        let tx = self.db.unchecked_transaction()?;
        let (path, anchor) = hint
            .read_target
            .split_once('#')
            .map_or((hint.read_target.as_str(), None), |(p, a)| (p, Some(a)));
        let source = self.read_source(path)?;
        ensure!(
            source.root == diagnostic.brief.root
                && source.revision == diagnostic.brief.index_revision
                && diagnostic
                    .brief
                    .files
                    .iter()
                    .any(|f| f.path == path && f.source_hash == source.hash),
            "Recommendation snapshot changed; rerun the query"
        );
        let section: i64 = self.db.query_row(
            "SELECT id FROM sections WHERE path=? AND ((? IS NULL AND parent_id IS NULL) OR (anchor=? AND parent_id IS NOT NULL))",
            params![path, anchor, anchor], |r| r.get(0))?;
        let ranges = self.guidance_ranges(section, hint.context)?;
        let mut result = ReadRecommendation {
            next_read_index: 0,
            choice: ReadChoice::Focused,
            reason: ReadReason::NoBoundedParent,
            source_bytes: ranges.iter().map(|[a, b]| b - a).sum(),
            additional_source_bytes: 0,
            focus_terms: Vec::new(),
            added_terms: Vec::new(),
        };
        if diagnostic.candidates.iter().any(|c| c.exact) {
            result.reason = ReadReason::ExactLookup;
        } else if let Some(parent) = &hint.broader_read {
            // This is stricter than offering an optional parent: recommend only
            // small additions, without claiming they fit one paginated response.
            if parent.source_bytes > 4096 || parent.additional_source_bytes > 2048 {
                result.reason = ReadReason::ParentCostLimit;
            } else {
                let terms: BTreeSet<_> = query_terms(&diagnostic.brief.query)
                    .into_iter()
                    .filter(|t| (2..=64).contains(&t.len()))
                    .collect();
                let visible = visible_spans(&diagnostic.brief);
                let spans = visible.get(path).map(Vec::as_slice).unwrap_or(&[]);
                let mut focus = BTreeSet::new();
                let mut remaining = 128;
                let mut limited = false;
                for range in &ranges {
                    let (all, _, limit, used) =
                        self.read_terms(path, *range, &terms, spans, remaining)?;
                    focus.extend(all);
                    limited |= limit;
                    remaining = remaining.saturating_sub(used);
                }
                result.reason = if limited {
                    ReadReason::ProbeLimit
                } else {
                    ReadReason::InsufficientBodyEvidence
                };
                if !limited && !focus.is_empty() {
                    for target in &parent.matching_sections {
                        let Some(candidate) = diagnostic.candidates.iter().take(128).find(|c| {
                            c.path == path
                                && format!("{}#{}", c.path, c.anchor) == *target
                                && c.section_id != section
                                && c.hits.iter().any(|h| {
                                    h.view != "identity"
                                        && matches!(h.channel.as_str(), "keyword" | "semantic")
                                })
                        }) else {
                            continue;
                        };
                        let bounds: [usize; 2] = self.db.query_row(
                            "SELECT start_byte,end_byte FROM sections WHERE id=?",
                            [candidate.section_id],
                            |r| Ok([r.get::<_, u32>(0)? as usize, r.get::<_, u32>(1)? as usize]),
                        )?;
                        let (all, omitted, limit, _) =
                            self.read_terms(path, bounds, &terms, spans, 128)?;
                        if limit {
                            result.reason = ReadReason::ProbeLimit;
                            continue;
                        }
                        let distinct: Vec<_> = focus.difference(&all).take(3).cloned().collect();
                        let added: Vec<_> = omitted.difference(&focus).take(3).cloned().collect();
                        if !distinct.is_empty() && !added.is_empty() {
                            result.choice = ReadChoice::Broader;
                            result.reason = ReadReason::DistinctQueryTerms;
                            result.source_bytes = parent.source_bytes;
                            result.additional_source_bytes = parent.additional_source_bytes;
                            result.focus_terms = distinct;
                            result.added_terms = added;
                            break;
                        }
                    }
                }
            }
        }
        tx.commit()?;
        Ok(result)
    }

    // Whole body blocks only: headings, ancestors outside the reading plan,
    // partial fragments, and semantic-only similarity cannot supply a term.
    fn read_terms(
        &self,
        path: &str,
        range: [usize; 2],
        query: &BTreeSet<String>,
        visible: &[[usize; 2]],
        limit: usize,
    ) -> Result<(BTreeSet<String>, BTreeSet<String>, bool, usize)> {
        let mut stmt = self.db.prepare("SELECT text,start_byte,end_byte FROM passages WHERE path=? AND kind<>'heading' AND partial=0 AND start_byte>=? AND end_byte<=? ORDER BY start_byte,id LIMIT ?")?;
        let rows = stmt
            .query_map(
                params![path, range[0] as i64, range[1] as i64, (limit + 1) as i64],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        [r.get::<_, u32>(1)? as usize, r.get::<_, u32>(2)? as usize],
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let limited = rows.len() > limit;
        let used = rows.len().min(limit);
        let mut all = BTreeSet::new();
        let mut omitted = BTreeSet::new();
        for (text, bounds) in rows.into_iter().take(limit) {
            let words = text_terms(&text);
            let matches: BTreeSet<_> = query
                .iter()
                .filter(|t| words.contains(*t))
                .cloned()
                .collect();
            all.extend(matches.iter().cloned());
            if !covered(bounds, visible) {
                omitted.extend(matches);
            }
        }
        Ok((all, omitted, limited, used))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        embedding::Embedder,
        search::{BriefOptions, Mode},
    };
    use std::fs;
    use tempfile::TempDir;

    struct Encoder;
    impl Embedder for Encoder {
        fn identity(&self) -> &str {
            "recommendation-test"
        }
        fn dimensions(&self) -> usize {
            384
        }
        fn token_count(&self, text: &str) -> Result<usize> {
            Ok(text.len().div_ceil(3))
        }
        fn embed_documents(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            Ok(texts
                .iter()
                .map(|_| {
                    let mut v = vec![0.0; 384];
                    v[0] = 1.0;
                    v
                })
                .collect())
        }
        fn embed_query(&mut self, _: &str) -> Result<Vec<f32>> {
            unreachable!()
        }
    }
    fn fixture(
        query: &str,
        sibling: &str,
        padding: &str,
    ) -> (TempDir, TempDir, Index, Diagnostics, GuidedRead) {
        let root = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        fs::write(root.path().join("guide.md"),format!("# Guide\n\nShared introduction.\n\n## Settings\n\nParent setup.\n\n### Executable\n\nbridge executable selection.\n\nAn omitted restriction.\n\n### Timeout duration\n\n{sibling}\n\n{padding}\n")).unwrap();
        let mut index = Index::open(state.path()).unwrap();
        index.index(root.path(), &mut Encoder).unwrap();
        let mut d = index
            .diagnose(
                query,
                None,
                &BriefOptions {
                    mode: Mode::Keyword,
                    connections: false,
                    ..Default::default()
                },
            )
            .unwrap();
        d.brief.files[0]
            .passages
            .retain(|p| p.text.contains("bridge executable selection"));
        let c = index.compact_diagnostics(&d, 2000).unwrap();
        let h = &c.next_reads[0];
        let hint = GuidedRead {
            read_target: h.read_target.clone(),
            context: h.context,
            gaps: h.gaps.clone(),
            broader_read: h.broader_read.clone(),
        };
        assert!(hint.broader_read.is_some());
        (root, state, index, d, hint)
    }
    #[test]
    fn recommendation_uses_omitted_body_terms_and_exact_utf8_read_cost() {
        let (_root, _state, index, d, h) = fixture(
            "bridge executable timeout duration",
            "timeout duration is bounded 検証🐦.",
            "",
        );
        let r = index.recommend_read(&d, &h).unwrap();
        assert_eq!(r.choice, ReadChoice::Broader);
        assert_eq!(r.reason, ReadReason::DistinctQueryTerms);
        assert!(r.added_terms.contains(&"timeout".into()));
        let parent = h.broader_read.as_ref().unwrap();
        let read = index
            .read_context(&parent.read_target, &Default::default())
            .unwrap();
        assert_eq!(
            r.source_bytes,
            read.parts
                .iter()
                .map(|p| p.content.text.len())
                .sum::<usize>()
        );
        let focus = index
            .read_context(&h.read_target, &Default::default())
            .unwrap();
        assert_eq!(
            r.source_bytes - r.additional_source_bytes,
            focus
                .parts
                .iter()
                .map(|p| p.content.text.len())
                .sum::<usize>()
        );
    }
    #[test]
    fn recommendation_stays_focused_for_narrow_heading_only_or_visible_terms() {
        for (query, sibling) in [
            (
                "bridge executable",
                "bridge executable timeout duration is bounded.",
            ),
            (
                "bridge executable timeout duration",
                "bridge executable has another restriction.",
            ),
        ] {
            let (_root, _state, index, d, h) = fixture(query, sibling, "");
            assert_eq!(
                index.recommend_read(&d, &h).unwrap().choice,
                ReadChoice::Focused
            );
        }
        let (_root, _state, index, mut d, h) = fixture(
            "bridge executable timeout duration",
            "timeout duration is bounded.",
            "",
        );
        let full = index
            .diagnose(
                "timeout duration",
                None,
                &BriefOptions {
                    mode: Mode::Keyword,
                    connections: false,
                    ..Default::default()
                },
            )
            .unwrap();
        d.brief.files[0].passages.extend(
            full.brief.files[0]
                .passages
                .iter()
                .filter(|p| p.text == "timeout duration is bounded.")
                .cloned(),
        );
        assert_eq!(
            index.recommend_read(&d, &h).unwrap().choice,
            ReadChoice::Focused
        );
    }
    #[test]
    fn recommendation_respects_exact_lookup_parent_cost_and_snapshot_guards() {
        let (root, _state, index, mut d, h) = fixture(
            "bridge executable timeout duration",
            "timeout duration is bounded.",
            "",
        );
        d.candidates[0].exact = true;
        assert_eq!(
            index.recommend_read(&d, &h).unwrap().reason,
            ReadReason::ExactLookup
        );
        d.candidates[0].exact = false;
        d.brief.index_revision += 1;
        assert!(
            index
                .recommend_read(&d, &h)
                .unwrap_err()
                .to_string()
                .contains("snapshot changed")
        );
        d.brief.index_revision -= 1;
        fs::write(root.path().join("guide.md"), "changed").unwrap();
        assert!(index.recommend_read(&d, &h).is_err());
        let (_root, _state, index, d, h) = fixture(
            "bridge executable timeout duration",
            "timeout duration is bounded.",
            &"x".repeat(2300),
        );
        assert_eq!(
            index.recommend_read(&d, &h).unwrap().reason,
            ReadReason::ParentCostLimit
        );
    }
    #[test]
    fn recommendation_abstains_on_incomplete_body_probe() {
        let (_root, _state, index, d, h) = fixture(
            "bridge executable timeout duration",
            "timeout duration is bounded.",
            &"z\n\n".repeat(140),
        );
        let r = index.recommend_read(&d, &h).unwrap();
        assert_eq!(r.choice, ReadChoice::Focused);
        assert_eq!(r.reason, ReadReason::ProbeLimit);
    }
}
