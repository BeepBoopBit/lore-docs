//! Bounded, source-verified context for numbered Markdown lists.
//! Structure is evidence of grouping, not proof of task prerequisites.
use crate::{index::Index, search::Brief};
use anyhow::Result;
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Range,
};

#[derive(Debug, Clone, Serialize)]
pub struct Group {
    pub path: String,
    pub section_id: i64,
    pub items: Vec<i64>,
    pub introduction: Option<i64>,
    pub restriction: Option<i64>,
}

impl Group {
    pub fn passage_ids(&self) -> impl Iterator<Item = i64> + '_ {
        self.introduction
            .into_iter()
            .chain(self.items.iter().copied())
            .chain(self.restriction)
    }
}

#[derive(Debug, Serialize)]
pub struct Coverage {
    /// This report does not assess whether the brief answers the query.
    pub scope: &'static str,
    pub groups: Vec<GroupCoverage>,
    pub limitations: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
pub struct GroupCoverage {
    pub path: String,
    pub source_hash: String,
    pub read_target: String,
    pub section_id: i64,
    pub total_steps: usize,
    pub visible_steps: usize,
    /// All recognized steps and adjoining context have visible source coverage.
    pub group_complete: bool,
    pub missing: Vec<MissingPart>,
}

#[derive(Debug, Serialize)]
pub struct MissingPart {
    pub passage_id: i64,
    pub role: &'static str,
    pub bytes: [usize; 2],
    pub lines: [usize; 2],
}

pub(crate) fn covered(range: [usize; 2], spans: &[[usize; 2]]) -> bool {
    let mut spans = spans.to_vec();
    spans.sort_unstable();
    let mut end = range[0];
    for [a, b] in spans {
        if a > end {
            break;
        }
        end = end.max(b);
        if end >= range[1] {
            return true;
        }
    }
    false
}

pub(crate) fn visible_spans(brief: &Brief) -> BTreeMap<&str, Vec<[usize; 2]>> {
    let mut visible = BTreeMap::<&str, Vec<[usize; 2]>>::new();
    for file in &brief.files {
        for passage in &file.passages {
            visible.entry(&file.path).or_default().push(passage.bytes);
            if let Some(context) = &passage.context {
                visible.entry(&file.path).or_default().push(context.bytes);
            }
        }
        for connection in &file.connections {
            for evidence in &connection.evidence {
                visible
                    .entry(&evidence.path)
                    .or_default()
                    .push(evidence.bytes);
            }
        }
    }
    visible
}

struct OrderedList {
    range: Range<usize>,
    items: Vec<Range<usize>>,
}

fn lists(source: &str) -> Vec<OrderedList> {
    let mut result = Vec::new();
    let (mut depth, mut item_start) = (0, 0);
    let mut current: Option<OrderedList> = None;
    for (event, range) in Parser::new_ext(
        source,
        Options::ENABLE_TABLES
            | Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_TASKLISTS
            | Options::ENABLE_HEADING_ATTRIBUTES,
    )
    .into_offset_iter()
    {
        match event {
            Event::Start(tag) => {
                if depth == 0 && matches!(tag, Tag::List(Some(_))) {
                    current = Some(OrderedList {
                        range: range.clone(),
                        items: Vec::new(),
                    });
                }
                if depth == 1 && matches!(tag, Tag::Item) {
                    item_start = range.start;
                }
                depth += 1;
            }
            Event::End(end) => {
                depth -= 1;
                if depth == 1
                    && end == TagEnd::Item
                    && let Some(list) = &mut current
                {
                    let mut stop = range.end;
                    while stop > item_start && source.as_bytes()[stop - 1].is_ascii_whitespace() {
                        stop -= 1;
                    }
                    list.items.push(item_start..stop);
                }
                if depth == 0
                    && let Some(mut list) = current.take()
                {
                    list.range.end = range.end;
                    result.push(list);
                }
            }
            _ => (),
        }
    }
    result
}

fn restriction(text: &str) -> bool {
    let plain = Parser::new(text)
        .filter_map(|e| match e {
            Event::Text(t) | Event::Code(t) => Some(t.to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ");
    let words: Vec<_> = plain
        .split(|c: char| !c.is_alphabetic())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect();
    words
        .iter()
        .any(|w| matches!(w.as_str(), "cannot" | "never"))
        || words.windows(2).any(|w| {
            matches!(
                (w[0].as_str(), w[1].as_str()),
                ("do" | "does" | "must", "not") | ("not", "a" | "an")
            )
        })
}

struct Row {
    id: i64,
    kind: String,
    partial: bool,
    bytes: Range<usize>,
    text: String,
}

impl Index {
    /// Inspect source already selected for this indexed brief; never alter selection.
    pub(crate) fn procedure_coverage(&self, brief: &Brief) -> Result<Coverage> {
        let ids = brief
            .files
            .iter()
            .flat_map(|f| &f.passages)
            .map(|p| p.id)
            .collect::<Vec<_>>();
        let visible = visible_spans(brief);
        let mut groups = Vec::new();
        for group in self.procedure_groups(&ids)? {
            let (anchor, parent, hash): (String, Option<i64>, String) = self.db.query_row(
                "SELECT s.anchor,s.parent_id,d.hash FROM sections s JOIN documents d ON d.path=s.path WHERE s.id=?",
                [group.section_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            let mut missing = Vec::new();
            let mut visible_steps = 0;
            for id in group.passage_ids() {
                let role = if Some(id) == group.introduction {
                    "introduction"
                } else if Some(id) == group.restriction {
                    "following_restriction"
                } else {
                    "step"
                };
                let (bytes, lines) = self.db.query_row(
                    "SELECT start_byte,end_byte,start_line,end_line FROM passages WHERE id=?",
                    [id],
                    |r| {
                        Ok((
                            [r.get::<_, u32>(0)? as usize, r.get::<_, u32>(1)? as usize],
                            [r.get::<_, u32>(2)? as usize, r.get::<_, u32>(3)? as usize],
                        ))
                    },
                )?;
                if covered(
                    bytes,
                    visible
                        .get(group.path.as_str())
                        .map(Vec::as_slice)
                        .unwrap_or(&[]),
                ) {
                    visible_steps += usize::from(role == "step");
                } else {
                    missing.push(MissingPart {
                        passage_id: id,
                        role,
                        bytes,
                        lines,
                    });
                }
            }
            let read_target = if parent.is_none() {
                group.path.clone()
            } else {
                format!("{}#{anchor}", group.path)
            };
            groups.push(GroupCoverage {
                path: group.path,
                source_hash: hash,
                read_target,
                section_id: group.section_id,
                total_steps: group.items.len(),
                visible_steps,
                group_complete: missing.is_empty(),
                missing,
            });
        }
        Ok(Coverage {
            scope: "recognized_numbered_lists_in_selected_passages",
            groups,
            limitations: vec![
                "Structural coverage only; numbered lists may describe alternatives rather than task dependencies. Missing context is not proof that the answer requires it.",
                "Bounded to 576 selected passage seeds, 64 sections, 128 direct rows per section, 512 KiB files, 2-12 complete list items, and 8192 source bytes per group. Partial, nested-only, and oversized lists are skipped.",
                "Only an immediately preceding prose block ending in a colon and an immediately following prose block with a recognized English restriction are checked. Sibling, ancestor, and cross-file prerequisites/caveats are not assessed. An empty report does not establish completeness.",
            ],
        })
    }

    /// Probe only sections reached by original passage seeds. No retrieval or models.
    /// Source hashes are checked for every inspected file, including unselected ones.
    pub fn procedure_groups(&self, seeds: &[i64]) -> Result<Vec<Group>> {
        let seed_set: BTreeSet<_> = seeds.iter().take(576).copied().collect();
        let mut sections = Vec::new();
        for id in seeds.iter().take(576) {
            let section: i64 =
                self.db
                    .query_row("SELECT section_id FROM passages WHERE id=?", [id], |r| {
                        r.get(0)
                    })?;
            if !sections.contains(&section) {
                sections.push(section);
                if sections.len() == 64 {
                    break;
                }
            }
        }
        let mut files = BTreeMap::new();
        let mut result = Vec::new();
        for section in sections {
            let mut stmt = self.db.prepare("SELECT id,kind,partial,start_byte,end_byte,text FROM passages WHERE section_id=? ORDER BY start_byte,id LIMIT 129")?;
            let rows = stmt
                .query_map([section], |r| {
                    Ok(Row {
                        id: r.get(0)?,
                        kind: r.get(1)?,
                        partial: r.get(2)?,
                        bytes: r.get::<_, u32>(3)? as usize..r.get::<_, u32>(4)? as usize,
                        text: r.get(5)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if rows.len() > 128
                || !rows
                    .iter()
                    .any(|r| seed_set.contains(&r.id) && r.kind == "list_item" && !r.partial)
            {
                continue;
            }
            let path: String =
                self.db
                    .query_row("SELECT path FROM sections WHERE id=?", [section], |r| {
                        r.get(0)
                    })?;
            if !files.contains_key(&path) {
                let source = self.read_source(&path)?.text;
                let found = if source.len() <= 512 * 1024 {
                    lists(&source)
                } else {
                    Vec::new()
                };
                files.insert(path.clone(), (source, found));
            }
            let (source, found) = &files[&path];
            for list in found {
                if !(2..=12).contains(&list.items.len()) {
                    continue;
                }
                let members = list
                    .items
                    .iter()
                    .map(|range| {
                        rows.iter().position(|r| {
                            !r.partial
                                && r.kind == "list_item"
                                && r.bytes == *range
                                && source.get(r.bytes.clone()) == Some(r.text.as_str())
                        })
                    })
                    .collect::<Option<Vec<_>>>();
                let Some(members) = members else { continue };
                if !members.iter().any(|&i| seed_set.contains(&rows[i].id)) {
                    continue;
                }
                let first = members[0];
                let last = *members.last().expect("nonempty list");
                let intro = first.checked_sub(1).filter(|&i| {
                    let r = &rows[i];
                    !r.partial
                        && r.kind == "prose"
                        && r.text.trim_end().ends_with(':')
                        && source
                            .get(r.bytes.end..list.range.start)
                            .is_some_and(|s| s.trim().is_empty())
                });
                let after = (last + 1 < rows.len()).then_some(last + 1).filter(|&i| {
                    let r = &rows[i];
                    !r.partial
                        && r.kind == "prose"
                        && restriction(&r.text)
                        && source
                            .get(list.range.end..r.bytes.start)
                            .is_some_and(|s| s.trim().is_empty())
                });
                let bytes: usize = members
                    .iter()
                    .copied()
                    .chain(intro)
                    .chain(after)
                    .map(|i| rows[i].text.len())
                    .sum();
                if bytes > 8192 {
                    continue;
                }
                result.push(Group {
                    path: path.clone(),
                    section_id: section,
                    items: members.iter().map(|&i| rows[i].id).collect(),
                    introduction: intro.map(|i| rows[i].id),
                    restriction: after.map(|i| rows[i].id),
                });
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::covered;

    #[test]
    fn coverage_requires_every_source_byte_even_when_evidence_is_fragmented() {
        assert!(covered([10, 40], &[[30, 40], [0, 20], [15, 30]]));
        assert!(covered([10, 40], &[[10, 20], [20, 40]]));
        assert!(!covered([10, 40], &[[10, 20], [21, 40]]));
        assert!(!covered([10, 40], &[[0, 9], [11, 50]]));
        assert!(!covered([10, 40], &[[10, 39]]));
        assert!(!covered([10, 40], &[]));
    }
}
