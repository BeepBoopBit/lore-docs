//! Conservative source ownership for API declarations and locally documented details.
use crate::{
    index::Index,
    preferences::QualifierMode,
    reranking::RerankMode,
    search::{BriefOptions, Mode, PassageScoring, Ranking, Selection},
};
use anyhow::Result;
use pulldown_cmark::{Event, Parser};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Dependency {
    pub dependent: i64,
    pub declaration: i64,
    pub function: String,
    pub reason: String,
}

#[derive(Clone, Debug)]
struct Signature {
    function: String,
    parameters: BTreeSet<String>,
}
#[derive(Clone)]
struct Declaration {
    id: i64,
    start: usize,
    signature: Signature,
}
#[derive(Clone)]
struct Section {
    path: String,
    parent: Option<i64>,
    heading_parameters: BTreeSet<String>,
    declaration: Option<Declaration>,
    barrier: bool,
    bounded: bool,
}
struct Seed {
    id: i64,
    path: String,
    section: i64,
    start: usize,
    eligible: bool,
    text: String,
    context: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Activation {
    pub requested: bool,
    pub enabled: bool,
    /// Stable option names (or `exact_lookup`) that prevent activation.
    pub disabled_by: Vec<&'static str>,
}

pub fn activation(o: &BriefOptions, exact: bool) -> Activation {
    let disabled_by = [
        (!o.api_context, "api_context"),
        (exact, "exact_lookup"),
        (o.mode != Mode::Hybrid, "mode"),
        (o.ranking != Ranking::Section, "ranking"),
        (o.selection != Selection::Evidence, "selection"),
        (
            o.passage_scoring != PassageScoring::Contextual,
            "passage_scoring",
        ),
        (o.graph, "graph"),
        (o.qualifiers != QualifierMode::Off, "qualifiers"),
        (o.alternatives != 0, "alternatives"),
        (o.recover_sections, "recover_sections"),
        (o.rerank != RerankMode::Off, "rerank"),
    ]
    .into_iter()
    .filter_map(|(disabled, name)| disabled.then_some(name))
    .collect::<Vec<_>>();
    Activation {
        requested: o.api_context,
        enabled: disabled_by.is_empty(),
        disabled_by,
    }
}

pub(crate) fn enabled(options: &BriefOptions, exact: bool) -> bool {
    activation(options, exact).enabled
}
fn identifier(s: &str) -> bool {
    let mut cs = s.chars();
    cs.next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
}
fn arguments(s: &str) -> Option<Vec<&str>> {
    let mut result = Vec::new();
    let mut stack = Vec::new();
    let mut quote = None;
    let mut escape = false;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        if let Some(q) = quote {
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' => quote = Some(c),
            '(' => stack.push(')'),
            '[' => stack.push(']'),
            '{' => stack.push('}'),
            ')' | ']' | '}' => {
                if stack.pop() != Some(c) {
                    return None;
                }
            }
            ',' if stack.is_empty() => {
                result.push(s[start..i].trim());
                start = i + 1;
            }
            _ => (),
        }
    }
    if quote.is_some() || !stack.is_empty() {
        return None;
    }
    result.push(s[start..].trim());
    Some(result)
}
fn signature(text: &str) -> Option<(Signature, bool)> {
    let rest = text.strip_prefix('`')?;
    if rest.starts_with('`') {
        return None;
    }
    let (code, tail) = rest.split_once('`')?;
    if code.len() > 2048 {
        return None;
    }
    let code = code.trim().strip_prefix("await ").unwrap_or(code.trim());
    let (function, args) = code.split_once('(')?;
    let function = function.trim();
    if !function.replace("::", ".").split('.').all(identifier) {
        return None;
    }
    let args = args.strip_suffix(')')?;
    let mut parameters = BTreeSet::new();
    for arg in arguments(args)? {
        if matches!(arg, "" | "*" | "/") {
            continue;
        }
        let name = arg.split(['=', ':']).next()?.trim().trim_start_matches('*');
        if !identifier(name) || !parameters.insert(name.to_string()) {
            return None;
        }
    }
    if parameters.len() > 32 {
        return None;
    }
    let verb = tail.split_whitespace().next().unwrap_or("");
    let declaration = matches!(
        verb,
        "writes"
            | "sends"
            | "returns"
            | "reads"
            | "starts"
            | "stops"
            | "creates"
            | "removes"
            | "requests"
            | "takes"
            | "sets"
            | "opens"
            | "has"
    );
    Some((
        Signature {
            function: function.into(),
            parameters,
        },
        declaration,
    ))
}
fn heading_parameters(text: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for event in Parser::new(text) {
        if let Event::Code(code) = event {
            let left = code.split('=').next().unwrap_or("");
            for name in left.split(',').map(str::trim) {
                if !identifier(name) {
                    return BTreeSet::new();
                }
                names.insert(name.into());
            }
        }
    }
    names
}

fn references(text: &str, context: Option<&str>, signature: &Signature) -> bool {
    for event in Parser::new(text) {
        if let Event::Code(code) = event {
            let name = code.split(['(', '=']).next().unwrap_or("").trim();
            if name == signature.function || signature.parameters.contains(name) {
                return true;
            }
        }
    }
    let header = context
        .and_then(|c| serde_json::from_str::<serde_json::Value>(c).ok())
        .and_then(|c| c.get("text").and_then(|v| v.as_str()).map(str::to_string));
    header.is_some_and(|h| {
        h.split(|c: char| !c.is_alphanumeric() && c != '_')
            .any(|word| {
                signature
                    .parameters
                    .iter()
                    .any(|p| p.eq_ignore_ascii_case(word))
            })
    })
}

impl Index {
    /// Read-only, bounded source-structure probe, also used by the isolated selector.
    pub fn api_dependencies(&self, ids: &[i64]) -> Result<Vec<Dependency>> {
        let mut cache = BTreeMap::new();
        let mut result = Vec::new();
        for &id in ids.iter().take(576) {
            let seed = self.db.query_row(
                "SELECT path,section_id,start_byte,kind,partial,text,context FROM passages WHERE id=?",
                [id],
                |r| {
                    Ok(Seed {
                        id,
                        path: r.get(0)?,
                        section: r.get(1)?,
                        start: r.get::<_, u32>(2)? as usize,
                        eligible: r.get::<_, String>(3)? != "heading" && !r.get::<_, bool>(4)?,
                        text: r.get(5)?, context: r.get(6)?,
                    })
                },
            )?;
            if !seed.eligible {
                continue;
            }
            let mut section_id = seed.section;
            let mut parameters = BTreeSet::new();
            let mut seen = BTreeSet::new();
            for depth in 0..8 {
                if !seen.insert(section_id) {
                    break;
                }
                if let std::collections::btree_map::Entry::Vacant(e) = cache.entry(section_id) {
                    e.insert(self.api_section(section_id)?);
                }
                let section = &cache[&section_id];
                if section.path != seed.path || !section.bounded {
                    break;
                }
                if depth == 0 {
                    parameters = section.heading_parameters.clone();
                }
                if section.barrier {
                    if let Some(d) = &section.declaration
                        && seed.id != d.id
                        && d.start < seed.start
                        && ((depth == 0
                            && references(&seed.text, seed.context.as_deref(), &d.signature))
                            || (depth > 0
                                && (!parameters.is_empty()
                                    && parameters.is_subset(&d.signature.parameters))))
                    {
                        result.push(Dependency {
                            dependent: seed.id,
                            declaration: d.id,
                            function: d.signature.function.clone(),
                            reason: if depth == 0 {
                                "same_section"
                            } else {
                                "ancestor_parameter"
                            }
                            .into(),
                        });
                    }
                    break;
                }
                if parameters.is_empty() {
                    break;
                }
                let Some(parent) = section.parent else {
                    break;
                };
                section_id = parent;
            }
        }
        Ok(result)
    }
    fn api_section(&self, id: i64) -> Result<Section> {
        let (path, parent) = self.db.query_row(
            "SELECT path,parent_id FROM sections WHERE id=?",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let mut stmt = self.db.prepare("SELECT id,kind,text,start_byte,partial FROM passages WHERE section_id=? ORDER BY start_byte LIMIT 129")?;
        let rows = stmt
            .query_map([id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, u32>(3)? as usize,
                    r.get::<_, bool>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut found = Vec::new();
        let mut heading = BTreeSet::new();
        let bounded = rows.len() <= 128;
        for (id, kind, text, start, partial) in rows {
            if kind == "heading" {
                heading = heading_parameters(&text);
            }
            if kind == "prose"
                && !partial
                && let Some((signature, accepted)) = signature(&text)
            {
                found.push((
                    Declaration {
                        id,
                        start,
                        signature,
                    },
                    accepted,
                ));
            }
        }
        let barrier = !found.is_empty();
        let declaration = if found.len() == 1 && found[0].1 {
            Some(found.remove(0).0)
        } else {
            None
        };
        Ok(Section {
            path,
            parent,
            heading_parameters: heading,
            declaration,
            barrier,
            bounded,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Index {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE sections(id INTEGER PRIMARY KEY,path TEXT,parent_id INTEGER); CREATE TABLE passages(id INTEGER PRIMARY KEY,path TEXT,section_id INTEGER,start_byte INTEGER,kind TEXT,partial INTEGER,text TEXT);
        INSERT INTO sections VALUES(1,'api.md',NULL),(2,'api.md',1),(3,'api.md',1),(4,'api.md',2),(5,'other.md',1);
        INSERT INTO passages VALUES
        (1,'api.md',1,0,'heading',0,'# Deliver'),
        (2,'api.md',1,20,'prose',0,'`deliver(text, submit=True)` writes one sequence.'),
        (3,'api.md',2,100,'heading',0,'## Option: `submit=False`'),
        (4,'api.md',2,150,'prose',0,'Leaves text unsubmitted.'),
        (5,'api.md',3,200,'heading',0,'## Option: `timeout=10`'),
        (6,'api.md',3,250,'prose',0,'Another option.'),
        (7,'api.md',4,300,'heading',0,'### Send a key'),
        (8,'api.md',4,350,'prose',0,'`press(key)` sends a key.'),
        (9,'api.md',4,400,'table_row',0,'| Simple | enter |'),
        (10,'other.md',5,500,'heading',0,'## Option: `submit=False`'),
        (11,'other.md',5,550,'prose',0,'Outside the source file.'); ALTER TABLE passages ADD COLUMN context TEXT; UPDATE passages SET context='{\"text\":\"| Key form | Value |\"}' WHERE id=9;").unwrap();
        Index { db }
    }
    #[test]
    fn source_ownership_respects_parameters_new_methods_and_files() {
        let index = fixture();
        let links = index.api_dependencies(&[2, 4, 6, 8, 9, 11]).unwrap();
        assert_eq!(
            links
                .iter()
                .map(|d| (d.dependent, d.declaration))
                .collect::<Vec<_>>(),
            vec![(4, 2), (9, 8)]
        );
        assert_eq!(links[0].reason, "ancestor_parameter");
        assert_eq!(links[1].function, "press");
    }
    #[test]
    fn ambiguous_declarations_and_partial_blocks_do_not_bind() {
        let index = fixture();
        index.db.execute("INSERT INTO passages VALUES(12,'api.md',1,50,'prose',0,'`other(submit=True)` writes something else.',NULL)",[]).unwrap();
        index
            .db
            .execute("UPDATE passages SET partial=1 WHERE id=9", [])
            .unwrap();
        assert!(index.api_dependencies(&[4, 9]).unwrap().is_empty());
    }
    #[test]
    fn an_illustrative_call_blocks_inheritance_and_forward_ownership() {
        let index = fixture();
        index.db.execute("UPDATE passages SET text='`press(key)` in this example is illustrative.' WHERE id=8",[]).unwrap();
        index
            .db
            .execute("UPDATE passages SET start_byte=10 WHERE id=4", [])
            .unwrap();
        assert!(index.api_dependencies(&[4, 9]).unwrap().is_empty());
    }
    #[test]
    fn oversized_sections_are_not_partially_interpreted() {
        let index = fixture();
        for i in 20..150 {
            index
                .db
                .execute(
                    "INSERT INTO passages VALUES(?,'api.md',1,?,'prose',0,'More text.',NULL)",
                    rusqlite::params![i, i * 10],
                )
                .unwrap();
        }
        assert!(index.api_dependencies(&[4]).unwrap().is_empty());
    }
    #[test]
    fn shared_section_needs_an_explicit_reference() {
        let (signature, _) = signature("`press(key)` sends a key.").unwrap();
        assert!(!references(
            "Nearby listener shutdown is different.",
            None,
            &signature
        ));
        assert!(!references(
            "[Crate documentation](README.md)",
            None,
            &signature
        ));
        assert!(references("Pass `key` as a string.", None, &signature));
        assert!(references(
            "`press` has no revision guard.",
            None,
            &signature
        ));
        assert!(!references(
            "The `keyboard` setting is different.",
            None,
            &signature
        ));
    }
    #[test]
    fn parses_named_parameters_and_nested_defaults() {
        let (s, ok) =
            signature("`client.write(text, options={'x': [1, 2]}, *, submit=True)` writes bytes.")
                .unwrap();
        assert!(ok);
        assert_eq!(s.function, "client.write");
        assert_eq!(
            s.parameters,
            ["text", "options", "submit"].map(String::from).into()
        );
        assert!(signature("`f(value='a,\\\'b', count=2)` returns a value.").is_some());
        assert!(
            signature("`await client.wait(session, **conditions)` returns state.")
                .unwrap()
                .1
        );
    }
    #[test]
    fn rejects_ambiguous_or_literal_call_syntax() {
        for text in [
            "`f('literal')` returns data.",
            "`f(x=[1,2)` returns data.",
            "`f(x,x)` returns data.",
            "```f(x)``` returns data.",
            "Example: `f(x)` returns data.",
        ] {
            assert!(signature(text).is_none(), "{text}");
        }
        assert!(
            !signature("`f(value)` in this example is illustrative.")
                .unwrap()
                .1
        );
    }
    #[test]
    fn only_explicit_heading_identifiers_are_parameters() {
        assert_eq!(
            heading_parameters("### Option: `submit=False`"),
            ["submit".to_string()].into()
        );
        assert_eq!(
            heading_parameters("### Size: `columns, rows`"),
            ["columns", "rows"].map(String::from).into()
        );
        assert!(heading_parameters("### Send one key").is_empty());
        assert!(heading_parameters("### Example: `send(x)`").is_empty());
    }
    #[test]
    fn nondefault_scoring_and_exact_queries_bypass() {
        let o = BriefOptions::default();
        assert!(enabled(&o, false));
        assert!(!enabled(&o, true));
        assert!(!enabled(
            &BriefOptions {
                api_context: false,
                ..o.clone()
            },
            false
        ));
        let mut o = o;
        o.rerank = RerankMode::Local;
        assert!(!enabled(&o, false));
        o.rerank = RerankMode::Off;
        o.mode = Mode::Keyword;
        assert!(!enabled(&o, false));
    }
}
