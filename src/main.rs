use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand};
use lore::{
    embedding::{self, LocalEmbedder},
    evaluate::{self, EvaluationCase},
    index::Index,
    preferences::{Qualifier, QualifierMode},
    reading::PageOptions,
    reranking::{self, LocalReranker, RerankMode, Reranker},
    search::{Brief, BriefOptions, Mode, PassageScoring, Ranking, RepositoryConfig, Selection},
};
use std::{
    io::{self, BufRead, Write},
    path::PathBuf,
};
#[derive(Parser)]
#[command(
    name = "lore",
    version,
    about = "Local Markdown navigation with ranked source evidence"
)]
struct Cli {
    #[arg(long, global = true, default_value = ".lore")]
    index_dir: PathBuf,
    #[arg(long, global = true)]
    model_dir: Option<PathBuf>,
    #[arg(long, global = true)]
    reranker_dir: Option<PathBuf>,
    #[arg(long, global = true, default_value_t = 2)]
    threads: usize,
    #[command(subcommand)]
    command: Command,
}
#[derive(Args, Clone)]
struct QueryArgs {
    #[arg(long,value_enum,default_value_t=Mode::Hybrid)]
    mode: Mode,
    #[arg(long, default_value_t = 5)]
    max_files: usize,
    #[arg(long, default_value_t = 2000)]
    max_tokens: usize,
    #[arg(long, default_value_t = 40)]
    candidates: usize,
    /// Relevance cutoff, not an answerability probability.
    #[arg(long, default_value_t = 0.5)]
    min_similarity: f32,
    #[arg(long)]
    no_connections: bool,
    /// Disable declaration context for recognized API options and key tables.
    #[arg(long)]
    no_api_context: bool,
    /// Literal corpus-relative path or directory; applies before retrieval.
    #[arg(long, default_value = "")]
    scope: String,
    /// Document aggregation or strongest-section control.
    #[arg(long,value_enum,default_value_t=Ranking::Section)]
    ranking: Ranking,
    /// Select evidence jointly within the budget, or use the previous file-first control.
    #[arg(long,value_enum,default_value_t=Selection::Evidence)]
    selection: Selection,
    /// Experimental content-only or blended evidence scores; discovery stays unchanged.
    #[arg(long,value_enum,default_value_t=PassageScoring::Contextual)]
    passage_scoring: PassageScoring,
    /// Experimental automatic language/platform preferences; does not filter candidates.
    #[arg(long,value_enum,default_value_t=QualifierMode::Off)]
    qualifiers: QualifierMode,
    /// Explicit soft preference, also works with automatic detection off; repeat as needed.
    #[arg(long, value_enum)]
    prefer: Vec<Qualifier>,
    /// Optional section references; use at most 15% of the same response budget (0-8).
    #[arg(long, default_value_t = 0)]
    alternatives: usize,
    /// Experimental bounded passage recovery within five discovered files.
    #[arg(long)]
    recover_sections: bool,
    /// Optional local pair scoring, or the same shortlist without model scoring.
    #[arg(long, value_enum, default_value_t=RerankMode::Off)]
    rerank: RerankMode,
    /// Experimental, bounded one-hop expansion through explicit section links.
    #[arg(long)]
    graph: bool,
    /// Explicit JSON file containing optional role/prefix/weight rules.
    #[arg(long)]
    config: Option<PathBuf>,
}
impl QueryArgs {
    fn options(&self) -> Result<BriefOptions> {
        let config: RepositoryConfig = if let Some(path) = &self.config {
            serde_json::from_slice(
                &std::fs::read(path).with_context(|| format!("Cannot read {}", path.display()))?,
            )?
        } else {
            RepositoryConfig::default()
        };
        Ok(BriefOptions {
            mode: self.mode,
            max_files: self.max_files,
            max_tokens: self.max_tokens,
            candidates: self.candidates,
            min_similarity: self.min_similarity,
            connections: !self.no_connections,
            api_context: !self.no_api_context,
            scope: self.scope.clone(),
            ranking: self.ranking,
            selection: self.selection,
            passage_scoring: self.passage_scoring,
            qualifiers: self.qualifiers,
            prefer: self.prefer.clone(),
            alternatives: self.alternatives,
            recover_sections: self.recover_sections,
            rerank: self.rerank,
            graph: self.graph,
            roles: config.roles,
        })
    }
}
#[derive(Args)]
struct PageArgs {
    /// Bound the complete JSON response using the UTF-8 byte estimate.
    #[arg(long, default_value_t = 2000)]
    max_tokens: usize,
    /// Continue the same target using its returned next_cursor.
    #[arg(long)]
    cursor: Option<String>,
    #[arg(long)]
    json: bool,
}
impl PageArgs {
    fn options(&self) -> PageOptions {
        PageOptions {
            max_tokens: self.max_tokens,
            cursor: self.cursor.clone(),
        }
    }
}
#[derive(Subcommand)]
enum Command {
    /// Serve read-only documentation tools over MCP stdio for one existing index.
    Mcp {
        /// Optional consumer-document role config, selectable per query as source_profile=usage.
        #[arg(long)]
        usage_config: Option<PathBuf>,
    },
    /// Explicit provisioning; the only command that accesses the network.
    Model {
        #[command(subcommand)]
        command: ModelCommand,
    },
    Index {
        #[arg(default_value = ".")]
        root: PathBuf,
        /// Provision content embeddings; subsequent refreshes maintain them automatically.
        #[arg(long)]
        content: bool,
        /// Move this index to ROOT and refresh it atomically, reusing cached embeddings.
        #[arg(long)]
        relocate: bool,
        #[arg(long)]
        json: bool,
    },
    Brief {
        query: String,
        /// Include up to three follow-up reads within the same total token budget.
        #[arg(long)]
        with_guidance: bool,
        #[command(flatten)]
        options: QueryArgs,
        #[arg(long)]
        json: bool,
    },
    /// Read an indexed FILE or FILE#anchor in source order, including child sections. No model needed.
    Read {
        target: String,
        /// Include introductory text from enclosing headings as separate source slices.
        #[arg(long)]
        context: bool,
        #[command(flatten)]
        options: PageArgs,
    },
    /// Plan ancestor introductions and the full FILE#anchor subtree. No model needed.
    Plan {
        target: String,
        #[command(flatten)]
        options: PageArgs,
    },
    /// List a file's heading anchors in source order. No model needed.
    Outline {
        path: String,
        #[command(flatten)]
        options: PageArgs,
    },
    /// Show discovery ranks, contributing source blocks, and assembly decisions.
    Diagnose {
        query: String,
        /// Return bounded omission hints and read targets instead of the full trace.
        #[arg(long)]
        compact: bool,
        #[command(flatten)]
        options: QueryArgs,
    },
    /// Read JSON-string queries from stdin, write one JSON response per line, retain the model.
    Batch {
        #[command(flatten)]
        options: QueryArgs,
        #[arg(long)]
        diagnostics: bool,
        /// Bound diagnostics to omission hints and read targets.
        #[arg(long, requires = "diagnostics")]
        compact: bool,
        /// Return evidence and follow-up reads together within one response budget.
        #[arg(long, conflicts_with_all = ["diagnostics", "compact"])]
        with_guidance: bool,
    },
    Related {
        path: String,
        #[arg(long, default_value_t = 8)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    Status {
        /// Check added, changed, removed, ignored and unreadable source documents. No model needed.
        #[arg(long)]
        check: bool,
        #[arg(long)]
        json: bool,
    },
    /// Compare all three retrieval modes. Connections are excluded; graph is opt-in.
    Eval {
        dataset: PathBuf,
        #[command(flatten)]
        options: QueryArgs,
        #[arg(long)]
        json: bool,
    },
}
#[derive(Subcommand)]
enum ModelCommand {
    Install {
        /// Provision the optional MiniLM reranker instead of the embedding model.
        #[arg(long)]
        reranker: bool,
    },
}
fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
fn output(value: &impl serde::Serialize) -> Result<()> {
    let mut out = io::stdout().lock();
    serde_json::to_writer(&mut out, value)?;
    writeln!(out)?;
    Ok(())
}
fn model(mode: Mode, dir: &std::path::Path, threads: usize) -> Result<Option<LocalEmbedder>> {
    if mode == Mode::Keyword {
        Ok(None)
    } else {
        Ok(Some(LocalEmbedder::load(dir, threads)?))
    }
}
fn reranker(
    mode: RerankMode,
    dir: &std::path::Path,
    threads: usize,
) -> Result<Option<LocalReranker>> {
    if mode == RerankMode::Local {
        Ok(Some(LocalReranker::load(dir, threads)?))
    } else {
        Ok(None)
    }
}
fn run() -> Result<()> {
    let cli = Cli::parse();
    let model_dir = cli
        .model_dir
        .unwrap_or_else(|| cli.index_dir.join("models/bge-small"));
    let reranker_dir = cli
        .reranker_dir
        .unwrap_or_else(|| cli.index_dir.join("models/minilm-reranker"));
    if let Command::Model {
        command: ModelCommand::Install { reranker },
    } = cli.command
    {
        return if reranker {
            reranking::install(&reranker_dir)
        } else {
            embedding::install(&model_dir)
        };
    }
    if let Command::Mcp { usage_config } = cli.command {
        return lore::mcp::serve_stdio(lore::mcp::LoreServer::open(
            cli.index_dir,
            model_dir,
            cli.threads,
            usage_config,
        )?);
    }
    let mut index = Index::open(&cli.index_dir)?;
    match cli.command {
        Command::Model { .. } | Command::Mcp { .. } => unreachable!(),
        Command::Index {
            root,
            json,
            content,
            relocate,
        } => {
            let mut model = LocalEmbedder::load(&model_dir, cli.threads)?;
            let r = if relocate {
                index.index_relocated(&root, &mut model, content)?
            } else if content {
                index.index_with_content(&root, &mut model)?
            } else {
                index.index(&root, &mut model)?
            };
            if json {
                output(&r)?;
            } else {
                if let Some(previous) = &r.relocated_from {
                    println!("Relocated from {previous}");
                }
                println!(
                    "Indexed {} documents / {} blocks from {}\nRevision {} · {} changed · {} removed · {} embeddings computed · {} reused · {:.0} ms",
                    r.documents,
                    r.passages,
                    r.root,
                    r.revision,
                    r.changed,
                    r.removed,
                    r.embeddings_computed,
                    r.embeddings_reused,
                    r.elapsed_ms
                );
                println!(
                    "Content view · {} embeddings computed · {} reused",
                    r.content_embeddings_computed, r.content_embeddings_reused
                );
                for w in r.warnings {
                    eprintln!("warning: {w}");
                }
            }
        }
        Command::Brief {
            query,
            options,
            json,
            with_guidance,
        } => {
            let options = options.options()?;
            let mut model = model(options.mode, &model_dir, cli.threads)?;
            let mut reranker = reranker(options.rerank, &reranker_dir, cli.threads)?;
            if with_guidance {
                let result = index.guided_brief_with_reranker(
                    &query,
                    model.as_mut().map(|m| m as &mut dyn embedding::Embedder),
                    reranker.as_mut().map(|m| m as &mut dyn Reranker),
                    &options,
                )?;
                if json {
                    output(&result)?;
                } else {
                    print_brief(&result.brief);
                    println!(
                        "\nEvidence and guidance · ~{} tokens",
                        result.estimated_tokens
                    );
                    println!("{}", result.guidance.limitations);
                    if let Some(choice) = &result.guidance.recommendation {
                        let hint = &result.guidance.next_reads[choice.next_read_index];
                        let (target, context) =
                            if choice.choice == lore::recommendation::ReadChoice::Broader {
                                let parent = hint
                                    .broader_read
                                    .as_ref()
                                    .expect("recommended parent exists");
                                (&parent.read_target, parent.context)
                            } else {
                                (&hint.read_target, hint.context)
                            };
                        println!(
                            "Recommended: read {}{} · {} source bytes (+{} over focused)",
                            target,
                            if context { " --context" } else { "" },
                            choice.source_bytes,
                            choice.additional_source_bytes
                        );
                        println!("  {}", choice.reason.explanation());
                    }
                    for hint in &result.guidance.next_reads {
                        println!(
                            "  Read {}{} · gaps: {} steps, {} adjoining blocks, {} section blocks, {} ancestor blocks",
                            hint.read_target,
                            if hint.context { " --context" } else { "" },
                            hint.gaps.numbered_steps,
                            hint.gaps.numbered_context_blocks,
                            hint.gaps.section_blocks,
                            hint.gaps.ancestor_blocks
                        );
                        if let Some(parent) = &hint.broader_read {
                            println!(
                                "    Broader option: {}{} · {} source bytes (+{})",
                                parent.read_target,
                                if parent.context { " --context" } else { "" },
                                parent.source_bytes,
                                parent.additional_source_bytes
                            );
                        }
                    }
                    println!(
                        "  {} suggestions omitted; {} consolidated. Probe limited: {}.",
                        result.guidance.omitted_suggestions,
                        result.guidance.consolidated_suggestions,
                        result.guidance.probe.limited
                    );
                }
                return Ok(());
            }
            let brief = index.brief_with_reranker(
                &query,
                model.as_mut().map(|m| m as &mut dyn embedding::Embedder),
                reranker.as_mut().map(|m| m as &mut dyn Reranker),
                &options,
            )?;
            if json {
                output(&brief)?;
            } else {
                print_brief(&brief);
            }
        }
        Command::Read {
            target,
            context: true,
            options,
        } => {
            let result = index.read_context(&target, &options.options())?;
            if options.json {
                output(&result)?;
            } else {
                println!(
                    "Context for {} · revision {} · ~{} tokens\n",
                    result.target, result.index_revision, result.estimated_tokens
                );
                for part in &result.parts {
                    println!(
                        "[Step {} · {:?} · {} · lines {}-{}]",
                        part.step,
                        part.role,
                        part.target,
                        part.content.lines[0],
                        part.content.lines[1]
                    );
                    if part.starts_mid_block {
                        println!("[Continues a Markdown block.]");
                    }
                    print!("{}", part.content.text);
                    if !part.content.text.ends_with('\n') {
                        println!();
                    }
                    if part.ends_mid_block {
                        println!("[This Markdown block continues on the next page.]");
                    }
                    println!();
                }
                if let Some(cursor) = result.next_cursor {
                    println!(
                        "More source remains. Read the same target with --context --cursor {cursor}"
                    );
                }
            }
        }
        Command::Read {
            target,
            context: false,
            options,
        } => {
            let result = index.read(&target, &options.options())?;
            if options.json {
                output(&result)?;
            } else {
                println!(
                    "{}:{}-{} · revision {} · ~{} tokens\n",
                    result.path,
                    result.content.lines[0],
                    result.content.lines[1],
                    result.index_revision,
                    result.estimated_tokens
                );
                if result.starts_mid_block {
                    println!("[Continues a Markdown block from the previous page.]\n");
                }
                print!("{}", result.content.text);
                if !result.content.text.ends_with('\n') {
                    println!();
                }
                if result.ends_mid_block {
                    println!("\n[This Markdown block continues on the next page.]");
                }
                if let Some(cursor) = result.next_cursor {
                    println!("\nMore source remains. Read the same target with --cursor {cursor}");
                }
                if let Some(parent) = result.section.parent_target {
                    println!(
                        "\nSurrounding section: {parent} (use `lore read` for its full context)"
                    );
                }
            }
        }
        Command::Plan { target, options } => {
            let result = index.plan(&target, &options.options())?;
            if options.json {
                output(&result)?;
            } else {
                println!(
                    "Reading plan for {} · revision {}\n",
                    result.target, result.index_revision
                );
                for step in result.steps {
                    println!(
                        "{}. {:?} · {} · lines {}-{}\n   {}",
                        step.order,
                        step.role,
                        step.target,
                        step.lines[0],
                        step.lines[1],
                        step.heading
                    );
                }
                if let Some(cursor) = result.next_cursor {
                    println!("\nMore plan steps remain. Repeat this plan with --cursor {cursor}");
                }
                println!(
                    "\nRead these source slices in order with `lore read TARGET --context`. Introduction steps cover only the listed lines, not the whole ancestor section."
                );
            }
        }
        Command::Outline { path, options } => {
            let result = index.outline(&path, &options.options())?;
            if options.json {
                output(&result)?;
            } else {
                println!("{} · revision {}\n", result.path, result.index_revision);
                for section in result.sections {
                    println!(
                        "{}{} · lines {}-{}\n{}  {}#{}",
                        "  ".repeat(section.depth.saturating_sub(1)),
                        section.title,
                        section.lines[0],
                        section.lines[1],
                        "  ".repeat(section.depth.saturating_sub(1)),
                        result.path,
                        section.anchor
                    );
                }
                if let Some(cursor) = result.next_cursor {
                    println!(
                        "\nMore headings remain. Outline the same file with --cursor {cursor}"
                    );
                }
            }
        }
        Command::Diagnose {
            query,
            options,
            compact,
        } => {
            let options = options.options()?;
            let mut model = model(options.mode, &model_dir, cli.threads)?;
            let mut reranker = reranker(options.rerank, &reranker_dir, cli.threads)?;
            let diagnostic = index.diagnose_with_reranker(
                &query,
                model.as_mut().map(|m| m as &mut dyn embedding::Embedder),
                reranker.as_mut().map(|m| m as &mut dyn Reranker),
                &options,
            )?;
            if compact {
                output(&index.compact_diagnostics(&diagnostic, options.max_tokens)?)?;
            } else {
                output(&diagnostic)?;
            }
        }
        Command::Batch {
            options,
            diagnostics,
            compact,
            with_guidance,
        } => {
            let options = options.options()?;
            let mut model = model(options.mode, &model_dir, cli.threads)?;
            let mut reranker = reranker(options.rerank, &reranker_dir, cli.threads)?;
            let mut failures = 0;
            let mut input = io::stdin().lock();
            let mut number = 0;
            while let Some(line) = read_line(&mut input)? {
                number += 1;
                let result = (|| -> Result<()> {
                    let query: String =
                        serde_json::from_slice(&line.context("Input line exceeds 64 KiB")?)
                            .context("Each input line must be one JSON string")?;
                    if with_guidance {
                        output(&index.guided_brief_with_reranker(
                            &query,
                            model.as_mut().map(|m| m as &mut dyn embedding::Embedder),
                            reranker.as_mut().map(|m| m as &mut dyn Reranker),
                            &options,
                        )?)?;
                    } else if diagnostics {
                        let diagnostic = index.diagnose_with_reranker(
                            &query,
                            model.as_mut().map(|m| m as &mut dyn embedding::Embedder),
                            reranker.as_mut().map(|m| m as &mut dyn Reranker),
                            &options,
                        )?;
                        if compact {
                            output(&index.compact_diagnostics(&diagnostic, options.max_tokens)?)?;
                        } else {
                            output(&diagnostic)?;
                        }
                    } else {
                        output(&index.brief_with_reranker(
                            &query,
                            model.as_mut().map(|m| m as &mut dyn embedding::Embedder),
                            reranker.as_mut().map(|m| m as &mut dyn Reranker),
                            &options,
                        )?)?;
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    failures += 1;
                    output(&serde_json::json!({"line":number,"error":format!("{error:#}")}))?;
                }
            }
            ensure!(
                failures == 0,
                "{failures} batch queries failed; see per-line errors"
            );
        }
        Command::Related { path, limit, json } => {
            let r = index.related(&path, limit)?;
            if json {
                output(&r)?;
            } else {
                println!("Connections for {} (revision {})", r.path, r.index_revision);
                for c in r.connections {
                    println!(
                        "  {} · {} · {}",
                        c.path,
                        c.kind,
                        c.identifier.as_deref().unwrap_or(&c.direction)
                    );
                }
            }
        }
        Command::Status { json, check } => {
            let r = if check {
                index.check_status()?
            } else {
                index.status()?
            };
            if json {
                output(&r)?;
            } else {
                println!(
                    "{} documents · {} sections · {} blocks · revision {}\nRoot: {}\nReindex required: {}",
                    r.documents,
                    r.sections,
                    r.passages,
                    r.revision,
                    r.root.as_deref().unwrap_or("not indexed"),
                    r.needs_reindex
                );
                if !r.root_available {
                    if r.root.is_some() {
                        println!(
                            "Source root unavailable. After a move, run `lore index NEW_ROOT --relocate`."
                        );
                    } else {
                        println!("No source root indexed; run `lore index PATH`.");
                    }
                }
                if let Some(check) = &r.source_check {
                    println!(
                        "Source check: {} documents · {} added · {} changed · {} removed or excluded · {} errors",
                        check.checked_documents,
                        check.added.len(),
                        check.changed.len(),
                        check.removed.len(),
                        check.errors.len()
                    );
                    for (kind, paths) in [
                        ("Added", &check.added),
                        ("Changed", &check.changed),
                        ("Removed or excluded", &check.removed),
                    ] {
                        for path in paths {
                            println!("  {kind}: {path}");
                        }
                    }
                    for error in &check.errors {
                        println!("  Error: {error}");
                    }
                } else {
                    println!("Source freshness unchecked; run `lore status --check`.");
                }
            }
        }
        Command::Eval {
            dataset,
            options,
            json,
        } => {
            let cases: Vec<EvaluationCase> = serde_json::from_slice(
                &std::fs::read(&dataset)
                    .with_context(|| format!("Cannot read {}", dataset.display()))?,
            )?;
            let mut model = LocalEmbedder::load(&model_dir, cli.threads)?;
            let options = options.options()?;
            let mut reranker = reranker(options.rerank, &reranker_dir, cli.threads)?;
            let report = evaluate::evaluate_with_reranker(
                &index,
                &mut model,
                reranker.as_mut().map(|m| m as &mut dyn Reranker),
                &cases,
                &options,
            )?;
            if json {
                output(&report)?;
            } else {
                for m in &report.modes {
                    println!(
                        "{:?}: target recall {:.1}% · evidence {:.1}% · candidate recall {:.1}% · p95 {:.1} ms",
                        m.mode,
                        m.summary.mean_target_recall_at_5.unwrap_or(0.0) * 100.0,
                        m.summary.mean_evidence_recall.unwrap_or(0.0) * 100.0,
                        m.summary.mean_candidate_recall.unwrap_or(0.0) * 100.0,
                        m.summary.p95_ms
                    );
                }
                println!(
                    "Source-informed evaluation; use --json for per-query diagnostics and limitations."
                );
            }
        }
    }
    Ok(())
}
/// Bounded allocation and recovery after an oversized line, without losing subsequent queries.
fn read_line(input: &mut impl BufRead) -> Result<Option<Option<Vec<u8>>>> {
    let mut line = Vec::new();
    let mut oversized = false;
    let mut any = false;
    loop {
        let buf = input.fill_buf()?;
        if buf.is_empty() {
            return Ok(any.then_some((!oversized).then_some(line)));
        }
        any = true;
        let end = buf.iter().position(|b| *b == b'\n').map(|i| i + 1);
        let n = end.unwrap_or(buf.len());
        if line.len() + n > 65536 {
            oversized = true;
            line.clear();
        }
        if !oversized {
            line.extend_from_slice(&buf[..n]);
        }
        input.consume(n);
        if end.is_some() {
            return Ok(Some((!oversized).then_some(line)));
        }
    }
}
fn print_brief(brief: &Brief) {
    println!(
        "{} · revision {} · {:.1} ms · ~{} tokens\n",
        brief.status, brief.index_revision, brief.elapsed_ms, brief.estimated_tokens
    );
    for file in &brief.files {
        println!(
            "{}. {} (rank score {:.5})",
            file.rank, file.path, file.score
        );
        for p in &file.passages {
            println!(
                "\n   {}:{}-{} · {}\n",
                file.path, p.lines[0], p.lines[1], p.heading
            );
            if let Some(ctx) = &p.context {
                println!("{}", ctx.text);
            }
            println!("{}\n", p.text);
        }
        for c in &file.connections {
            println!(
                "   → {}{} [{}]",
                c.path,
                c.anchor.as_ref().map_or(String::new(), |a| format!("#{a}")),
                c.kind
            );
        }
    }
    if !brief.qualifiers.is_empty() {
        println!("\nLanguage/platform preferences: {:?}", brief.qualifiers);
    }
    if !brief.alternative_sections.is_empty() {
        println!("\nOther retrieved sections to read:");
        for a in &brief.alternative_sections {
            println!("  {} · {}", a.target, a.title);
        }
    }
    if brief.truncated {
        println!("\nAdditional evidence or connections did not fit the requested limits.");
    }
    if !brief.files.is_empty() {
        println!(
            "\nRead a complete section with `lore read FILE#anchor`; find anchors with `lore outline FILE`."
        );
    }
}
