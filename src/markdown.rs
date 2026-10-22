use anyhow::Result;
use pulldown_cmark::{Event, Options, Parser, Tag};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, ops::Range, sync::LazyLock};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub section: usize,
    pub block: usize,
    pub kind: String,
    pub view: String,
    pub partial: bool,
    pub context: Option<Range<usize>>,
    pub heading: String,
    pub text: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub embedding_input: String,
}
#[derive(Debug, Clone)]
pub struct Section {
    pub parent: Option<usize>,
    pub title: String,
    pub heading: String,
    pub anchor: String,
    pub start: usize,
    pub end: usize,
}
#[derive(Debug, Clone)]
pub struct Link {
    pub destination: String,
    pub line: usize,
}
pub struct Document {
    pub title: String,
    pub sections: Vec<Section>,
    pub chunks: Vec<Chunk>,
    pub links: Vec<Link>,
}
struct Block {
    range: Range<usize>,
    heading: Option<(usize, String, Option<String>)>,
    kind: &'static str,
}
pub fn line_at(source: &str, offset: usize) -> usize {
    source.as_bytes()[..offset]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
        + 1
}
/// Ordinary GitHub-style headings; renderer-specific rules beyond this are not guessed.
pub fn slug(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .filter_map(|c| {
            if c.is_alphanumeric() || c == '_' || c == '-' {
                Some(c)
            } else if c.is_whitespace() {
                Some('-')
            } else {
                None
            }
        })
        .collect()
}
pub fn parse(
    source: &str,
    fallback_title: &str,
    count: &dyn Fn(&str) -> Result<usize>,
) -> Result<Document> {
    let mut blocks = Vec::new();
    let mut links = Vec::new();
    let mut depth = 0;
    let mut start = 0;
    let mut kind = "prose";
    let mut heading = None;
    let mut item_starts = Vec::new();
    let mut items = Vec::new();
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
                if depth == 0 {
                    start = range.start;
                    heading = match &tag {
                        Tag::Heading { level, id, .. } => Some((
                            *level as usize,
                            String::new(),
                            id.as_ref().map(ToString::to_string),
                        )),
                        _ => None,
                    };
                    kind = match tag {
                        Tag::Heading { .. } => "heading",
                        Tag::CodeBlock(_) => "code",
                        Tag::Table(_) => "table",
                        Tag::List(_) => "list",
                        _ => "prose",
                    };
                    items.clear();
                }
                if matches!(tag, Tag::Item) {
                    item_starts.push((depth, range.start));
                }
                if let Tag::Link { dest_url, .. } = tag {
                    links.push(Link {
                        destination: dest_url.to_string(),
                        line: line_at(source, range.start),
                    });
                }
                depth += 1;
            }
            Event::End(end) => {
                depth -= 1;
                if matches!(end, pulldown_cmark::TagEnd::Item)
                    && let Some((d, s)) = item_starts.pop()
                    && d == 1
                {
                    items.push(s..range.end);
                }
                if depth == 0 {
                    if kind == "list" && !items.is_empty() {
                        for r in &items {
                            blocks.push(Block {
                                range: r.clone(),
                                heading: None,
                                kind: "list_item",
                            });
                        }
                    } else {
                        blocks.push(Block {
                            range: start..range.end,
                            heading: heading.take(),
                            kind,
                        });
                    }
                }
            }
            Event::Text(text) | Event::Code(text) => {
                if let Some((_, h, _)) = &mut heading {
                    h.push_str(&text);
                }
            }
            Event::Rule | Event::Html(_) if depth == 0 => blocks.push(Block {
                range,
                heading: None,
                kind: "prose",
            }),
            _ => {}
        }
    }
    let title = blocks
        .iter()
        .find_map(|b| {
            b.heading
                .as_ref()
                .filter(|(l, _, _)| *l == 1)
                .map(|(_, h, _)| h.clone())
        })
        .unwrap_or_else(|| fallback_title.into());
    let mut sections = vec![Section {
        parent: None,
        title: title.clone(),
        heading: title.clone(),
        anchor: String::new(),
        start: 0,
        end: source.len(),
    }];
    let mut ancestors: Vec<(usize, usize)> = Vec::new();
    let mut slugs = BTreeSet::new();
    let mut current = 0;
    let mut chunks = Vec::new();
    for (block_id, block) in blocks.into_iter().enumerate() {
        if let Some((level, name, explicit)) = block.heading {
            while ancestors.last().is_some_and(|(l, _)| *l >= level) {
                let (_, id) = ancestors.pop().unwrap();
                sections[id].end = block.range.start;
            }
            let parent = ancestors.last().map(|(_, id)| *id).unwrap_or(0);
            let breadcrumb = if parent == 0 {
                name.clone()
            } else {
                format!("{} > {name}", sections[parent].heading)
            };
            let base = explicit.unwrap_or_else(|| slug(&name));
            let mut anchor = base.clone();
            let mut suffix = 0;
            while slugs.contains(&anchor) {
                suffix += 1;
                anchor = format!("{base}-{suffix}");
            }
            slugs.insert(anchor.clone());
            current = sections.len();
            sections.push(Section {
                parent: Some(parent),
                title: name,
                heading: breadcrumb,
                anchor,
                start: block.range.start,
                end: source.len(),
            });
            ancestors.push((level, current));
        }
        let heading = &sections[current].heading;
        if block.kind == "table" {
            let mut lines = source[block.range.clone()].split_inclusive('\n');
            let header_len = lines.next().map_or(0, str::len) + lines.next().map_or(0, str::len);
            let header = block.range.start..block.range.start + header_len;
            let mut offset = header.end;
            let mut had_row = false;
            for line in lines {
                let range = offset..offset + line.len();
                offset = range.end;
                if !line.trim().is_empty() {
                    append(
                        source,
                        heading,
                        range,
                        count,
                        &mut chunks,
                        current,
                        block_id,
                        "table_row",
                        Some(header.clone()),
                        false,
                    )?;
                    had_row = true;
                }
            }
            if !had_row {
                append(
                    source,
                    heading,
                    block.range,
                    count,
                    &mut chunks,
                    current,
                    block_id,
                    "table",
                    None,
                    false,
                )?;
            }
        } else {
            append(
                source,
                heading,
                block.range,
                count,
                &mut chunks,
                current,
                block_id,
                block.kind,
                None,
                false,
            )?;
        }
    }
    Ok(Document {
        title,
        sections,
        chunks,
        links,
    })
}
#[allow(clippy::too_many_arguments)]
fn append(
    source: &str,
    heading: &str,
    mut range: Range<usize>,
    count: &dyn Fn(&str) -> Result<usize>,
    chunks: &mut Vec<Chunk>,
    section: usize,
    block: usize,
    kind: &str,
    context: Option<Range<usize>>,
    partial: bool,
) -> Result<()> {
    while range.start < range.end && source.as_bytes()[range.end - 1].is_ascii_whitespace() {
        range.end -= 1;
    }
    if range.is_empty() {
        return Ok(());
    }
    let mut prefix = heading.chars().take(160).collect::<String>();
    if let Some(ctx) = &context {
        prefix.push('\n');
        prefix.extend(source[ctx.clone()].chars().take(240));
    }
    while count(&prefix)? > 100 {
        prefix.pop();
    }
    let input = format!("{prefix}\n\n{}", &source[range.clone()]);
    if count(&input)? > 300 && source[range.clone()].chars().count() > 1 {
        let middle = range.start + range.len() / 2;
        let split = source[range.clone()]
            .char_indices()
            .filter(|(i, c)| *i > 0 && c.is_whitespace())
            .min_by_key(|(i, _)| (range.start + i).abs_diff(middle))
            .map(|(i, _)| range.start + i)
            .unwrap_or_else(|| {
                let mut point = middle;
                while !source.is_char_boundary(point) {
                    point += 1;
                }
                if point == range.end {
                    source[..point].char_indices().next_back().unwrap().0
                } else {
                    point
                }
            });
        append(
            source,
            heading,
            range.start..split,
            count,
            chunks,
            section,
            block,
            kind,
            context.clone(),
            true,
        )?;
        append(
            source,
            heading,
            split..range.end,
            count,
            chunks,
            section,
            block,
            kind,
            context,
            true,
        )?;
    } else {
        chunks.push(Chunk {
            section,
            block,
            kind: kind.into(),
            view: match kind {
                "heading" => "identity",
                "code" | "table_row" | "table" => "example",
                _ => "prose",
            }
            .into(),
            partial,
            context,
            heading: heading.into(),
            text: source[range.clone()].into(),
            start_byte: range.start,
            end_byte: range.end,
            start_line: line_at(source, range.start),
            end_line: line_at(source, range.end - 1),
            embedding_input: input,
        });
    }
    Ok(())
}
/// Shared identifiers require distinctive inline code, not ordinary prose.
pub fn identifiers(text: &str) -> BTreeSet<String> {
    static IDENTIFIER: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"^[A-Za-z_][A-Za-z0-9_.:/-]{2,95}$").unwrap());
    Parser::new(text)
        .filter_map(|event| match event {
            Event::Code(code) if IDENTIFIER.is_match(&code) => {
                let distinctive = code.contains('_')
                    || code.contains("::")
                    || code.contains('.')
                    || code.chars().skip(1).any(|c| c.is_uppercase());
                distinctive.then(|| code.to_string())
            }
            _ => None,
        })
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    fn count(s: &str) -> Result<usize> {
        Ok(s.chars().count().div_ceil(3) + 1)
    }
    #[test]
    fn hierarchy_anchors_and_source_blocks() {
        let source = "# Guide\r\n\r\nFirst paragraph.\r\n\r\n## Install\r\n\r\n```sh\r\n# not a heading\r\necho hi\r\n```\r\n\r\nRead [next](next.md#usage).\r\n\r\n## Install\r\n\n- First item\n- Second item\n";
        let doc = parse(source, "file", &count).unwrap();
        assert_eq!(doc.sections.len(), 4);
        assert_eq!(doc.sections[2].anchor, "install");
        assert_eq!(doc.sections[3].anchor, "install-1");
        assert_eq!(doc.sections[2].parent, Some(1));
        assert!(
            doc.chunks
                .iter()
                .any(|c| c.kind == "code" && c.text.ends_with("```"))
        );
        assert_eq!(
            doc.chunks.iter().filter(|c| c.kind == "list_item").count(),
            2
        );
        for c in &doc.chunks {
            assert_eq!(c.text, source[c.start_byte..c.end_byte]);
        }
        assert_eq!(doc.links[0].line, 12);
    }
    #[test]
    fn large_unicode_block_is_split_without_loss() {
        let source = format!("# Title\n\n{}", "界🙂é".repeat(900));
        let doc = parse(&source, "", &count).unwrap();
        for c in &doc.chunks {
            assert!(count(&c.embedding_input).unwrap() <= 300);
            assert_eq!(c.text, source[c.start_byte..c.end_byte]);
        }
        assert_eq!(
            doc.chunks
                .iter()
                .map(|c| c.text.matches('界').count())
                .sum::<usize>(),
            900
        );
    }
    #[test]
    fn table_rows_keep_original_header() {
        let source = "Title\n=====\n\n| Name | Meaning |\n| --- | --- |\n| retry | reuse `RequestKey` |\n| stop | terminate |\n";
        let doc = parse(source, "", &count).unwrap();
        assert_eq!(doc.title, "Title");
        let rows: Vec<_> = doc
            .chunks
            .iter()
            .filter(|c| c.kind == "table_row")
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            &source[rows[0].context.clone().unwrap()],
            "| Name | Meaning |\n| --- | --- |\n"
        );
        assert!(identifiers(&rows[0].text).contains("RequestKey"));
    }
}
