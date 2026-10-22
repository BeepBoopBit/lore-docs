//! Read contiguous source and section outlines without retrieval or model inference.
use crate::{
    digest,
    index::{Index, MAX_FILE_BYTES, PIPELINE},
    search::{SourceSlice, estimate_tokens, validate_scope},
};
use anyhow::{Context, Result, ensure};
use pulldown_cmark::{Event, Options, Parser};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use std::{collections::BTreeMap, fs::File, io::Read, ops::Range};

pub(crate) const TOKEN_METHOD: &str = "estimated_utf8_bytes_div_3_including_json_metadata";

#[derive(Debug, Clone)]
pub struct PageOptions {
    pub max_tokens: usize,
    pub cursor: Option<String>,
}
impl Default for PageOptions {
    fn default() -> Self {
        Self {
            max_tokens: 2000,
            cursor: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ReadSection {
    pub anchor: Option<String>,
    pub heading: String,
    pub bytes: [usize; 2],
    /// Exact target for reading the enclosing heading, or the file for a top-level heading.
    /// A structural navigation hint, not a claim that the parent answers the query.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_target: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ReadResult {
    pub root: String,
    pub path: String,
    pub source_hash: String,
    pub index_revision: u64,
    pub section: ReadSection,
    pub content: SourceSlice,
    pub starts_mid_block: bool,
    pub ends_mid_block: bool,
    pub has_more: bool,
    pub next_cursor: Option<String>,
    pub estimated_tokens: usize,
    pub token_count_method: &'static str,
}

#[derive(Debug, Serialize)]
pub struct OutlineEntry {
    pub anchor: String,
    pub title: String,
    /// Nesting in the indexed heading tree, not a renderer's numeric heading level.
    pub depth: usize,
    pub lines: [usize; 2],
}

#[derive(Debug, Serialize)]
pub struct OutlineResult {
    pub root: String,
    pub path: String,
    pub source_hash: String,
    pub index_revision: u64,
    pub title: String,
    pub sections: Vec<OutlineEntry>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
    pub estimated_tokens: usize,
    pub token_count_method: &'static str,
}

pub(crate) struct Source {
    pub(crate) root: String,
    pub(crate) path: String,
    pub(crate) hash: String,
    pub(crate) title: String,
    pub(crate) revision: u64,
    pub(crate) text: String,
    line_starts: Vec<usize>,
}
impl Source {
    pub(crate) fn lines(&self, start: usize, end: usize) -> [usize; 2] {
        let line = |offset| self.line_starts.partition_point(|&s| s <= offset);
        [line(start), line(end.saturating_sub(1).max(start))]
    }

    pub(crate) fn binding(&self, kind: &str, anchor: Option<&str>) -> Result<String> {
        Ok(digest(&serde_json::to_vec(&(
            kind,
            &self.root,
            &self.path,
            &self.hash,
            self.revision,
            anchor,
        ))?))
    }
}

impl Index {
    /// Return a contiguous page of the file or heading subtree. No ranking is applied.
    pub fn read(&self, target: &str, options: &PageOptions) -> Result<ReadResult> {
        validate(options)?;
        ensure!(target.len() <= 16 * 1024, "Read target exceeds 16 KiB");
        let (path, anchor) = target
            .split_once('#')
            .map_or((target, None), |(p, a)| (p, Some(a)));
        let tx = self.db.unchecked_transaction()?;
        let source = self.read_source(path)?;
        let (heading, start, end) = if let Some(anchor) = anchor {
            self.db.query_row(
                "SELECT heading,start_byte,end_byte FROM sections WHERE path=? AND anchor=? AND parent_id IS NOT NULL",
                params![path, anchor],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, u32>(1)? as usize, r.get::<_, u32>(2)? as usize)),
            ).optional()?.context("Heading anchor not indexed; use `lore outline PATH` to find exact anchors")?
        } else {
            (source.title.clone(), 0, source.text.len())
        };
        let parent_target = if let Some(anchor) = anchor {
            let (parent_anchor, parent_id): (String, Option<i64>) = self.db.query_row(
                "SELECT parent.anchor,parent.parent_id FROM sections child JOIN sections parent ON parent.id=child.parent_id WHERE child.path=? AND child.anchor=? AND parent.path=child.path",
                params![path, anchor], |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            Some(if parent_id.is_none() {
                path.to_owned()
            } else {
                format!("{path}#{parent_anchor}")
            })
        } else {
            None
        };
        ensure!(
            start <= end && source.text.get(start..end).is_some(),
            "Invalid indexed section bounds; reindex required"
        );
        let binding = source.binding("read", anchor)?;
        let offset = position(options.cursor.as_deref(), &binding)?.unwrap_or(start);
        ensure!(
            offset >= start
                && offset <= end
                && source.text.is_char_boundary(offset)
                && (options.cursor.is_none() || offset < end),
            "Invalid continuation position; restart this read"
        );
        let blocks = block_ranges(&source.text[start..end], start);
        let mid_block = |offset| blocks.iter().any(|r| r.start < offset && offset < r.end);
        let mut result = ReadResult {
            root: source.root.clone(),
            path: source.path.clone(),
            source_hash: source.hash.clone(),
            index_revision: source.revision,
            section: ReadSection {
                anchor: anchor.map(str::to_owned),
                heading,
                bytes: [start, end],
                parent_target,
            },
            content: SourceSlice {
                text: String::new(),
                lines: source.lines(offset, offset),
                bytes: [offset, offset],
            },
            starts_mid_block: mid_block(offset),
            ends_mid_block: false,
            has_more: false,
            next_cursor: None,
            // Reserve the maximum digit count until the final size is known.
            estimated_tokens: options.max_tokens,
            token_count_method: TOKEN_METHOD,
        };
        let set_end = |r: &mut ReadResult, stop| {
            r.content.text = source.text[offset..stop].to_owned();
            r.content.lines = source.lines(offset, stop);
            r.content.bytes[1] = stop;
            r.ends_mid_block = mid_block(stop);
            r.has_more = stop < end;
            r.next_cursor = r.has_more.then(|| cursor(&binding, stop));
        };
        set_end(&mut result, end);
        if cost(&result)? > options.max_tokens {
            // Partial-page envelopes include a continuation. The complete envelope was
            // checked separately because omitting that cursor makes its cost discontinuous.
            let boundaries: Vec<_> = source.text[offset..end]
                .char_indices()
                .map(|(i, _)| offset + i)
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
            let maximum = lo.checked_sub(1).map(|i| boundaries[i]).unwrap_or(offset);
            ensure!(
                maximum > offset,
                "max-tokens too small for metadata and source text; increase --max-tokens"
            );
            // Prefer intact top-level Markdown blocks (including lists and code fences).
            // If a single block is oversized, fall back to a line, then a UTF-8 boundary.
            let block_end = blocks
                .iter()
                .flat_map(|r| [r.start, r.end])
                .filter(|&point| {
                    point > offset
                        && point <= maximum
                        && !source.text[offset..point].trim().is_empty()
                })
                .max();
            let stop = block_end
                .or_else(|| {
                    source.text[offset..maximum]
                        .rfind('\n')
                        .map(|i| offset + i + 1)
                        .filter(|&point| !source.text[offset..point].trim().is_empty())
                })
                .unwrap_or(maximum);
            set_end(&mut result, stop);
        }
        // Starting with the maximum digit count makes this converge from above.
        loop {
            let measured = cost(&result)?;
            if result.estimated_tokens == measured {
                break;
            }
            result.estimated_tokens = measured;
        }
        ensure!(
            result.estimated_tokens <= options.max_tokens,
            "Internal read budget exceeded"
        );
        tx.commit()?;
        Ok(result)
    }

    /// List the indexed heading hierarchy in source order, without loading an encoder.
    pub fn outline(&self, path: &str, options: &PageOptions) -> Result<OutlineResult> {
        validate(options)?;
        ensure!(
            !path.contains('#'),
            "Outline takes a file path; use `lore read PATH#anchor` for a section"
        );
        let tx = self.db.unchecked_transaction()?;
        let source = self.read_source(path)?;
        let binding = source.binding("outline", None)?;
        let offset = position(options.cursor.as_deref(), &binding)?.unwrap_or(0);
        let mut stmt = self.db.prepare("SELECT id,parent_id,title,anchor,start_byte,end_byte FROM sections WHERE path=? ORDER BY start_byte,id")?;
        let mut depths = BTreeMap::new();
        let mut entries = Vec::new();
        for row in stmt.query_map([path], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, u32>(4)? as usize,
                r.get::<_, u32>(5)? as usize,
            ))
        })? {
            let (id, parent, title, anchor, start, end) = row?;
            let depth = parent
                .map(|p| depths.get(&p).copied().unwrap_or(0) + 1)
                .unwrap_or(0);
            depths.insert(id, depth);
            if parent.is_some() {
                entries.push(OutlineEntry {
                    anchor,
                    title,
                    depth,
                    lines: source.lines(start, end),
                });
            }
        }
        drop(stmt);
        ensure!(
            offset <= entries.len() && (options.cursor.is_none() || offset < entries.len()),
            "Invalid continuation position; restart this outline"
        );
        let total = entries.len();
        let mut result = OutlineResult {
            root: source.root,
            path: source.path,
            source_hash: source.hash,
            index_revision: source.revision,
            title: source.title,
            sections: Vec::new(),
            has_more: offset < total,
            next_cursor: (offset < total).then(|| cursor(&binding, offset)),
            estimated_tokens: options.max_tokens,
            token_count_method: TOKEN_METHOD,
        };
        ensure!(
            cost(&result)? <= options.max_tokens,
            "max-tokens too small for outline metadata; increase --max-tokens"
        );
        for (i, entry) in entries.into_iter().enumerate().skip(offset) {
            result.sections.push(entry);
            result.has_more = i + 1 < total;
            result.next_cursor = result.has_more.then(|| cursor(&binding, i + 1));
            if cost(&result)? > options.max_tokens {
                result.sections.pop();
                result.has_more = true;
                result.next_cursor = Some(cursor(&binding, i));
                ensure!(
                    !result.sections.is_empty(),
                    "max-tokens too small for the next heading; increase --max-tokens"
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
            result.estimated_tokens <= options.max_tokens,
            "Internal outline budget exceeded"
        );
        tx.commit()?;
        Ok(result)
    }

    pub(crate) fn read_source(&self, path: &str) -> Result<Source> {
        ensure!(
            !path.is_empty() && path.len() <= 16 * 1024,
            "Expected an indexed file path of at most 16 KiB"
        );
        validate_scope(path)?;
        let root = self.root()?;
        ensure!(
            self.meta("pipeline")?.as_deref() == Some(PIPELINE),
            "Index structure changed; run `lore index {}`",
            root.display()
        );
        let (hash, title): (String, String) = self
            .db
            .query_row(
                "SELECT hash,title FROM documents WHERE path=?",
                [path],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .context("Document not indexed; use a path relative to the corpus root")?;
        let full = root
            .join(path)
            .canonicalize()
            .with_context(|| format!("Source {path} is missing; reindex required"))?;
        ensure!(
            full.starts_with(&root),
            "Source {path} now resolves outside the corpus; reindex required"
        );
        let mut bytes = Vec::new();
        File::open(full)?
            .take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            digest(&bytes) == hash,
            "Source {path} changed since indexing; run `lore index {}` before reading",
            root.display()
        );
        let text = String::from_utf8(bytes)?;
        let mut line_starts = vec![0];
        line_starts.extend(
            text.bytes()
                .enumerate()
                .filter_map(|(i, b)| (b == b'\n').then_some(i + 1)),
        );
        Ok(Source {
            root: root.display().to_string(),
            path: path.into(),
            hash,
            title,
            revision: self.revision()?,
            text,
            line_starts,
        })
    }
}

pub(crate) fn validate(options: &PageOptions) -> Result<()> {
    ensure!(
        (128..=100_000).contains(&options.max_tokens),
        "max-tokens must be between 128 and 100000"
    );
    Ok(())
}

pub(crate) fn cost(value: &impl Serialize) -> Result<usize> {
    Ok(estimate_tokens(&serde_json::to_string(value)?))
}

// Integrity/binding checks, not an authorization token. No secrets or hidden state.
pub(crate) fn cursor(binding: &str, position: usize) -> String {
    format!(
        "v1:{position}:{}",
        digest(format!("{binding}:{position}").as_bytes())
    )
}

pub(crate) fn position(value: Option<&str>, binding: &str) -> Result<Option<usize>> {
    let Some(value) = value else {
        return Ok(None);
    };
    ensure!(value.len() <= 96, "Invalid continuation cursor");
    let parts: Vec<_> = value.split(':').collect();
    ensure!(
        parts.len() == 3 && parts[0] == "v1",
        "Invalid continuation cursor"
    );
    let offset = parts[1]
        .parse::<usize>()
        .context("Invalid continuation cursor")?;
    ensure!(
        cursor(binding, offset) == value,
        "Continuation is stale, invalid, or belongs to another target; restart without --cursor"
    );
    Ok(Some(offset))
}

pub(crate) fn block_ranges(text: &str, base: usize) -> Vec<Range<usize>> {
    let mut result = Vec::new();
    let (mut depth, mut start) = (0, 0);
    let parser = Parser::new_ext(
        text,
        Options::ENABLE_TABLES
            | Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_TASKLISTS
            | Options::ENABLE_HEADING_ATTRIBUTES,
    );
    for (event, range) in parser.into_offset_iter() {
        match event {
            Event::Start(_) => {
                if depth == 0 {
                    start = range.start;
                }
                depth += 1;
            }
            Event::End(_) => {
                depth -= 1;
                if depth == 0 {
                    result.push(base + start..base + range.end);
                }
            }
            Event::Rule | Event::Html(_) if depth == 0 => {
                result.push(base + range.start..base + range.end)
            }
            _ => {}
        }
    }
    result
}
