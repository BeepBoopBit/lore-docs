//! An independent, cached passage-content representation. Discovery never reads it.
use crate::{digest, embedding::Embedder, vector_blob};
use anyhow::{Result, ensure};
use rusqlite::{OptionalExtension, Transaction, params};
use std::collections::BTreeMap;

#[derive(Clone, Debug, serde::Serialize)]
pub struct PassageScore {
    pub passage_id: i64,
    pub contextual_quality: f64,
    pub content_quality: f64,
    pub selected_quality: f64,
    pub content_similarity: Option<f32>,
    pub content_bm25: f64,
}

pub(crate) struct Signal {
    pub similarity: Option<f32>,
    pub bm25: f64,
    pub quality: f64,
}

impl crate::index::Index {
    pub(crate) fn content_scores(
        &self,
        ids: &[i64],
        query: &str,
        vector: Option<&[f32]>,
        options: &crate::search::BriefOptions,
    ) -> Result<BTreeMap<i64, Signal>> {
        use crate::search::{Mode, query_terms};
        let terms = query_terms(query);
        let mut lexical = BTreeMap::new();
        if options.mode != Mode::Semantic && !terms.is_empty() {
            let expr = terms
                .iter()
                .map(|t| format!("\"{t}\""))
                .collect::<Vec<_>>()
                .join(" OR ");
            let mut stmt = self.db.prepare("SELECT c.rowid,-bm25(content_fts) FROM content_fts c JOIN passages p ON p.id=c.rowid WHERE content_fts MATCH ? AND (?='' OR p.path=? OR substr(p.path,1,length(?)+1)=?||'/')")?;
            for row in stmt.query_map(
                params![
                    expr,
                    options.scope,
                    options.scope,
                    options.scope,
                    options.scope
                ],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?)),
            )? {
                let (id, score) = row?;
                lexical.insert(id, score);
            }
        }
        let maximum = ids
            .iter()
            .filter_map(|id| lexical.get(id))
            .copied()
            .fold(0.0f64, f64::max)
            .max(f64::EPSILON);
        let mut out = BTreeMap::new();
        let mut stmt=self.db.prepare("SELECT e.vector FROM passage_content c JOIN embeddings e ON e.key=c.embedding_key WHERE c.passage_id=?")?;
        for &id in ids {
            let bm25 = lexical.get(&id).copied().unwrap_or(0.0);
            let similarity = if let Some(q) = vector {
                let blob = stmt.query_row([id], |r| r.get::<_, Vec<u8>>(0))?;
                Some(crate::cosine(q, &crate::vector_from_blob(&blob)))
            } else {
                None
            };
            let dense = similarity.unwrap_or(0.0).max(0.0) as f64;
            let quality = match options.mode {
                Mode::Keyword => bm25 / maximum,
                Mode::Semantic => dense,
                Mode::Hybrid => 0.75 * dense + 0.25 * bm25 / maximum,
            };
            out.insert(
                id,
                Signal {
                    similarity,
                    bm25,
                    quality,
                },
            );
        }
        Ok(out)
    }
}

pub(crate) const PIPELINE: &str = "content-v1/body/optional-table-header-240chars-100tokens";

#[derive(Default)]
pub(crate) struct ContentReport {
    pub computed: usize,
    pub reused: usize,
    pub updated: bool,
}

fn input(text: &str, context: Option<&str>, model: &dyn Embedder) -> Result<String> {
    let mut header = context
        .unwrap_or_default()
        .chars()
        .take(240)
        .collect::<String>();
    while !header.is_empty() && model.token_count(&header)? > 100 {
        header.pop();
    }
    let value = if header.is_empty() {
        text.to_owned()
    } else {
        format!("{header}\n\n{text}")
    };
    ensure!(
        model.token_count(&value)? <= 512,
        "Content passage exceeds embedding limit"
    );
    Ok(value)
}

/// Backfill/add content rows inside the same transaction as discovery indexing.
/// Passage IDs and the original discovery vectors are never updated here.
pub(crate) fn index(tx: &Transaction<'_>, model: &mut dyn Embedder) -> Result<ContentReport> {
    let old: Option<String> = tx
        .query_row(
            "SELECT value FROM metadata WHERE key='content_pipeline'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    let reset = old.as_deref() != Some(PIPELINE);
    if reset {
        tx.execute("DELETE FROM passage_content", [])?;
    }
    let existing: usize = tx.query_row("SELECT count(*) FROM passage_content", [], |r| {
        r.get::<_, u32>(0)
    })? as usize;
    let mut pending = Vec::new();
    let mut missing = BTreeMap::new();
    let mut reused = existing;
    let mut stmt = tx.prepare("SELECT p.id,p.text,p.context FROM passages p LEFT JOIN passage_content c ON c.passage_id=p.id WHERE c.passage_id IS NULL ORDER BY p.id")?;
    for row in stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })? {
        let (id, text, context) = row?;
        let context: Option<crate::search::SourceSlice> =
            context.map(|s| serde_json::from_str(&s)).transpose()?;
        let value = input(&text, context.as_ref().map(|c| c.text.as_str()), model)?;
        let key = digest(format!("{}\0{PIPELINE}\0{value}", model.identity()).as_bytes());
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM embeddings WHERE key=?)",
            [&key],
            |r| r.get(0),
        )?;
        if exists {
            reused += 1;
        } else {
            missing.insert(key.clone(), value.clone());
        }
        pending.push((id, key, value));
    }
    drop(stmt);
    let missing: Vec<_> = missing.into_iter().collect();
    for (batch_number, batch) in missing.chunks(16).enumerate() {
        let texts: Vec<_> = batch.iter().map(|(_, v)| v.clone()).collect();
        let vectors = model.embed_documents(&texts)?;
        ensure!(
            vectors.len() == texts.len(),
            "Content embedding count mismatch"
        );
        for ((key, _), vector) in batch.iter().zip(vectors) {
            ensure!(
                vector.len() == 384 && vector.iter().all(|v| v.is_finite()),
                "Invalid content embedding vector"
            );
            tx.execute(
                "INSERT INTO embeddings(key,vector) VALUES(?,?)",
                params![key, vector_blob(&vector)],
            )?;
        }
        if batch_number % 8 == 0 || (batch_number + 1) * 16 >= missing.len() {
            eprintln!(
                "Embedded {}/{} content passages",
                ((batch_number + 1) * 16).min(missing.len()),
                missing.len()
            );
        }
    }
    for (id, key, value) in &pending {
        tx.execute(
            "INSERT INTO passage_content(passage_id,embedding_key) VALUES(?,?)",
            params![id, key],
        )?;
        tx.execute(
            "INSERT INTO content_fts(rowid,text) VALUES(?,?)",
            params![id, value],
        )?;
    }
    tx.execute("INSERT INTO metadata(key,value) VALUES('content_pipeline',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[PIPELINE])?;
    Ok(ContentReport {
        computed: missing.len(),
        reused,
        updated: reset || !pending.is_empty(),
    })
}
