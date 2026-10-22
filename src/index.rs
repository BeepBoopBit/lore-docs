use crate::{digest, embedding::Embedder, markdown, vector_blob};
use anyhow::{Context, Result, bail, ensure};
use ignore::WalkBuilder;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Component, Path, PathBuf},
    sync::Once,
    time::{Duration, Instant},
};

pub(crate) const PIPELINE: &str = "markdown-v2/sections/blocks-300/separate-views/header-context";
pub(crate) const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

pub struct Index {
    pub(crate) db: Connection,
}

#[derive(Debug, Serialize)]
pub struct IndexReport {
    pub root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relocated_from: Option<String>,
    pub revision: u64,
    pub documents: usize,
    pub passages: usize,
    pub changed: usize,
    pub removed: usize,
    pub embeddings_computed: usize,
    pub embeddings_reused: usize,
    pub content_embeddings_computed: usize,
    pub content_embeddings_reused: usize,
    pub elapsed_ms: f64,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Status {
    pub root: Option<String>,
    pub root_available: bool,
    pub revision: u64,
    pub documents: usize,
    pub passages: usize,
    pub model: Option<String>,
    pub sections: usize,
    pub needs_reindex: bool,
    pub content_passages: usize,
    pub needs_content_index: bool,
    /// None means source freshness has not been checked, even when the root exists.
    pub source_check: Option<SourceCheck>,
}

#[derive(Debug, Default, Serialize)]
pub struct SourceCheck {
    pub checked_documents: usize,
    pub added: Vec<String>,
    pub changed: Vec<String>,
    pub removed: Vec<String>,
    pub errors: Vec<String>,
}

impl SourceCheck {
    fn needs_reindex(&self) -> bool {
        !self.added.is_empty()
            || !self.changed.is_empty()
            || !self.removed.is_empty()
            || !self.errors.is_empty()
    }
}

impl Index {
    pub fn open(directory: &Path) -> Result<Self> {
        static REGISTER: Once = Once::new();
        REGISTER.call_once(|| {
            // sqlite-vec documents this extension entry-point cast for rusqlite.
            // Registration is process-wide, once, before opening any connections.
            unsafe {
                rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<
                    unsafe extern "C" fn(),
                    unsafe extern "C" fn(
                        *mut rusqlite::ffi::sqlite3,
                        *mut *mut std::ffi::c_char,
                        *const rusqlite::ffi::sqlite3_api_routines,
                    ) -> std::ffi::c_int,
                >(
                    sqlite_vec::sqlite3_vec_init
                )));
            }
        });
        fs::create_dir_all(directory)?;
        let db = Connection::open(directory.join("index.sqlite"))?;
        db.busy_timeout(Duration::from_secs(10))?;
        db.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL;")?;
        let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        ensure!(
            version <= 3,
            "Index was created by a newer Lore; use a different --index-dir"
        );
        if version == 0 {
            db.execute_batch("
          CREATE TABLE IF NOT EXISTS metadata(key TEXT PRIMARY KEY, value TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS documents(path TEXT PRIMARY KEY, hash TEXT NOT NULL, title TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS embeddings(key TEXT PRIMARY KEY, vector BLOB NOT NULL);
          CREATE TABLE IF NOT EXISTS passages(
            id INTEGER PRIMARY KEY, path TEXT NOT NULL REFERENCES documents(path) ON DELETE CASCADE,
            heading TEXT NOT NULL, text TEXT NOT NULL, start_byte INTEGER NOT NULL, end_byte INTEGER NOT NULL,
            start_line INTEGER NOT NULL, end_line INTEGER NOT NULL, embedding_key TEXT NOT NULL REFERENCES embeddings(key));
          CREATE INDEX IF NOT EXISTS passages_path ON passages(path);
          CREATE VIRTUAL TABLE IF NOT EXISTS passages_fts USING fts5(path, heading, text, tokenize='porter unicode61');
          CREATE VIRTUAL TABLE IF NOT EXISTS passage_vectors USING vec0(embedding float[384] distance_metric=cosine);
          CREATE TRIGGER IF NOT EXISTS passage_delete AFTER DELETE ON passages BEGIN
            DELETE FROM passages_fts WHERE rowid=old.id;
            DELETE FROM passage_vectors WHERE rowid=old.id;
          END;
          CREATE TABLE IF NOT EXISTS links(source TEXT NOT NULL REFERENCES documents(path) ON DELETE CASCADE,
            target TEXT NOT NULL, line INTEGER NOT NULL, destination TEXT NOT NULL);
          CREATE INDEX IF NOT EXISTS links_source ON links(source);
          CREATE INDEX IF NOT EXISTS links_target ON links(target);
          CREATE TABLE IF NOT EXISTS identifiers(path TEXT NOT NULL REFERENCES documents(path) ON DELETE CASCADE,
            name TEXT NOT NULL, passage_id INTEGER NOT NULL REFERENCES passages(id) ON DELETE CASCADE,
            PRIMARY KEY(path,name,passage_id));
          CREATE INDEX IF NOT EXISTS identifiers_name ON identifiers(name);
          PRAGMA user_version=1;
        ")?;
        }
        if version <= 1 {
            db.execute_batch("BEGIN IMMEDIATE;
                CREATE TABLE sections(id INTEGER PRIMARY KEY, path TEXT NOT NULL REFERENCES documents(path) ON DELETE CASCADE,
                    parent_id INTEGER REFERENCES sections(id) ON DELETE CASCADE, title TEXT NOT NULL,
                    heading TEXT NOT NULL, anchor TEXT NOT NULL, start_byte INTEGER NOT NULL, end_byte INTEGER NOT NULL);
                CREATE INDEX sections_path ON sections(path,anchor);
                ALTER TABLE passages ADD COLUMN section_id INTEGER REFERENCES sections(id) ON DELETE CASCADE;
                ALTER TABLE passages ADD COLUMN block_id INTEGER NOT NULL DEFAULT 0;
                ALTER TABLE passages ADD COLUMN kind TEXT NOT NULL DEFAULT 'prose';
                ALTER TABLE passages ADD COLUMN view TEXT NOT NULL DEFAULT 'prose';
                ALTER TABLE passages ADD COLUMN partial INTEGER NOT NULL DEFAULT 0;
                ALTER TABLE passages ADD COLUMN context TEXT;
                CREATE INDEX passages_section ON passages(section_id,view);
                PRAGMA user_version=2;
                COMMIT;")?;
        }
        if version <= 2 {
            db.execute_batch("BEGIN IMMEDIATE;
                CREATE TABLE IF NOT EXISTS passage_content(passage_id INTEGER PRIMARY KEY REFERENCES passages(id) ON DELETE CASCADE,
                    embedding_key TEXT NOT NULL REFERENCES embeddings(key));
                CREATE VIRTUAL TABLE IF NOT EXISTS content_fts USING fts5(text, tokenize='porter unicode61');
                CREATE TRIGGER IF NOT EXISTS content_delete AFTER DELETE ON passage_content BEGIN
                    DELETE FROM content_fts WHERE rowid=old.passage_id;
                END;
                PRAGMA user_version=3;
                COMMIT;")?;
        }
        Ok(Self { db })
    }

    pub fn status(&self) -> Result<Status> {
        let root = self.meta("root")?;
        let root_available = root.as_ref().is_some_and(|p| Path::new(p).is_dir());
        Ok(Status {
            root,
            root_available,
            revision: self.revision()?,
            documents: self
                .db
                .query_row("SELECT count(*) FROM documents", [], |r| {
                    r.get::<_, u32>(0).map(|v| v as usize)
                })?,
            passages: self
                .db
                .query_row("SELECT count(*) FROM passages", [], |r| {
                    r.get::<_, u32>(0).map(|v| v as usize)
                })?,
            model: self.meta("model")?,
            sections: self
                .db
                .query_row("SELECT count(*) FROM sections", [], |r| r.get::<_, u32>(0))?
                as usize,
            needs_reindex: !root_available || self.meta("pipeline")?.as_deref() != Some(PIPELINE),
            content_passages: self.db.query_row(
                "SELECT count(*) FROM passage_content",
                [],
                |r| r.get::<_, u32>(0),
            )? as usize,
            needs_content_index: !self.content_ready()?,
            source_check: None,
        })
    }

    /// Compare current sources with one committed index snapshot. No model or writes.
    /// Uses the same discovery and ignore rules as indexing, including newly added files.
    pub fn check_status(&self) -> Result<Status> {
        let tx = self.db.unchecked_transaction()?;
        let mut status = self.status()?;
        let mut check = SourceCheck::default();
        if let Some(root) = status.root.as_deref().filter(|_| status.root_available) {
            let root = Path::new(root);
            let old = self
                .db
                .prepare("SELECT path,hash FROM documents ORDER BY path")?
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
            match discover_paths(root) {
                Ok(paths) => {
                    let mut seen = HashSet::new();
                    for path in paths {
                        let relative = relative_path(root, &path)?;
                        let inspected = (|| -> Result<()> {
                            if path.metadata()?.len() > MAX_FILE_BYTES {
                                return Ok(());
                            }
                            seen.insert(relative.clone());
                            let bytes = fs::read(&path)?;
                            std::str::from_utf8(&bytes)?;
                            check.checked_documents += 1;
                            match old.get(&relative) {
                                None => check.added.push(relative.clone()),
                                Some(hash) if digest(&bytes) != *hash => {
                                    check.changed.push(relative.clone());
                                }
                                _ => (),
                            }
                            Ok(())
                        })();
                        if let Err(error) = inspected {
                            check.errors.push(format!("{relative}: {error:#}"));
                        }
                    }
                    check.removed = old.keys().filter(|p| !seen.contains(*p)).cloned().collect();
                }
                Err(error) => check.errors.push(format!("Source scan failed: {error:#}")),
            }
        } else {
            check.errors.push(match &status.root {
                Some(root) => format!("Source root unavailable: {root}; use `lore index NEW_ROOT --relocate` after a move"),
                None => "No index yet; run `lore index PATH`".into(),
            });
        }
        status.needs_reindex |= check.needs_reindex();
        status.source_check = Some(check);
        tx.commit()?;
        Ok(status)
    }
    pub(crate) fn content_ready(&self) -> Result<bool> {
        Ok(
            self.meta("content_pipeline")?.as_deref() == Some(crate::content::PIPELINE)
                && self.db.query_row(
                    "SELECT (SELECT count(*) FROM passage_content)=(SELECT count(*) FROM passages)",
                    [],
                    |r| r.get::<_, bool>(0),
                )?,
        )
    }
    pub(crate) fn meta(&self, name: &str) -> Result<Option<String>> {
        Ok(self
            .db
            .query_row("SELECT value FROM metadata WHERE key=?", [name], |r| {
                r.get(0)
            })
            .optional()?)
    }
    pub(crate) fn revision(&self) -> Result<u64> {
        Ok(self
            .meta("revision")?
            .map(|s| s.parse())
            .transpose()?
            .unwrap_or(0))
    }
    pub(crate) fn root(&self) -> Result<PathBuf> {
        self.meta("root")?
            .map(PathBuf::from)
            .context("No index yet. Run `lore index PATH` first")
    }

    pub fn index(&mut self, root: &Path, embedder: &mut dyn Embedder) -> Result<IndexReport> {
        self.index_inner(root, embedder, false, false)
    }

    /// Provision the optional content view; later ordinary refreshes maintain it too.
    pub fn index_with_content(
        &mut self,
        root: &Path,
        embedder: &mut dyn Embedder,
    ) -> Result<IndexReport> {
        self.index_inner(root, embedder, true, false)
    }

    /// Explicitly move this index to another source root and refresh atomically.
    /// Reuses cached embeddings; failure preserves the previous root and revision.
    pub fn index_relocated(
        &mut self,
        root: &Path,
        embedder: &mut dyn Embedder,
        prepare_content: bool,
    ) -> Result<IndexReport> {
        self.index_inner(root, embedder, prepare_content, true)
    }

    fn index_inner(
        &mut self,
        root: &Path,
        embedder: &mut dyn Embedder,
        prepare_content: bool,
        allow_relocation: bool,
    ) -> Result<IndexReport> {
        let start = Instant::now();
        let root = root
            .canonicalize()
            .context("Repository root does not exist")?;
        ensure!(root.is_dir(), "Index root must be a directory");
        ensure!(
            embedder.dimensions() == 384,
            "Expected 384-dimensional embeddings"
        );
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let metadata = |key: &str| -> Result<Option<String>> {
            Ok(tx
                .query_row("SELECT value FROM metadata WHERE key=?", [key], |r| {
                    r.get(0)
                })
                .optional()?)
        };
        let relocated_from = metadata("root")?.filter(|previous| Path::new(previous) != root);
        if let Some(previous) = &relocated_from {
            ensure!(
                allow_relocation,
                "This index belongs to {previous}; use `index NEW_ROOT --relocate` to move and refresh it, or a different --index-dir for another corpus"
            );
        }
        let reset = metadata("model")?.as_deref() != Some(embedder.identity())
            || metadata("pipeline")?.as_deref() != Some(PIPELINE);
        let maintain_content = prepare_content || metadata("content_pipeline")?.is_some();
        let revision: u64 = metadata("revision")?
            .map(|s| s.parse())
            .transpose()?
            .unwrap_or(0);
        let old = tx
            .prepare("SELECT path,hash FROM documents")?
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
        let mut seen = HashSet::new();
        let mut warnings = Vec::new();
        let mut changed = Vec::new();
        let paths = discover_paths(&root)?;
        let mut unchanged_embeddings = 0;
        for path in paths {
            let relative = relative_path(&root, &path)?;
            if path.metadata()?.len() > MAX_FILE_BYTES {
                warnings.push(format!("Skipped {relative}: larger than 2 MiB"));
                continue;
            }
            let bytes = fs::read(&path)
                .with_context(|| format!("Cannot read {relative}; previous index preserved"))?;
            let source =
                std::str::from_utf8(&bytes).with_context(|| format!("{relative} is not UTF-8"))?;
            let hash = digest(&bytes);
            seen.insert(relative.clone());
            if !reset && old.get(&relative) == Some(&hash) {
                unchanged_embeddings += tx.query_row(
                    "SELECT count(*) FROM passages WHERE path=?",
                    [&relative],
                    |r| r.get::<_, u32>(0),
                )? as usize;
                continue;
            }
            let doc = markdown::parse(source, &relative, &|s| embedder.token_count(s))?;
            changed.push((relative, hash, doc));
        }
        let removed: Vec<_> = old.keys().filter(|p| !seen.contains(*p)).cloned().collect();
        let mut missing = BTreeMap::new();
        let mut reused = unchanged_embeddings;
        for (_, _, doc) in &changed {
            for chunk in &doc.chunks {
                let key = embedding_key(embedder.identity(), &chunk.embedding_input);
                let exists: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM embeddings WHERE key=?)",
                    [&key],
                    |r| r.get(0),
                )?;
                if exists {
                    reused += 1;
                } else {
                    missing.insert(key, chunk.embedding_input.clone());
                }
            }
        }
        let missing: Vec<_> = missing.into_iter().collect();
        for (batch_number, batch) in missing.chunks(16).enumerate() {
            let texts: Vec<_> = batch.iter().map(|(_, text)| text.clone()).collect();
            let vectors = embedder.embed_documents(&texts)?;
            ensure!(vectors.len() == texts.len(), "Embedding count mismatch");
            for ((key, _), vector) in batch.iter().zip(vectors) {
                ensure!(
                    vector.len() == 384 && vector.iter().all(|v| v.is_finite()),
                    "Invalid embedding vector"
                );
                tx.execute(
                    "INSERT INTO embeddings(key,vector) VALUES(?,?)",
                    params![key, vector_blob(&vector)],
                )?;
            }
            if batch_number % 8 == 0 || (batch_number + 1) * 16 >= missing.len() {
                eprintln!(
                    "Embedded {}/{} changed passages",
                    ((batch_number + 1) * 16).min(missing.len()),
                    missing.len()
                );
            }
        }
        for path in &removed {
            tx.execute("DELETE FROM documents WHERE path=?", [path])?;
        }
        for (path, hash, doc) in &changed {
            tx.execute("DELETE FROM documents WHERE path=?", [path])?;
            tx.execute(
                "INSERT INTO documents(path,hash,title) VALUES(?,?,?)",
                params![path, hash, doc.title],
            )?;
            let mut section_ids = Vec::new();
            for section in &doc.sections {
                let parent = section.parent.map(|p| section_ids[p]);
                tx.execute("INSERT INTO sections(path,parent_id,title,heading,anchor,start_byte,end_byte) VALUES(?,?,?,?,?,?,?)",
                    params![path,parent,section.title,section.heading,section.anchor,section.start as i64,section.end as i64])?;
                section_ids.push(tx.last_insert_rowid());
            }
            let source = fs::read_to_string(root.join(path))?;
            ensure!(
                digest(source.as_bytes()) == *hash,
                "Source {path} changed during indexing; retry index"
            );
            for chunk in &doc.chunks {
                let key = embedding_key(embedder.identity(), &chunk.embedding_input);
                let context = chunk.context.as_ref().map(|r| serde_json::json!({"text":source[r.clone()],"bytes":[r.start,r.end],"lines":[markdown::line_at(&source,r.start),markdown::line_at(&source,r.end-1)]}).to_string());
                tx.execute("INSERT INTO passages(path,heading,text,start_byte,end_byte,start_line,end_line,embedding_key,section_id,block_id,kind,view,partial,context) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                    params![path,chunk.heading,chunk.text,chunk.start_byte as i64,chunk.end_byte as i64,chunk.start_line as i64,chunk.end_line as i64,key,section_ids[chunk.section],chunk.block as i64,chunk.kind,chunk.view,chunk.partial,context])?;
                let id = tx.last_insert_rowid();
                tx.execute(
                    "INSERT INTO passages_fts(rowid,path,heading,text) VALUES(?,?,?,?)",
                    params![id, path, chunk.heading, chunk.text],
                )?;
                let blob: Vec<u8> =
                    tx.query_row("SELECT vector FROM embeddings WHERE key=?", [&key], |r| {
                        r.get(0)
                    })?;
                tx.execute(
                    "INSERT INTO passage_vectors(rowid,embedding) VALUES(?,?)",
                    params![id, blob],
                )?;
                for name in markdown::identifiers(&chunk.text) {
                    tx.execute(
                        "INSERT INTO identifiers(path,name,passage_id) VALUES(?,?,?)",
                        params![path, name, id],
                    )?;
                }
            }
            for link in &doc.links {
                if let Some(target) = resolve_link(path, &link.destination) {
                    tx.execute(
                        "INSERT INTO links(source,target,line,destination) VALUES(?,?,?,?)",
                        params![path, target, link.line as i64, link.destination],
                    )?;
                }
            }
        }
        let content = if maintain_content {
            crate::content::index(&tx, embedder)?
        } else {
            crate::content::ContentReport::default()
        };
        if content.updated {
            let mut statement = tx.prepare("SELECT path,hash FROM documents")?;
            for row in
                statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            {
                let (path, hash) = row?;
                ensure!(
                    digest(&fs::read(root.join(&path))?) == hash,
                    "Source {path} changed during content indexing; retry index"
                );
            }
        }
        // Retain both representations; content backfill never replaces discovery vectors.
        tx.execute(
            "DELETE FROM embeddings WHERE key NOT IN (SELECT embedding_key FROM passages UNION SELECT embedding_key FROM passage_content)",
            [],
        )?;
        let next_revision = revision
            + u64::from(
                reset
                    || relocated_from.is_some()
                    || !changed.is_empty()
                    || !removed.is_empty()
                    || content.updated,
            );
        for (key, value) in [
            ("root", root.to_string_lossy().to_string()),
            ("model", embedder.identity().to_owned()),
            ("pipeline", PIPELINE.to_owned()),
            ("revision", next_revision.to_string()),
        ] {
            tx.execute("INSERT INTO metadata(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [key,&value])?;
        }
        tx.commit()?;
        let status = self.status()?;
        Ok(IndexReport {
            root: root.display().to_string(),
            relocated_from,
            revision: next_revision,
            documents: status.documents,
            passages: status.passages,
            changed: changed.len(),
            removed: removed.len(),
            embeddings_computed: missing.len(),
            embeddings_reused: reused,
            content_embeddings_computed: content.computed,
            content_embeddings_reused: content.reused,
            elapsed_ms: start.elapsed().as_secs_f64() * 1000.0,
            warnings,
        })
    }

    /// Check only cited files. Newly created or changed unrelated files require `index`.
    pub(crate) fn verify_source(&self, path: &str, expected_hash: &str) -> Result<()> {
        let root = self.root()?;
        let full = root.join(path).canonicalize().with_context(|| {
            format!(
                "Source {path} is missing; run `lore index {}`",
                root.display()
            )
        })?;
        ensure!(
            full.starts_with(&root),
            "Source {path} now resolves outside the corpus; reindex required"
        );
        let bytes = fs::read(&full)?;
        if digest(&bytes) != expected_hash {
            bail!(
                "Source {path} changed since indexing; run `lore index {}` before using its citations",
                root.display()
            );
        }
        Ok(())
    }
}

fn relative_path(root: &Path, path: &Path) -> Result<String> {
    Ok(path
        .strip_prefix(root)?
        .to_str()
        .context("Non-UTF-8 Markdown path")?
        .replace('\\', "/"))
}

fn discover_paths(root: &Path) -> Result<Vec<PathBuf>> {
    let mut walk = WalkBuilder::new(root);
    walk.hidden(false)
        .follow_links(false)
        .require_git(false)
        .git_global(false)
        .add_custom_ignore_filename(".loreignore")
        .filter_entry(|e| {
            !matches!(
                e.file_name().to_str(),
                Some(".git" | ".lore" | "target" | "node_modules" | ".venv")
            )
        });
    let mut paths = Vec::new();
    for entry in walk.build() {
        let entry = entry.context("Cannot scan corpus; the previous index is preserved")?;
        if entry.file_type().is_some_and(|t| t.is_file())
            && entry
                .path()
                .extension()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.eq_ignore_ascii_case("md") || s.eq_ignore_ascii_case("markdown"))
        {
            paths.push(entry.into_path());
        }
    }
    paths.sort();
    Ok(paths)
}

fn embedding_key(model: &str, input: &str) -> String {
    digest(format!("{model}\0{PIPELINE}\0{input}").as_bytes())
}

pub fn resolve_link(source: &str, destination: &str) -> Option<String> {
    let target = destination.split(['#', '?']).next()?;
    if target.is_empty() {
        return Some(source.to_owned());
    }
    if target.contains(':') || target.starts_with('/') || target.starts_with('\\') {
        return None;
    }
    let decoded = percent_encoding::percent_decode_str(target)
        .decode_utf8()
        .ok()?;
    let joined = Path::new(source).parent()?.join(decoded.as_ref());
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::Normal(s) => normalized.push(s),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            _ => return None,
        }
    }
    Some(normalized.to_str()?.replace('\\', "/"))
}
