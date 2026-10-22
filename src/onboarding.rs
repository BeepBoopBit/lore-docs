//! Project initialization and AI client setup.
pub use crate::project::Client;
use crate::{
    embedding::{self, LocalEmbedder},
    index::Index,
    project,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Configuration {
    pub version: u32,
    pub root: PathBuf,
    pub index_dir: PathBuf,
    pub model_dir: PathBuf,
    pub threads: usize,
    pub content: bool,
}

fn saved(root: &Path) -> Result<Option<Configuration>> {
    let path = root.join(".lore/project.json");
    project::read_text(&path)?
        .map(|text| {
            let config: Configuration = serde_json::from_str(&text)
                .with_context(|| format!("Invalid configuration in {}", path.display()))?;
            ensure!(
                config.version == 1,
                "Unsupported Lore project configuration version"
            );
            ensure!(config.root == root,
                "Project settings belong to {}; this project was moved or copied. Relocate or recreate the index and update project.json before initialization.", config.root.display());
            Ok(config)
        })
        .transpose()
}

/// Inspect existing index metadata without creating an index or changing its schema.
pub fn indexed_root(directory: &Path) -> Result<Option<PathBuf>> {
    let path = directory.join("index.sqlite");
    if !path.is_file() {
        return Ok(None);
    }
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let root: Option<String> = db
        .query_row("SELECT value FROM metadata WHERE key='root'", [], |row| {
            row.get(0)
        })
        .optional()?;
    Ok(root.filter(|value| !value.is_empty()).map(PathBuf::from))
}

pub fn configuration(
    root: Option<&Path>,
    index_dir: Option<&Path>,
    model_dir: Option<&Path>,
    threads: Option<usize>,
    content: Option<bool>,
) -> Result<Configuration> {
    let cwd = std::env::current_dir()?;
    let initial = match root {
        Some(root) => root
            .canonicalize()
            .with_context(|| format!("Project does not exist: {}", root.display()))?,
        None => project::discover(&cwd, ".lore").unwrap_or(cwd.clone()),
    };
    let mut previous = saved(&initial)?;
    let index_dir = project::absolute(
        &index_dir
            .map(Path::to_owned)
            .or_else(|| previous.as_ref().map(|config| config.index_dir.clone()))
            .unwrap_or_else(|| initial.join(".lore")),
    )?;
    let root = if root.is_none() {
        indexed_root(&index_dir)?.unwrap_or(initial)
    } else {
        initial
    };
    if previous.as_ref().is_none_or(|config| config.root != root) {
        previous = saved(&root)?;
    }
    let model_dir = project::absolute(
        &model_dir
            .map(Path::to_owned)
            .or_else(|| previous.as_ref().map(|config| config.model_dir.clone()))
            .unwrap_or(project::home()?.join(".lore/models/bge-small")),
    )?;
    let threads = threads
        .or_else(|| previous.as_ref().map(|config| config.threads))
        .unwrap_or(2);
    ensure!(threads > 0, "threads must be positive");
    Ok(Configuration {
        version: 1,
        root,
        index_dir,
        model_dir,
        threads,
        content: content
            .or_else(|| previous.map(|config| config.content))
            .unwrap_or(false),
    })
}

/// Explicit initialization may download model assets unless offline was requested.
pub fn initialize(config: &Configuration, offline: bool) -> Result<Value> {
    ensure!(
        config.root.is_dir(),
        "Project directory is unavailable: {}",
        config.root.display()
    );
    project::validate_state_directory(&config.root.join(".lore"))?;
    project::validate_state_directory(&config.index_dir)?;
    if let Some(root) = indexed_root(&config.index_dir)? {
        ensure!(
            root == config.root,
            "Index belongs to {}; choose a separate --index-dir for {}",
            root.display(),
            config.root.display()
        );
    }
    project::ignore_state(&config.root, ".lore")?;
    project::ignore_database(&config.root, &config.index_dir.join("index.sqlite"))?;
    if !offline {
        eprintln!(
            "Lore: preparing search model at {}",
            config.model_dir.display()
        );
        embedding::install(&config.model_dir)?;
    }
    let mut model = LocalEmbedder::load(&config.model_dir, config.threads)?;
    let mut index = Index::open(&config.index_dir)?;
    eprintln!("Lore: indexing {}", config.root.display());
    let report = if config.content {
        index.index_with_content(&config.root, &mut model)?
    } else {
        index.index(&config.root, &mut model)?
    };
    let status = index.check_status()?;
    ensure!(
        status.root_available && !status.needs_reindex,
        "Initialization did not produce a current index"
    );
    let configuration = config.root.join(".lore/project.json");
    project::write_atomic(
        &configuration,
        &format!("{}\n", serde_json::to_string_pretty(config)?),
    )?;
    Ok(
        json!({"initialized":true,"state":"ready","root":config.root,"index_dir":config.index_dir,
        "model_dir":config.model_dir,"configuration":configuration,"index":report,
        "documents":status.documents,"next":"lore brief \"How do I configure this project?\""}),
    )
}

pub fn uninitialized(root: &Path, index_dir: &Path) -> Value {
    json!({"initialized":false,"state":"not_initialized","root":root,"index_dir":index_dir,
        "next":"Run lore init, lore setup codex, or call lore_init through MCP."})
}

pub fn require_initialized(index_dir: &Path) -> Result<()> {
    ensure!(
        indexed_root(index_dir)?.is_some(),
        "not_initialized: No initialized Lore index at {}. Run 'lore init' or 'lore setup codex' first.",
        index_dir.display()
    );
    Ok(())
}

pub fn server_args(config: &Configuration) -> Vec<String> {
    vec![
        "--index-dir".into(),
        config.index_dir.to_string_lossy().into(),
        "--model-dir".into(),
        config.model_dir.to_string_lossy().into(),
        "--threads".into(),
        config.threads.to_string(),
        "mcp".into(),
        "--root".into(),
        config.root.to_string_lossy().into(),
    ]
}

pub fn setup(
    config: &Configuration,
    client: Client,
    global: bool,
    name: Option<&str>,
    replace: bool,
    offline: bool,
) -> Result<Value> {
    let args = server_args(config);
    let instructions = "Use Lore for questions about this project's Markdown documentation.\nCheck lore_status for the project root and freshness. If not initialized, call lore_init.\nSearch with lore_brief, then read returned targets with lore_read before answering.\nPreserve targets and context when following cursors; cite supporting source paths.\nAfter documentation changes, refresh with lore init from the project root.\nIf MCP is unavailable, use the Lore CLI from this project (lore --help).";
    let registration = project::Registration::prepare(
        "lore",
        &config.root,
        client,
        global,
        name,
        replace,
        &args,
        instructions,
    )?;
    let prepared = registration.complete(|| {
        let initialized = initialize(config, offline)?;
        eprintln!("Lore: verifying MCP status");
        project::probe(&args, "lore_status", &config.root)?;
        Ok(initialized)
    })?;
    Ok(
        json!({"initialized":true,"root":config.root,"client":client,"server_name":registration.name,
        "scope":if global {"user"} else {"project"},"configuration":registration.configuration,
        "instructions":registration.instructions,"mcp_verified":true,"initialization":prepared,
        "next":"Reload the client's MCP configuration or start a new session. Accept project trust if requested, then call lore_status."}),
    )
}
