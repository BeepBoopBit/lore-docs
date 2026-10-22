//! Source-backed reading plans: ancestor introductions followed by the requested subtree.
use crate::{
    index::Index,
    reading::{
        PageOptions, ReadSection, Source, TOKEN_METHOD, block_ranges, cost, cursor, position,
        validate,
    },
    search::SourceSlice,
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextRole {
    Introduction,
    Section,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanStep {
    pub order: usize,
    /// Target for the enclosing section; bytes/lines identify only this planned slice.
    pub target: String,
    pub role: ContextRole,
    pub heading: String,
    pub bytes: [usize; 2],
    pub lines: [usize; 2],
}

#[derive(Debug, Serialize)]
pub struct ReadingPlan {
    pub target: String,
    pub root: String,
    pub path: String,
    pub source_hash: String,
    pub index_revision: u64,
    pub focus: ReadSection,
    pub steps: Vec<PlanStep>,
    pub total_steps: usize,
    pub has_more: bool,
    pub next_cursor: Option<String>,
    pub estimated_tokens: usize,
    pub token_count_method: &'static str,
}

#[derive(Debug, Serialize)]
pub struct ContextPart {
    pub step: usize,
    pub target: String,
    pub role: ContextRole,
    pub content: SourceSlice,
    pub starts_mid_block: bool,
    pub ends_mid_block: bool,
}

#[derive(Debug, Serialize)]
pub struct ContextRead {
    pub target: String,
    pub root: String,
    pub path: String,
    pub source_hash: String,
    pub index_revision: u64,
    pub focus: ReadSection,
    pub parts: Vec<ContextPart>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
    /// Shows whether a continuation still precedes the requested section.
    pub next_role: Option<ContextRole>,
    pub estimated_tokens: usize,
    pub token_count_method: &'static str,
}

struct Section {
    parent: Option<i64>,
    heading: String,
    anchor: String,
    start: usize,
    end: usize,
}

struct Document {
    source: Source,
    focus: ReadSection,
    steps: Vec<PlanStep>,
}

impl Index {
    /// Plan structural context for an exact target. No query inference or model is used.
    pub fn plan(&self, target: &str, options: &PageOptions) -> Result<ReadingPlan> {
        validate(options)?;
        let tx = self.db.unchecked_transaction()?;
        let doc = self.context_document(target)?;
        let binding = doc
            .source
            .binding("context_plan/v1", doc.focus.anchor.as_deref())?;
        let offset = position(options.cursor.as_deref(), &binding)?.unwrap_or(0);
        ensure!(
            offset < doc.steps.len(),
            "Invalid plan continuation; restart without --cursor"
        );
        let mut result = ReadingPlan {
            target: target.into(),
            root: doc.source.root,
            path: doc.source.path,
            source_hash: doc.source.hash,
            index_revision: doc.source.revision,
            focus: doc.focus,
            total_steps: doc.steps.len(),
            steps: Vec::new(),
            has_more: false,
            next_cursor: None,
            estimated_tokens: options.max_tokens,
            token_count_method: TOKEN_METHOD,
        };
        for (i, step) in doc.steps.into_iter().enumerate().skip(offset) {
            result.steps.push(step);
            result.has_more = i + 1 < result.total_steps;
            result.next_cursor = result.has_more.then(|| cursor(&binding, i + 1));
            if cost(&result)? > options.max_tokens {
                result.steps.pop();
                result.has_more = true;
                result.next_cursor = Some(cursor(&binding, i));
                ensure!(
                    !result.steps.is_empty(),
                    "max-tokens too small for plan metadata and the next step; increase --max-tokens"
                );
                break;
            }
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
            "Internal plan budget exceeded"
        );
        tx.commit()?;
        Ok(result)
    }

    /// Read every planned introduction and the full focus subtree, in source order.
    /// Sibling sections are excluded. Separate slices never imply contiguous source.
    pub fn read_context(&self, target: &str, options: &PageOptions) -> Result<ContextRead> {
        validate(options)?;
        let tx = self.db.unchecked_transaction()?;
        let doc = self.context_document(target)?;
        let source = &doc.source;
        let binding = source.binding("context_read/v1", doc.focus.anchor.as_deref())?;
        let continuation = position(options.cursor.as_deref(), &binding)?;
        let offset = continuation.unwrap_or(doc.steps[0].bytes[0]);
        let start_step = doc
            .steps
            .iter()
            .position(|s| s.bytes[0] <= offset && offset < s.bytes[1]);
        let empty = source.text.is_empty() && continuation.is_none();
        ensure!(
            empty || (start_step.is_some() && source.text.is_char_boundary(offset)),
            "Invalid contextual continuation; restart without --cursor"
        );
        let mut result = ContextRead {
            target: target.into(),
            root: source.root.clone(),
            path: source.path.clone(),
            source_hash: source.hash.clone(),
            index_revision: source.revision,
            focus: doc.focus.clone(),
            parts: Vec::new(),
            has_more: false,
            next_cursor: None,
            next_role: None,
            estimated_tokens: options.max_tokens,
            token_count_method: TOKEN_METHOD,
        };
        for (i, step) in doc
            .steps
            .iter()
            .enumerate()
            .skip(start_step.unwrap_or(doc.steps.len()))
        {
            let start = step.bytes[0].max(offset);
            let end = step.bytes[1];
            let blocks = block_ranges(&source.text[step.bytes[0]..end], step.bytes[0]);
            let mid_block = |at| blocks.iter().any(|r| r.start < at && at < r.end);
            result.parts.push(ContextPart {
                step: step.order,
                target: step.target.clone(),
                role: step.role,
                content: SourceSlice {
                    text: String::new(),
                    bytes: [start, start],
                    lines: source.lines(start, start),
                },
                starts_mid_block: mid_block(start),
                ends_mid_block: false,
            });
            let set_end = |r: &mut ContextRead, stop| {
                let part = r.parts.last_mut().expect("current part");
                part.content.text = source.text[start..stop].into();
                part.content.bytes[1] = stop;
                part.content.lines = source.lines(start, stop);
                part.ends_mid_block = mid_block(stop);
                let next = if stop < end {
                    Some((stop, step.role))
                } else {
                    doc.steps.get(i + 1).map(|s| (s.bytes[0], s.role))
                };
                r.has_more = next.is_some();
                r.next_cursor = next.map(|(at, _)| cursor(&binding, at));
                r.next_role = next.map(|(_, role)| role);
            };
            set_end(&mut result, end);
            if cost(&result)? <= options.max_tokens {
                continue;
            }

            // A complete step has a different continuation envelope. Check it first,
            // then find a fitting UTF-8 prefix and prefer complete Markdown blocks.
            let boundaries: Vec<_> = source.text[start..end]
                .char_indices()
                .map(|(n, _)| start + n)
                .collect();
            let (mut lo, mut hi) = (0, boundaries.len());
            while lo < hi {
                let m = (lo + hi) / 2;
                set_end(&mut result, boundaries[m]);
                if cost(&result)? <= options.max_tokens {
                    lo = m + 1;
                } else {
                    hi = m;
                }
            }
            let maximum = lo.checked_sub(1).map(|n| boundaries[n]).unwrap_or(start);
            if maximum == start {
                set_end(&mut result, start);
                result.parts.pop();
                ensure!(
                    !result.parts.is_empty(),
                    "max-tokens too small for contextual metadata and source text; increase --max-tokens"
                );
                break;
            }
            let stop = blocks
                .iter()
                .flat_map(|r| [r.start, r.end])
                .filter(|&at| {
                    at > start && at <= maximum && !source.text[start..at].trim().is_empty()
                })
                .max()
                .or_else(|| {
                    source.text[start..maximum]
                        .rfind('\n')
                        .map(|n| start + n + 1)
                        .filter(|&at| !source.text[start..at].trim().is_empty())
                })
                .unwrap_or(maximum);
            set_end(&mut result, stop);
            break;
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
            "max-tokens too small for contextual metadata; increase --max-tokens"
        );
        tx.commit()?;
        Ok(result)
    }

    fn context_document(&self, target: &str) -> Result<Document> {
        ensure!(target.len() <= 16 * 1024, "Context target exceeds 16 KiB");
        let (path, anchor) = target
            .split_once('#')
            .map_or((target, None), |(p, a)| (p, Some(a)));
        let source = self.read_source(path)?;
        let mut stmt = self.db.prepare("SELECT id,parent_id,heading,anchor,start_byte,end_byte FROM sections WHERE path=? ORDER BY start_byte,id")?;
        let sections = stmt
            .query_map([path], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    Section {
                        parent: r.get(1)?,
                        heading: r.get(2)?,
                        anchor: r.get(3)?,
                        start: r.get::<_, u32>(4)? as usize,
                        end: r.get::<_, u32>(5)? as usize,
                    },
                ))
            })?
            .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
        let focus_id = sections
            .iter()
            .find(|(_, s)| match anchor {
                Some(a) => s.parent.is_some() && s.anchor == a,
                None => s.parent.is_none(),
            })
            .map(|(&id, _)| id)
            .context("Heading anchor not indexed; use `lore outline PATH` to find exact anchors")?;
        let target_for = |s: &Section| {
            if s.parent.is_none() {
                path.to_owned()
            } else {
                format!("{path}#{}", s.anchor)
            }
        };
        let focus_section = &sections[&focus_id];
        let focus = ReadSection {
            anchor: anchor.map(str::to_owned),
            heading: focus_section.heading.clone(),
            bytes: [focus_section.start, focus_section.end],
            parent_target: focus_section
                .parent
                .map(|id| {
                    sections
                        .get(&id)
                        .map(&target_for)
                        .context("Invalid section ancestry; reindex required")
                })
                .transpose()?,
        };
        let mut children = BTreeMap::<i64, usize>::new();
        for s in sections.values() {
            if let Some(parent) = s.parent {
                children
                    .entry(parent)
                    .and_modify(|at| *at = (*at).min(s.start))
                    .or_insert(s.start);
            }
        }
        let substantive = self
            .db
            .prepare("SELECT DISTINCT section_id FROM passages WHERE path=? AND kind<>'heading'")?
            .query_map([path], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<BTreeSet<_>>>()?;
        let mut ancestors = Vec::new();
        let mut parent = focus_section.parent;
        let mut seen = BTreeSet::from([focus_id]);
        while let Some(id) = parent {
            ensure!(seen.insert(id), "Cyclic section ancestry; reindex required");
            let s = sections
                .get(&id)
                .context("Invalid section ancestry; reindex required")?;
            if substantive.contains(&id) {
                ancestors.push(id);
            }
            parent = s.parent;
        }
        ancestors.reverse();
        let mut steps = Vec::new();
        for id in ancestors.into_iter().chain(std::iter::once(focus_id)) {
            let s = &sections[&id];
            let is_focus = id == focus_id;
            let end = if is_focus {
                s.end
            } else {
                children.get(&id).copied().unwrap_or(s.end)
            };
            ensure!(
                s.start <= end && source.text.get(s.start..end).is_some(),
                "Invalid section bounds; reindex required"
            );
            if !is_focus && source.text[s.start..end].trim().is_empty() {
                continue;
            }
            if let Some(previous) = steps.last() {
                let previous: &PlanStep = previous;
                ensure!(
                    previous.bytes[1] <= s.start,
                    "Overlapping context steps; reindex required"
                );
            }
            steps.push(PlanStep {
                order: steps.len() + 1,
                target: target_for(s),
                role: if is_focus {
                    ContextRole::Section
                } else {
                    ContextRole::Introduction
                },
                heading: s.heading.clone(),
                bytes: [s.start, end],
                lines: source.lines(s.start, end),
            });
        }
        Ok(Document {
            source,
            focus,
            steps,
        })
    }
}
