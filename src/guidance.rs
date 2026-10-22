//! Bounded source omissions and reading targets, without an answerability claim.
use crate::{
    api_context::Activation,
    index::Index,
    procedure::{covered, visible_spans},
    reading::{Source, TOKEN_METHOD, cost},
    search::Diagnostics,
};
use anyhow::{Result, ensure};
use rusqlite::params;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

const MAX_SECTIONS: usize = 64;
const MAX_FILES: usize = 16;
const MAX_BLOCKS: usize = 128;
const MAX_ANCESTORS: usize = 8;
const MAX_READS: usize = 6;
const MAX_PARENT_CANDIDATES: usize = 128;
const MAX_PARENT_BYTES: usize = 6 * 1024;
const MAX_ADDED_BYTES: usize = 4 * 1024;

#[derive(Debug, Serialize)]
pub struct CompactDiagnostics<'a> {
    pub query: &'a str,
    pub root: &'a str,
    pub index_revision: u64,
    pub scope: &'a str,
    pub retrieval_status: &'a str,
    pub brief_truncated: bool,
    pub answer_completeness: &'static str,
    pub api_context: &'a Activation,
    pub next_reads: Vec<NextRead>,
    /// Suggestions excluded by the response budget or the six-read limit.
    pub omitted_suggestions: usize,
    /// Suggestions whose known omissions are covered by an earlier returned read.
    pub consolidated_suggestions: usize,
    pub probe: Probe,
    pub limitations: Vec<&'static str>,
    pub estimated_tokens: usize,
    pub token_count_method: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct Probe {
    pub selected_sections: usize,
    pub examined_sections: usize,
    pub limited: bool,
}

#[derive(Debug, Serialize)]
pub struct NextRead {
    pub read_target: String,
    /// Pass --context to read when enclosing introductions have omitted blocks.
    pub context: bool,
    pub source_hash: String,
    pub gaps: Gaps,
    /// At most three indexed blocks not fully visible in the brief; not every gap.
    pub examples: Vec<MissingExample>,
    /// Only the known omissions are covered, not necessarily each whole section.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub covers_gaps_from: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub broader_read: Option<BroaderRead>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BroaderRead {
    pub read_target: String,
    pub context: bool,
    /// Query-discovered sibling sections, not a claim of required task context.
    pub matching_sections: Vec<String>,
    /// Full source size including introductions, excluding response metadata.
    pub source_bytes: usize,
    pub additional_source_bytes: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Gaps {
    pub numbered_steps: usize,
    pub numbered_context_blocks: usize,
    pub section_blocks: usize,
    pub ancestor_blocks: usize,
}

#[derive(Debug, Serialize)]
pub struct MissingExample {
    pub kind: &'static str,
    pub bytes: [usize; 2],
    pub lines: [usize; 2],
}

impl NextRead {
    fn example(&mut self, kind: &'static str, bytes: [usize; 2], source: &Source) {
        if self.examples.len() < 3 && !self.examples.iter().any(|e| e.bytes == bytes) {
            self.examples.push(MissingExample {
                kind,
                bytes,
                lines: source.lines(bytes[0], bytes[1]),
            });
        }
    }
}

struct Section {
    path: String,
    parent: Option<i64>,
    anchor: String,
    bytes: [usize; 2],
}

impl Section {
    fn target(&self) -> String {
        if self.parent.is_none() {
            self.path.clone()
        } else {
            format!("{}#{}", self.path, self.anchor)
        }
    }
}

struct Suggested {
    next: NextRead,
    section_id: i64,
    path: String,
    omitted: Vec<[usize; 2]>,
    read_ranges: Vec<[usize; 2]>,
}

impl Index {
    /// Summarize an existing diagnostic snapshot without model calls or reselecting evidence.
    /// The entire response has its own budget; the original brief is not included or reduced.
    pub fn compact_diagnostics<'a>(
        &self,
        diagnostic: &'a Diagnostics,
        max_tokens: usize,
    ) -> Result<CompactDiagnostics<'a>> {
        self.pack_guidance(diagnostic, max_tokens, MAX_READS, cost)
    }

    // Use the actual output representation for packing, so condensed guidance
    // consolidates only against reads that really fit its smaller budget.
    pub(crate) fn pack_guidance<'a>(
        &self,
        diagnostic: &'a Diagnostics,
        max_tokens: usize,
        max_reads: usize,
        measure: impl Fn(&CompactDiagnostics<'a>) -> Result<usize>,
    ) -> Result<CompactDiagnostics<'a>> {
        ensure!(
            (128..=100_000).contains(&max_tokens),
            "max-tokens must be between 128 and 100000"
        );
        let tx = self.db.unchecked_transaction()?;
        let brief = &diagnostic.brief;
        ensure!(
            self.root()?.to_string_lossy() == brief.root
                && self.revision()? == brief.index_revision,
            "Diagnostic snapshot changed; rerun the query before requesting compact guidance"
        );
        let visible = visible_spans(brief);
        let mut seen = BTreeSet::new();
        let sections: Vec<_> = brief
            .files
            .iter()
            .flat_map(|f| f.passages.iter().map(move |p| (f, p)))
            .filter(|(_, p)| seen.insert(p.section_id))
            .collect();
        let mut result = CompactDiagnostics {
            query: &brief.query,
            root: &brief.root,
            index_revision: brief.index_revision,
            scope: &diagnostic.scope,
            retrieval_status: &brief.status,
            brief_truncated: brief.truncated,
            answer_completeness: "not_assessed",
            api_context: &diagnostic.api_context,
            next_reads: Vec::new(),
            omitted_suggestions: 0,
            consolidated_suggestions: 0,
            probe: Probe {
                selected_sections: sections.len(),
                examined_sections: 0,
                limited: sections.len() > MAX_SECTIONS,
            },
            limitations: vec![
                "Structural omissions only, not required answer facts. Empty hints do not establish completeness; matches do not establish support for the requested capability.",
                "Gaps cover selected sections and ancestor introductions; categories can overlap. Consolidation covers known gaps, not whole sections. Parent alternatives use query-discovered siblings, not inferred dependencies. No cross-file dependency check.",
                "Probe: 64 selected sections, 16 files, 128 blocks/section, 8 ancestors; numbered-list grammar is bounded separately. Parent probe: 128 candidates, 6 KiB source and 4 KiB added. At most 6 reads; follow continuations. Parent alternatives and covered-target lists may be omitted for space.",
            ],
            estimated_tokens: max_tokens,
            token_count_method: TOKEN_METHOD,
        };
        ensure!(
            measure(&result)? + 8 <= max_tokens,
            "max-tokens too small for compact diagnostic metadata; increase --max-tokens"
        );
        let mut sources = BTreeMap::<String, Source>::new();
        let mut suggestions = Vec::new();
        for (file, passage) in sections.into_iter().take(MAX_SECTIONS) {
            if !sources.contains_key(&file.path) {
                if sources.len() == MAX_FILES {
                    result.probe.limited = true;
                    continue;
                }
                sources.insert(file.path.clone(), self.read_source(&file.path)?);
            }
            let source = &sources[&file.path];
            ensure!(
                source.hash == file.source_hash,
                "Diagnostic source changed; rerun the query"
            );
            let section = self.guidance_section(passage.section_id)?;
            ensure!(
                section.path == file.path
                    && source
                        .text
                        .get(section.bytes[0]..section.bytes[1])
                        .is_some(),
                "Invalid diagnostic section; rerun the query"
            );
            result.probe.examined_sections += 1;
            let mut suggestion = NextRead {
                read_target: section.target(),
                context: false,
                source_hash: source.hash.clone(),
                gaps: Gaps::default(),
                examples: Vec::new(),
                covers_gaps_from: Vec::new(),
                broader_read: None,
            };
            let mut omitted_ranges = Vec::new();
            for group in diagnostic
                .procedure_coverage
                .groups
                .iter()
                .filter(|g| g.section_id == passage.section_id)
            {
                for missing in &group.missing {
                    omitted_ranges.push(missing.bytes);
                    let kind = if missing.role == "step" {
                        suggestion.gaps.numbered_steps += 1;
                        "numbered_step"
                    } else {
                        suggestion.gaps.numbered_context_blocks += 1;
                        "numbered_context"
                    };
                    suggestion.example(kind, missing.bytes, source);
                }
            }
            let spans = visible
                .get(file.path.as_str())
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let (omitted, limited) =
                self.omitted_blocks(passage.section_id, section.bytes, spans)?;
            result.probe.limited |= limited;
            suggestion.gaps.section_blocks = omitted.len();
            omitted_ranges.extend(&omitted);
            if let Some(&bytes) = omitted.first() {
                suggestion.example("section_block", bytes, source);
            }
            let mut parent = section.parent;
            let mut visited = BTreeSet::from([passage.section_id]);
            for _ in 0..MAX_ANCESTORS {
                let Some(id) = parent else { break };
                ensure!(
                    visited.insert(id),
                    "Cyclic section ancestry; reindex required"
                );
                let ancestor = self.guidance_section(id)?;
                ensure!(
                    ancestor.path == file.path
                        && ancestor.bytes[0] <= section.bytes[0]
                        && ancestor.bytes[1] >= section.bytes[1],
                    "Invalid section ancestry; reindex required"
                );
                let child_start: Option<u32> = self.db.query_row(
                    "SELECT MIN(start_byte) FROM sections WHERE parent_id=?",
                    [id],
                    |r| r.get(0),
                )?;
                let bounds = [
                    ancestor.bytes[0],
                    child_start.map(|n| n as usize).unwrap_or(ancestor.bytes[1]),
                ];
                let (missing, limited) = self.omitted_blocks(id, bounds, spans)?;
                result.probe.limited |= limited;
                suggestion.gaps.ancestor_blocks += missing.len();
                omitted_ranges.extend(&missing);
                if let Some(&bytes) = missing.first() {
                    suggestion.example("ancestor_block", bytes, source);
                }
                parent = ancestor.parent;
            }
            result.probe.limited |= parent.is_some();
            suggestion.context = suggestion.gaps.ancestor_blocks > 0;
            if !suggestion.examples.is_empty() {
                let read_ranges = self.guidance_ranges(passage.section_id, suggestion.context)?;
                suggestions.push(Suggested {
                    next: suggestion,
                    section_id: passage.section_id,
                    path: file.path.clone(),
                    omitted: omitted_ranges,
                    read_ranges,
                });
            }
        }
        // Recognized partial lists have a specific structural relationship. Other
        // suggestions retain first-seen brief order, not a new relevance score.
        suggestions
            .sort_by_key(|s| s.next.gaps.numbered_steps + s.next.gaps.numbered_context_blocks == 0);
        result.omitted_suggestions = suggestions.len();
        let mut accepted: Vec<(i64, String, Vec<[usize; 2]>)> = Vec::new();
        for suggestion in suggestions {
            // Only an already packed read can suppress a suggestion. If the earlier
            // read does not fit, the later focused target remains a viable fallback.
            if let Some(at) = accepted.iter().position(|(_, path, ranges)| {
                path == &suggestion.path && suggestion.omitted.iter().all(|&r| covered(r, ranges))
            }) {
                result.consolidated_suggestions += 1;
                result.omitted_suggestions -= 1;
                result.next_reads[at]
                    .covers_gaps_from
                    .push(suggestion.next.read_target);
                if measure(&result)? + 2 > max_tokens {
                    result.next_reads[at].covers_gaps_from.pop();
                }
                continue;
            }
            if result.next_reads.len() == max_reads {
                continue;
            }
            result.next_reads.push(suggestion.next);
            result.omitted_suggestions -= 1;
            if measure(&result)? + 2 > max_tokens {
                result.next_reads.pop();
                result.omitted_suggestions += 1;
            } else {
                accepted.push((
                    suggestion.section_id,
                    suggestion.path,
                    suggestion.read_ranges,
                ));
            }
        }
        // Optional broader reads spend only the space left after focused guidance.
        let mut offered = BTreeSet::new();
        result.probe.limited |= diagnostic.candidates.len() > MAX_PARENT_CANDIDATES;
        for (at, (section, path, ranges)) in accepted.iter().enumerate() {
            if let Some(parent) =
                self.broader_read(*section, result.next_reads[at].context, ranges, diagnostic)?
            {
                if offered.contains(&parent.read_target) {
                    continue;
                }
                let parent_id = self
                    .guidance_section(*section)?
                    .parent
                    .expect("broader read has a parent");
                let parent_ranges = self.guidance_ranges(parent_id, parent.context)?;
                if accepted.iter().any(|(_, p, r)| {
                    p == path && parent_ranges.iter().all(|&span| covered(span, r))
                }) {
                    continue;
                }
                let target = parent.read_target.clone();
                result.next_reads[at].broader_read = Some(parent);
                if measure(&result)? + 2 > max_tokens {
                    result.next_reads[at].broader_read = None;
                } else {
                    offered.insert(target);
                }
            }
        }
        loop {
            let measured = measure(&result)?;
            if result.estimated_tokens == measured {
                break;
            }
            result.estimated_tokens = measured;
        }
        ensure!(
            measure(&result)? <= max_tokens,
            "Internal compact diagnostic budget exceeded"
        );
        tx.commit()?;
        Ok(result)
    }

    fn broader_read(
        &self,
        section_id: i64,
        context: bool,
        focused: &[[usize; 2]],
        diagnostic: &Diagnostics,
    ) -> Result<Option<BroaderRead>> {
        let section = self.guidance_section(section_id)?;
        let Some(parent_id) = section.parent else {
            return Ok(None);
        };
        let parent = self.guidance_section(parent_id)?;
        let Some(grandparent_id) = parent.parent else {
            return Ok(None);
        };
        // Do not turn a focused suggestion into a whole-file/top-level-heading read.
        if self.guidance_section(grandparent_id)?.parent.is_none() {
            return Ok(None);
        }
        let ranges = self.guidance_ranges(parent_id, context)?;
        let source_bytes: usize = ranges.iter().map(|[a, b]| b - a).sum();
        let focused_bytes: usize = focused.iter().map(|[a, b]| b - a).sum();
        ensure!(
            focused.iter().all(|&r| covered(r, &ranges)),
            "Parent reading plan does not enclose focused read"
        );
        let added = source_bytes.saturating_sub(focused_bytes);
        if source_bytes > MAX_PARENT_BYTES || added > MAX_ADDED_BYTES {
            return Ok(None);
        }
        let mut matches = Vec::new();
        let mut seen = BTreeSet::new();
        for c in diagnostic.candidates.iter().take(MAX_PARENT_CANDIDATES) {
            if c.path != section.path
                || !c.hits.iter().any(|h| {
                    h.view != "identity" && matches!(h.channel.as_str(), "keyword" | "semantic")
                })
                || !seen.insert(c.section_id)
            {
                continue;
            }
            let sibling = self.guidance_section(c.section_id)?;
            if sibling.parent == Some(parent_id) {
                matches.push(sibling.target());
                if matches.len() == 3 {
                    break;
                }
            }
        }
        if matches.len() < 2 {
            return Ok(None);
        }
        Ok(Some(BroaderRead {
            read_target: parent.target(),
            context,
            matching_sections: matches,
            source_bytes,
            additional_source_bytes: added,
        }))
    }

    /// Match read --context's source slices, including introductions already visible
    /// in the brief. Coverage comparisons must account for everything the read returns.
    pub(crate) fn guidance_ranges(
        &self,
        section_id: i64,
        context: bool,
    ) -> Result<Vec<[usize; 2]>> {
        let focus = self.guidance_section(section_id)?;
        let mut ranges = vec![focus.bytes];
        let mut parent = if context { focus.parent } else { None };
        let mut seen = BTreeSet::from([section_id]);
        for _ in 0..MAX_ANCESTORS {
            let Some(id) = parent else { break };
            ensure!(seen.insert(id), "Cyclic section ancestry; reindex required");
            let s = self.guidance_section(id)?;
            ensure!(
                s.path == focus.path
                    && s.bytes[0] <= focus.bytes[0]
                    && s.bytes[1] >= focus.bytes[1],
                "Invalid section ancestry; reindex required"
            );
            let stop: Option<u32> = self.db.query_row(
                "SELECT MIN(start_byte) FROM sections WHERE parent_id=?",
                [id],
                |r| r.get(0),
            )?;
            let stop = stop.map(|n| n as usize).unwrap_or(s.bytes[1]);
            let substantive: bool = self.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM passages WHERE section_id=? AND kind<>'heading')",
                [id],
                |r| r.get(0),
            )?;
            if substantive && s.bytes[0] < stop {
                ranges.push([s.bytes[0], stop]);
            }
            parent = s.parent;
        }
        ensure!(
            parent.is_none(),
            "Section ancestry exceeds reading probe; reindex required"
        );
        ranges.sort_unstable();
        Ok(ranges)
    }

    fn guidance_section(&self, id: i64) -> Result<Section> {
        Ok(self.db.query_row(
            "SELECT path,parent_id,anchor,start_byte,end_byte FROM sections WHERE id=?",
            [id],
            |r| {
                Ok(Section {
                    path: r.get(0)?,
                    parent: r.get(1)?,
                    anchor: r.get(2)?,
                    bytes: [r.get::<_, u32>(3)? as usize, r.get::<_, u32>(4)? as usize],
                })
            },
        )?)
    }

    fn omitted_blocks(
        &self,
        section: i64,
        bounds: [usize; 2],
        spans: &[[usize; 2]],
    ) -> Result<(Vec<[usize; 2]>, bool)> {
        let mut stmt = self.db.prepare("SELECT start_byte,end_byte FROM passages WHERE section_id=? AND kind<>'heading' AND start_byte>=? AND end_byte<=? ORDER BY start_byte,id LIMIT ?")?;
        let mut rows = stmt
            .query_map(
                params![
                    section,
                    bounds[0] as i64,
                    bounds[1] as i64,
                    (MAX_BLOCKS + 1) as i64
                ],
                |r| Ok([r.get::<_, u32>(0)? as usize, r.get::<_, u32>(1)? as usize]),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let limited = rows.len() > MAX_BLOCKS;
        rows.truncate(MAX_BLOCKS);
        rows.retain(|&range| !covered(range, spans));
        Ok((rows, limited))
    }
}
