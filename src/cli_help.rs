pub const CLI: &str = r#"Get started:
  Run these commands inside your project:

    lore init
    lore brief "How do I configure this project?"

Examples:
  Read a document returned by a search:
    lore read README.md

  Discover its section references:
    lore outline README.md

  Get search results and suggested reads as JSON:
    lore brief "How does authentication work?" --with-guidance --json

  Check whether documentation has changed:
    lore status --check

  Refresh using saved project settings:
    lore init

  Search another project's existing index:
    lore --index-dir /path/to/project/.lore brief "How do I run tests?"

Use with AI agents:
  Initialize and connect a client in one command:
    lore setup codex
    lore setup claude

  Setup writes the client's MCP configuration and adds usage instructions to
  AGENTS.md or CLAUDE.md. Reload the client, then ask it to call lore_status.
  An agent with terminal access can also run Lore commands directly.
  AI setup guide: https://github.com/BeepBoopBit/lore-docs

Model and storage:
  "lore init" downloads or reuses a local search model (~67 MB).
  Lore uses it automatically for indexing and semantic search. Your AI keeps
  using its existing model. No API key is required.
  By default, indexes and settings live in .lore/; init adds the ignore rule.
  Models are shared across projects in ~/.lore/models/; download them once.
  Indexing and search work offline after the model is downloaded.

Run "lore <command> --help" for command options and examples."#;

pub const MODEL: &str = r#"Examples:
  Download the search model before indexing your first project:
    lore model install

  Store model files in a separate directory:
    lore --model-dir /path/to/shared/models/bge-small model install

  Download the optional reranker:
    lore model install --reranker

The search model is about 67 MB and runs locally. Lore uses it automatically;
there is no model to select in your AI client and no API key is required.
Model installation and explicit initialization can download model assets.
Models default to ~/.lore/models/ and are shared across projects and indexes.
Existing valid files are reused; missing or damaged files are downloaded again.

After installation, initialize a project using the cached model:
  lore init --offline
  lore brief "your question"

A standalone model install does not save project settings. For a custom model,
pass --model-dir to init once; later project commands reuse the saved path."#;

pub const MODEL_INSTALL: &str = r#"Examples:
  Download or repair the default search model:
    lore model install

  Download it into a custom directory:
    lore --model-dir /path/to/shared/models/bge-small model install

  Download the optional reranker for searches using --rerank local:
    lore model install --reranker

The search model (~67 MB) defaults to ~/.lore/models/bge-small. The optional
reranker defaults to ~/.lore/models/minilm-reranker. Both are shared across
projects, independent of --index-dir. Override them with --model-dir and
--reranker-dir respectively.
No API key is required. Indexing and search run offline after installation.

Next, run these commands inside your project:
  lore init --offline
  lore brief "How do I configure this project?"

For a custom model, pass --model-dir to init once to save it for this project.
Use init without --offline to prepare the project and download assets in one step."#;

pub const INDEX: &str = r#"For first-time setup or a saved project refresh, prefer "lore init". This lower-level
command refreshes an index without saving settings or managing ignore rules.

Examples:
  Prepare the model, then index the current project:
    lore model install
    lore index .

  Index a project from another directory:
    lore model install
    lore --index-dir /path/to/project/.lore index /path/to/project

  Refresh after documentation changes:
    lore index .

Lore indexes UTF-8 .md and .markdown files, respects .gitignore and .loreignore,
and leaves source documents unchanged. Each index belongs to one project.
Refreshes reuse cached embeddings where possible. There is no file watcher.

Next, run "lore brief" with your question using the same index and model paths."#;

pub const BRIEF: &str = r#"Examples:
  Search documentation with a question:
    lore brief "How do I configure this project?"

  Get structured results and suggested follow-up reads for an AI agent:
    lore brief "How does authentication work?" --with-guidance --json

  Search only the docs directory:
    lore brief "Which environment variables are required?" --scope docs

  Search for a literal term without loading the model:
    lore brief "APP_PORT" --mode keyword

  Search another project's existing index:
    lore --index-dir /path/to/project/.lore brief "How do I run tests?"

First run "lore init" inside your project.
Read returned sources with "lore read FILE" or "lore read 'FILE#anchor'".
Use "lore outline FILE" to discover exact section references."#;

pub const READ: &str = r#"Examples:
  Read a document returned by a search:
    lore read README.md

  Find its section references, then read a section with enclosing context:
    lore outline README.md
    lore read 'README.md#install' --context --json

  Continue a long read using the cursor from the previous JSON response:
    lore read 'README.md#install' --context --json --cursor 'RETURNED_CURSOR'

Replace the example file and #install with references returned by your index.
Keep the same target and --context setting when continuing a read. After
refreshing the index, restart the read without an old cursor.
Reads need an existing index and matching source files, but no model."#;

pub const OUTLINE: &str = r#"Examples:
  List a document's headings and exact section references:
    lore outline README.md

  Get headings as JSON for an AI agent:
    lore outline README.md --json

  Continue a long outline using its returned cursor:
    lore outline README.md --json --cursor 'RETURNED_CURSOR'

Use a file path returned by your search. Copy a section reference into
"lore read 'FILE#anchor' --context" to read that section with enclosing context.
An existing index is required; the model is not loaded."#;

pub const STATUS: &str = r#"Examples:
  Show the indexed project and document counts:
    lore status

  Check whether source documents have changed:
    lore status --check

  Verify an AI agent's index from another directory:
    lore --index-dir /path/to/project/.lore status --check --json

Before initialization, reports initialized: false and state: not_initialized
without creating a database. Run "lore init" to prepare the project.
For a prepared project containing Markdown, check that initialized is true, root
is the intended absolute path, root_available is true, needs_reindex is false,
and documents is greater than zero. Freshness is checked only with --check.
Refresh changed documentation with "lore init". A stale index stays initialized.
Status does not load a model."#;

pub const MCP: &str = r#"Setup:
  Initialize and register the current project with your AI client:
    lore setup codex
    lore setup claude

  For manual registration, configure an absolute executable path and arguments
  equivalent to this command; --root also allows startup before initialization:
    lore --index-dir /absolute/path/to/project/.lore mcp --root /absolute/path/to/project

  Your client starts Lore and communicates through stdin/stdout. Running the
  server directly in a terminal waits for client input. No port is needed.

Verify and use:
  Reload your client or start a new session. Call lore_status with {} and check
  the project path. If initialized is false, call lore_init with {} to prepare
  this bound project, or {"offline": true} to require cached model assets.
  Search with lore_brief, then read returned sources with lore_read.

Only lore_init writes project state and can download pinned model assets.
Search and reading tools remain read-only and require an initialized index.
The root is fixed by server configuration; lore_init cannot redirect it or
register an AI client. Refresh saved preparation with lore init or lore_init.
Models and custom index paths saved by init are reused automatically.

AI setup guide: https://github.com/BeepBoopBit/lore-docs"#;

pub const PLAN: &str = r#"Examples:
  Inspect the source sections used for a contextual read:
    lore plan 'README.md#install'

  Get the plan as JSON:
    lore plan 'README.md#install' --json

Use a reference returned by "lore outline FILE" in place of the example.
Read the source text with "lore read 'FILE#anchor' --context".
Continue a paginated plan with its returned --cursor, keeping the same target.
An existing index is required; the model is not loaded."#;

pub const RELATED: &str = r#"Examples:
  Find documents connected to a search result:
    lore related README.md

  Limit the results and return JSON:
    lore related README.md --limit 5 --json

Use an indexed file path. Connections come from document links, shared
identifiers, and semantic similarity between indexed passages. Inspect a result
with "lore outline FILE" or "lore read FILE"."#;

pub const DIAGNOSE: &str = r#"Examples:
  Inspect retrieval ranks, contributing passages, and selection decisions:
    lore diagnose "How do I configure this project?"

  Get bounded omission hints and suggested read targets:
    lore diagnose "How do I configure this project?" --compact

Output is JSON. Use the same search options as "lore brief" to investigate a
particular result. Run "lore init" before using this command."#;

pub const BATCH: &str = r#"Examples:
  Search a file containing one JSON string per line:
    lore batch < queries.jsonl

  Include suggested follow-up reads:
    lore batch --with-guidance < queries.jsonl

  Return compact diagnostics for each query:
    lore batch --diagnostics --compact < queries.jsonl

Example queries.jsonl contents:
  "How do I configure this project?"
  "How do I run tests?"

Run "lore init" first. Lore keeps the model loaded and writes one
JSON response per input line. Each line must be a JSON string, not an object."#;

pub const EVAL: &str = r#"Examples:
  Compare keyword, semantic, and hybrid search on an evaluation dataset:
    lore eval /path/to/evaluation.json

  Include per-query results and limitations as JSON:
    lore eval /path/to/evaluation.json --json

Run "lore init" first. The dataset is a JSON array of cases:
  [
    {
      "id": "setup", "category": "getting-started", "query": "How do I install?",
      "expected": [{"paths": ["README.md"], "contains": "cargo install"}]
    }
  ]

Replace the example paths and expected phrase with evidence from your project.
Connections are excluded; graph expansion is opt-in with --graph.
Use "lore brief" for ordinary documentation questions."#;

pub const INIT: &str = r#"Examples:
  Prepare the current project and its search model:
    lore init

  Initialize another project:
    lore init /path/to/project

  Reuse existing model assets without downloading:
    lore init --offline

  Save custom index and model paths for future commands:
    lore --index-dir /path/to/index --model-dir /path/to/model init

  Prepare optional content embeddings and report preparation as JSON:
    lore init --content --json

Initialization adds .lore/ to .gitignore, prepares the model, indexes Markdown,
and saves settings in .lore/project.json. A custom index inside the project is
also ignored. Repeating init refreshes saved preparation and reuses valid models
and cached embeddings. Existing valid indexes are adopted.
Commands from this project or its subdirectories reuse saved paths and settings.
Models default to ~/.lore/models/bge-small, shared across all projects.
An existing initialized index remains usable without a project settings file.

Next, search with lore brief or connect an AI client with lore setup codex/claude.
MCP clients can perform the same initialization using the lore_init tool."#;

pub const SETUP: &str = r#"Examples:
  Initialize this project and configure Codex:
    lore setup codex

  Configure Claude Code:
    lore setup claude

  Configure a different project using already-downloaded model assets:
    lore setup codex /path/to/project --offline

  Register a distinct connection for your user across projects:
    lore setup codex --global

  Choose a distinct server name:
    lore setup codex --name lore-docs

  Replace a conflicting registration explicitly:
    lore setup claude --replace

Setup initializes the project, verifies the local MCP server with lore_status,
and configures the selected client. Project setup uses .codex/config.toml for
Codex or .mcp.json for Claude Code. --global uses $CODEX_HOME/config.toml
(default ~/.codex/config.toml) or $CLAUDE_CONFIG_DIR/.claude.json
(default ~/.claude.json), with a project-specific server name.
A managed block in AGENTS.md or CLAUDE.md tells the agent when to use Lore.
Existing settings and instructions are preserved. Identical setup can be repeated;
a same-named connection with different settings needs --replace or another --name.

Reload the client's MCP configuration or start a new session after setup.
The client may require project trust. Ask the agent to call lore_status to verify.
AI setup guide: https://github.com/BeepBoopBit/lore-docs"#;
