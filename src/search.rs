pub use crate::leads::Alternative;
use crate::preferences::{Qualifier, QualifierMode};
use crate::{
    embedding::Embedder,
    index::{Index, PIPELINE},
    reranking::{RerankMode, RerankSession, Reranker},
    retrieval::Candidate,
    vector_blob, vector_from_blob,
};
use anyhow::{Context, Result, ensure};
use clap::ValueEnum;
use rmcp::schemars;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashSet},
    time::Instant,
};

#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, ValueEnum, PartialEq, Eq, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Keyword,
    Semantic,
    Hybrid,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Ranking {
    Document,
    Section,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Selection {
    Ranked,
    Evidence,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PassageScoring {
    Contextual,
    Content,
    Blended,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleRule {
    pub prefix: String,
    pub role: String,
    pub weight: f64,
}
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryConfig {
    #[serde(default)]
    pub roles: Vec<RoleRule>,
}
#[derive(Debug, Clone)]
pub struct BriefOptions {
    pub mode: Mode,
    pub max_files: usize,
    pub max_tokens: usize,
    pub candidates: usize,
    pub min_similarity: f32,
    pub connections: bool,
    /// Keep recognized API details with their declaration in default evidence selection.
    pub api_context: bool,
    pub scope: String,
    pub ranking: Ranking,
    pub selection: Selection,
    pub passage_scoring: PassageScoring,
    pub graph: bool,
    pub roles: Vec<RoleRule>,
    pub qualifiers: QualifierMode,
    pub prefer: Vec<Qualifier>,
    pub alternatives: usize,
    pub recover_sections: bool,
    pub rerank: RerankMode,
}
impl Default for BriefOptions {
    fn default() -> Self {
        Self {
            mode: Mode::Hybrid,
            max_files: 5,
            max_tokens: 2000,
            candidates: 40,
            min_similarity: 0.5,
            connections: true,
            api_context: true,
            scope: String::new(),
            ranking: Ranking::Section,
            selection: Selection::Evidence,
            passage_scoring: PassageScoring::Contextual,
            graph: false,
            roles: Vec::new(),
            qualifiers: QualifierMode::Off,
            prefer: Vec::new(),
            alternatives: 0,
            recover_sections: false,
            rerank: RerankMode::Off,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceSlice {
    pub text: String,
    pub lines: [usize; 2],
    pub bytes: [usize; 2],
}
#[derive(Debug, Clone, Serialize)]
pub struct Passage {
    pub id: i64,
    pub section_id: i64,
    pub anchor: String,
    pub heading: String,
    pub kind: String,
    pub lines: [usize; 2],
    pub bytes: [usize; 2],
    pub text: String,
    pub matched_by: Vec<String>,
    pub semantic_similarity: Option<f32>,
    pub excerpted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<SourceSlice>,
}
#[derive(Debug, Clone, Serialize)]
pub struct FileMatch {
    pub rank: usize,
    pub path: String,
    pub title: String,
    pub source_hash: String,
    pub score: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub passages: Vec<Passage>,
    pub connections: Vec<Connection>,
}
#[derive(Debug, Clone, Serialize)]
pub struct Evidence {
    pub path: String,
    pub source_hash: String,
    pub lines: [usize; 2],
    pub bytes: [usize; 2],
    pub text: String,
}
#[derive(Debug, Clone, Serialize)]
pub struct Connection {
    pub path: String,
    pub kind: String,
    pub direction: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identifier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub similarity: Option<f32>,
    pub evidence: Vec<Evidence>,
}
#[derive(Debug, Clone, Serialize)]
pub struct Brief {
    pub query: String,
    pub mode: Mode,
    pub root: String,
    pub index_revision: u64,
    pub files: Vec<FileMatch>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub qualifiers: Vec<Qualifier>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub alternative_sections: Vec<Alternative>,
    pub status: String,
    pub truncated: bool,
    pub estimated_tokens: usize,
    pub token_count_method: String,
    pub elapsed_ms: f64,
}
#[derive(Debug, Serialize)]
pub struct Related {
    pub path: String,
    pub root: String,
    pub index_revision: u64,
    pub connections: Vec<Connection>,
}
#[derive(Debug, Serialize)]
pub struct Diagnostics {
    pub brief: Brief,
    pub api_context: crate::api_context::Activation,
    pub procedure_coverage: crate::procedure::Coverage,
    pub ranking: Ranking,
    pub selection: Selection,
    pub passage_scoring: PassageScoring,
    pub qualifier_mode: QualifierMode,
    pub alternative_limit: usize,
    pub recover_sections: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reranking: Option<crate::reranking::RerankReport>,
    pub selection_trace: Vec<crate::selection::SelectionDecision>,
    pub scope: String,
    pub graph: bool,
    pub retrieval_ms: f64,
    pub assembly_ms: f64,
    pub candidates: Vec<Candidate>,
    pub notes: Vec<String>,
}
#[derive(Clone)]
pub(crate) struct StoredPassage {
    pub path: String,
    pub title: String,
    pub hash: String,
    pub passage: Passage,
    pub vector: Vec<f32>,
}
/// Conservative byte estimate including the complete compact JSON response.
pub fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(3)
}
pub fn text_terms(text: &str) -> HashSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .collect()
}
pub fn query_terms(query: &str) -> Vec<String> {
    const STOP: &[&str] = &[
        "a", "an", "and", "are", "as", "at", "be", "by", "can", "do", "does", "for", "from", "how",
        "i", "if", "in", "is", "it", "me", "my", "of", "on", "or", "our", "the", "this", "to",
        "what", "when", "where", "which", "who", "why", "with", "would", "you",
    ];
    let mut seen = HashSet::new();
    query
        .split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|s| !s.is_empty() && !STOP.contains(&s.as_str()) && seen.insert(s.clone()))
        .take(32)
        .collect()
}
pub fn in_scope(path: &str, scope: &str) -> bool {
    scope.is_empty()
        || path == scope
        || path.strip_prefix(scope).is_some_and(|p| p.starts_with('/'))
}
pub fn validate_scope(scope: &str) -> Result<()> {
    ensure!(
        !scope.starts_with('/')
            && !scope.contains('\\')
            && !scope.contains(':')
            && !scope.split('/').any(|s| s == ".." || s == "."),
        "scope/prefix must be a literal repository-relative file or directory"
    );
    Ok(())
}
impl Index {
    pub fn brief(
        &self,
        query: &str,
        embedder: Option<&mut dyn Embedder>,
        options: &BriefOptions,
    ) -> Result<Brief> {
        Ok(self.diagnose(query, embedder, options)?.brief)
    }
    pub fn diagnose(
        &self,
        query: &str,
        embedder: Option<&mut dyn Embedder>,
        options: &BriefOptions,
    ) -> Result<Diagnostics> {
        self.diagnose_with_reranker(query, embedder, None, options)
    }
    pub fn brief_with_reranker(
        &self,
        query: &str,
        embedder: Option<&mut dyn Embedder>,
        reranker: Option<&mut dyn Reranker>,
        options: &BriefOptions,
    ) -> Result<Brief> {
        Ok(self
            .diagnose_with_reranker(query, embedder, reranker, options)?
            .brief)
    }
    pub fn diagnose_with_reranker(
        &self,
        query: &str,
        embedder: Option<&mut dyn Embedder>,
        reranker: Option<&mut dyn Reranker>,
        options: &BriefOptions,
    ) -> Result<Diagnostics> {
        ensure!(
            options.rerank == RerankMode::Off || options.selection == Selection::Evidence,
            "Reranking requires --selection evidence"
        );
        ensure!(
            options.rerank == RerankMode::Off
                || options.passage_scoring == PassageScoring::Contextual,
            "Reranking currently requires --passage-scoring contextual"
        );
        let mut reranking = RerankSession::new(reranker);
        ensure!(!query.trim().is_empty(), "Query cannot be empty");
        ensure!(query.len() <= 16 * 1024, "Query exceeds 16 KiB");
        ensure!(
            (1..=100).contains(&options.max_files),
            "max-files must be between 1 and 100"
        );
        ensure!(
            (128..=100_000).contains(&options.max_tokens),
            "max-tokens must be between 128 and 100000"
        );
        ensure!(
            (1..=1000).contains(&options.candidates),
            "candidates must be between 1 and 1000"
        );
        ensure!(
            options.min_similarity.is_finite() && (0.0..=1.0).contains(&options.min_similarity),
            "min-similarity must be between 0 and 1"
        );
        validate_scope(&options.scope)?;
        ensure!(
            options.alternatives <= 8,
            "alternatives must be between 0 and 8"
        );
        ensure!(
            options.prefer.len() <= 7,
            "At most seven qualifier preferences are supported"
        );
        for r in &options.roles {
            validate_scope(&r.prefix)?;
            ensure!(
                r.weight.is_finite()
                    && (0.1..=2.0).contains(&r.weight)
                    && !r.role.is_empty()
                    && r.role.len() <= 64,
                "role requires a name of 1..64 bytes and weight between 0.1 and 2"
            );
        }
        let mut normalized_options = options.clone();
        normalized_options.scope = options.scope.trim_end_matches('/').into();
        for rule in &mut normalized_options.roles {
            rule.prefix = rule.prefix.trim_end_matches('/').into();
        }
        let options = &normalized_options;
        let started = Instant::now();
        let tx = self.db.unchecked_transaction()?;
        let root = self.root()?;
        if options.passage_scoring != PassageScoring::Contextual {
            ensure!(
                options.selection == Selection::Evidence,
                "Content scoring requires --selection evidence"
            );
            ensure!(
                self.content_ready()?,
                "Content representation missing or outdated; run `lore index {} --content`",
                root.display()
            );
        }
        ensure!(
            self.meta("pipeline")?.as_deref() == Some(PIPELINE),
            "Index structure changed; run `lore index {}`",
            root.display()
        );
        let mut brief = Brief {
            query: query.into(),
            mode: options.mode,
            root: root.display().to_string(),
            index_revision: self.revision()?,
            files: Vec::new(),
            qualifiers: Vec::new(),
            alternative_sections: Vec::new(),
            status: "no_matches".into(),
            truncated: false,
            estimated_tokens: 0,
            token_count_method: "estimated_utf8_bytes_div_3_including_json_metadata".into(),
            elapsed_ms: 0.0,
        };
        ensure!(
            estimate_tokens(&serde_json::to_string(&brief)?) + 32 <= options.max_tokens,
            "Query and result metadata exceed max-tokens; increase the budget or shorten the query"
        );
        let (mut candidates, vector) = self.retrieve(query, embedder, options)?;
        if !candidates.iter().any(|c| c.exact) {
            brief.qualifiers =
                crate::preferences::requested(query, options.qualifiers, &options.prefer);
        }
        ensure!(
            estimate_tokens(&serde_json::to_string(&brief)?) + 32 <= options.max_tokens,
            "Query and qualifier metadata exceed max-tokens; increase the budget or shorten the query"
        );
        let retrieval_ms = started.elapsed().as_secs_f64() * 1000.0;
        let original_candidates = (options.alternatives > 0).then(|| candidates.clone());
        let mut selection_trace = self.assemble(
            &mut brief,
            &mut candidates,
            query,
            vector.as_deref(),
            options,
            &mut reranking,
        )?;
        if let Some(original) = original_candidates {
            selection_trace = self.add_alternatives(
                &mut brief,
                &mut candidates,
                original,
                query,
                vector.as_deref(),
                options,
                selection_trace,
                &mut reranking,
            )?;
        }
        brief.status = if brief.files.is_empty() {
            if candidates.is_empty() {
                "no_matches"
            } else if !brief.alternative_sections.is_empty() {
                "navigation_only"
            } else {
                "budget_too_small"
            }
        } else {
            "matches"
        }
        .into();
        let procedure_coverage = self.procedure_coverage(&brief)?;
        brief.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        brief.estimated_tokens = estimate_tokens(&serde_json::to_string(&brief)?) + 8;
        ensure!(
            estimate_tokens(&serde_json::to_string(&brief)?) <= options.max_tokens,
            "Internal response budget exceeded"
        );
        let assembly_ms = brief.elapsed_ms - retrieval_ms;
        tx.commit()?;
        Ok(Diagnostics{procedure_coverage,api_context:crate::api_context::activation(options,candidates.iter().any(|c| c.exact)),recover_sections:options.recover_sections,reranking:reranking.report(options.rerank),qualifier_mode:options.qualifiers,alternative_limit:options.alternatives,brief,ranking:options.ranking,selection:options.selection,passage_scoring:options.passage_scoring,selection_trace,scope:options.scope.clone(),graph:options.graph,retrieval_ms,assembly_ms,candidates,notes:vec!["Ranks and similarity are relevance signals, not answerability probabilities.".into(),"Candidate rows are sections; each view keeps up to three contributing blocks. Evidence selection can also include structurally adjacent context, recorded in selection_trace.".into(),"Diagnostics are not constrained by the brief token budget. Candidate spans refer to the index snapshot; only sources supporting returned brief results are checked for freshness. Newly added files require indexing.".into()]})
    }
    pub(crate) fn passage(&self, id: i64) -> Result<StoredPassage> {
        let (mut value,ctx):(_,Option<String>)=self.db.query_row("SELECT p.path,d.title,d.hash,p.heading,p.text,p.start_byte,p.end_byte,p.start_line,p.end_line,e.vector,p.section_id,s.anchor,p.kind,p.partial,p.context FROM passages p JOIN documents d ON d.path=p.path JOIN sections s ON s.id=p.section_id JOIN embeddings e ON e.key=p.embedding_key WHERE p.id=?",[id],|r|Ok((StoredPassage{path:r.get(0)?,title:r.get(1)?,hash:r.get(2)?,passage:Passage{id,heading:r.get(3)?,text:r.get(4)?,bytes:[r.get::<_,u32>(5)? as usize,r.get::<_,u32>(6)? as usize],lines:[r.get::<_,u32>(7)? as usize,r.get::<_,u32>(8)? as usize],matched_by:Vec::new(),semantic_similarity:None,excerpted:r.get(13)?,section_id:r.get(10)?,anchor:r.get(11)?,kind:r.get(12)?,context:None},vector:vector_from_blob(&r.get::<_,Vec<u8>>(9)?)},r.get(14)?)))?;
        value.passage.context = ctx.map(|s| serde_json::from_str(&s)).transpose()?;
        Ok(value)
    }
    pub fn related(&self, path: &str, limit: usize) -> Result<Related> {
        ensure!(
            (1..=100).contains(&limit),
            "limit must be between 1 and 100"
        );
        let tx = self.db.unchecked_transaction()?;
        ensure!(
            self.meta("pipeline")?.as_deref() == Some(PIPELINE),
            "Index structure changed; run lore index"
        );
        let (path, anchor) = path
            .split_once('#')
            .map_or((path, None), |(p, a)| (p, Some(a)));
        let hash: String = self
            .db
            .query_row("SELECT hash FROM documents WHERE path=?", [path], |r| {
                r.get(0)
            })
            .optional()?
            .context("Document not indexed; use a path relative to the corpus root")?;
        self.verify_source(path, &hash)?;
        let section_ids = if let Some(anchor) = anchor {
            let mut stmt=self.db.prepare("SELECT p.id FROM passages p JOIN sections s ON s.id=p.section_id WHERE p.path=? AND s.anchor=? ORDER BY p.start_byte")?;
            let ids = stmt
                .query_map(params![path, anchor], |r| r.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            ensure!(!ids.is_empty(), "Heading anchor not indexed");
            Some(ids)
        } else {
            None
        };
        let connections = self.connections(path, section_ids.as_deref(), limit)?;
        for connection in &connections {
            self.verify_connection(connection)?;
        }
        let result = Related {
            path: path.into(),
            root: self.root()?.display().to_string(),
            index_revision: self.revision()?,
            connections,
        };
        tx.commit()?;
        Ok(result)
    }

    pub(crate) fn evidence(&self, id: i64, needle: Option<&str>) -> Result<Evidence> {
        let c = self.passage(id)?;
        if needle.is_none() {
            return Ok(Evidence {
                path: c.path,
                source_hash: c.hash,
                lines: c.passage.lines,
                bytes: c.passage.bytes,
                text: c.passage.text,
            });
        }
        let offset = needle.and_then(|n| c.passage.text.find(n)).unwrap_or(0);
        let local_start = c.passage.text[..offset]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let local_end = c.passage.text[offset..]
            .find('\n')
            .map(|i| offset + i)
            .unwrap_or(c.passage.text.len());
        let line = c.passage.lines[0]
            + c.passage.text[..local_start]
                .bytes()
                .filter(|&b| b == b'\n')
                .count();
        Ok(Evidence {
            path: c.path,
            source_hash: c.hash,
            lines: [line, line],
            bytes: [
                c.passage.bytes[0] + local_start,
                c.passage.bytes[0] + local_end,
            ],
            text: c.passage.text[local_start..local_end].to_owned(),
        })
    }

    fn link_evidence(&self, id: i64, line: usize) -> Result<Evidence> {
        let c = self.passage(id)?;
        let source = std::fs::read_to_string(self.root()?.join(&c.path))?;
        ensure!(
            crate::digest(source.as_bytes()) == c.hash,
            "Source {} changed since indexing; reindex required",
            c.path
        );
        let start = source
            .split_inclusive('\n')
            .take(line - 1)
            .map(str::len)
            .sum::<usize>();
        let end = source[start..]
            .find('\n')
            .map_or(source.len(), |i| start + i);
        Ok(Evidence {
            path: c.path,
            source_hash: c.hash,
            lines: [line, line],
            bytes: [start, end],
            text: source[start..end].into(),
        })
    }

    pub(crate) fn verify_connection(&self, connection: &Connection) -> Result<()> {
        for e in &connection.evidence {
            self.verify_source(&e.path, &e.source_hash)?;
        }
        let hash: String = self.db.query_row(
            "SELECT hash FROM documents WHERE path=?",
            [&connection.path],
            |r| r.get(0),
        )?;
        self.verify_source(&connection.path, &hash)
    }

    pub(crate) fn connections(
        &self,
        path: &str,
        passage_ids: Option<&[i64]>,
        limit: usize,
    ) -> Result<Vec<Connection>> {
        let mut output = Vec::new();
        let selected_sections = passage_ids
            .map(|ids| {
                ids.iter()
                    .map(|id| {
                        self.db
                            .query_row("SELECT section_id FROM passages WHERE id=?", [id], |r| {
                                r.get::<_, i64>(0)
                            })
                    })
                    .collect::<rusqlite::Result<BTreeSet<_>>>()
            })
            .transpose()?;
        let mut seen = BTreeSet::new();
        let mut statement = self.db.prepare("SELECT l.source,l.target,l.line,l.destination,d.hash FROM links l JOIN documents d ON d.path=l.source JOIN documents t ON t.path=l.target WHERE (l.source=? OR l.target=?) ORDER BY l.source,l.target,l.line LIMIT 100")?;
        for row in statement.query_map([path, path], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, u32>(2)? as usize,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })? {
            let (source, target, line, destination, _hash) = row?;
            let incoming = target == path && source != path;
            let other = if incoming { source.clone() } else { target };

            let passage_id:Option<i64>=self.db.query_row("SELECT id FROM passages WHERE path=? AND start_line<=? AND end_line>=? ORDER BY start_byte LIMIT 1",params![source,line as i64,line as i64],|r|r.get(0)).optional()?;
            let Some(passage_id) = passage_id else {
                continue;
            };
            let anchor = destination.split_once('#').and_then(|(_, s)| {
                percent_encoding::percent_decode_str(s)
                    .decode_utf8()
                    .ok()
                    .map(|s| s.into_owned())
            });
            if let Some(sections) = &selected_sections {
                if !incoming {
                    let section: i64 = self.db.query_row(
                        "SELECT section_id FROM passages WHERE id=?",
                        [passage_id],
                        |r| r.get(0),
                    )?;
                    if !sections.contains(&section) {
                        continue;
                    }
                } else if let Some(anchor) = &anchor {
                    let target_section: Option<i64> = self
                        .db
                        .query_row(
                            "SELECT id FROM sections WHERE path=? AND anchor=?",
                            params![path, anchor],
                            |r| r.get(0),
                        )
                        .optional()?;
                    if target_section.is_none_or(|id| !sections.contains(&id)) {
                        continue;
                    }
                }
            }
            if !seen.insert((
                other.clone(),
                format!("references#{}", anchor.as_deref().unwrap_or("")),
            )) {
                continue;
            }
            output.push(Connection {
                path: other,
                kind: "references".into(),
                direction: if incoming { "incoming" } else { "outgoing" }.into(),
                anchor: if incoming {
                    None
                } else {
                    destination.split_once('#').and_then(|(_, s)| {
                        percent_encoding::percent_decode_str(s)
                            .decode_utf8()
                            .ok()
                            .map(|s| s.into_owned())
                    })
                },
                identifier: None,
                similarity: None,
                evidence: vec![self.link_evidence(passage_id, line)?],
            });
        }
        let mut statement=self.db.prepare("SELECT a.name,a.passage_id,b.path,b.passage_id FROM identifiers a JOIN identifiers b ON a.name=b.name AND a.path!=b.path WHERE a.path=? AND (SELECT count(DISTINCT path) FROM identifiers WHERE name=a.name)<=max(3,(SELECT count(*)/5 FROM documents)) ORDER BY a.name,b.path,a.passage_id,b.passage_id LIMIT 200")?;
        for row in statement.query_map([path], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })? {
            let (name, source, other, target) = row?;
            if let Some(sections) = &selected_sections {
                let section: i64 = self.db.query_row(
                    "SELECT section_id FROM passages WHERE id=?",
                    [source],
                    |r| r.get(0),
                )?;
                if !sections.contains(&section) {
                    continue;
                }
            }
            if !seen.insert((other.clone(), "shared_identifier".to_owned())) {
                continue;
            }
            output.push(Connection {
                path: other,
                kind: "shared_identifier".into(),
                direction: "undirected".into(),
                anchor: None,
                identifier: Some(name.clone()),
                similarity: None,
                evidence: vec![
                    self.evidence(source, Some(&name))?,
                    self.evidence(target, Some(&name))?,
                ],
            });
        }
        let ids: Vec<i64> = match passage_ids {
            Some(ids) => ids.to_vec(),
            None => self
                .db
                .prepare("SELECT id FROM passages WHERE path=? ORDER BY start_byte LIMIT 8")?
                .query_map([path], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?,
        };
        let mut semantic = Vec::new();
        for id in ids {
            let source = self.passage(id)?;
            let mut statement=self.db.prepare("SELECT v.rowid,v.distance,p.path FROM passage_vectors v JOIN passages p ON p.id=v.rowid WHERE v.embedding MATCH ? AND k=16 ORDER BY v.distance")?;
            for row in statement.query_map([vector_blob(&source.vector)], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, f32>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })? {
                let (target, distance, other) = row?;
                if other == path || 1.0 - distance < 0.75 {
                    continue;
                }
                semantic.push((1.0 - distance, id, target, other));
            }
        }
        semantic.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.3.cmp(&b.3)));
        for (similarity, source, target, other) in semantic {
            if !seen.insert((other.clone(), "semantically_related".to_owned())) {
                continue;
            }
            output.push(Connection {
                path: other,
                kind: "semantically_related".into(),
                direction: "undirected".into(),
                anchor: None,
                identifier: None,
                similarity: Some(similarity),
                evidence: vec![self.evidence(source, None)?, self.evidence(target, None)?],
            });
        }
        // Round-robin across relation types so a link-heavy README cannot hide other evidence.
        let mut result = Vec::new();
        while !output.is_empty() && result.len() < limit {
            for kind in ["references", "shared_identifier", "semantically_related"] {
                if result.len() == limit {
                    break;
                }
                if let Some(position) = output.iter().position(|c| c.kind == kind) {
                    result.push(output.remove(position));
                }
            }
        }
        Ok(result)
    }
}
