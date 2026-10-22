# Usage

Lore finds passages in Markdown documentation and returns their source locations.
Start with a question, then read the relevant files or sections.

For installation and a workflow tailored to coding agents, see [AI agents](agents.md).

## Index a project

First [install Lore](../README.md#install), then run:

```sh
cd /path/to/project
lore init
```

`init` downloads or verifies the search model (~67 MB), indexes Markdown, adds
`.lore/` to `.gitignore`, and saves settings in `.lore/project.json`. Models live
in `~/.lore/models/bge-small`, shared across projects. The default index is
`.lore/index.sqlite`; source documents are unchanged. Later indexing and searches
run locally. Use `lore init --offline` to require already-provisioned model assets.
A separate `lore model install` can provision the shared model in advance.

Commands from the project or its subdirectories discover saved settings. To use
the index from elsewhere, pass `--index-dir /path/to/project/.lore` to each command.
Each index belongs to one project. To save custom paths at initialization:

```sh
lore --index-dir /path/to/index --model-dir /path/to/models/bge-small init /path/to/project
```

`--threads N` and `init --content` are also remembered. Explicit flags override
saved settings. Existing valid indexes work without a settings file; running
`init` adopts them. The lower-level `index ROOT` command remains available for
manual indexing, but does not save settings or manage ignore rules.

For automatic AI client registration, run `lore setup codex` or
`lore setup claude`; see [MCP setup](mcp.md).

## Search

```sh
lore brief "How do I configure this project?"
lore brief "Which environment variables are required?" --with-guidance --json
lore brief "APP_PORT" --mode keyword --scope docs --json
```

`brief` returns ranked source passages. `--with-guidance` adds suggested follow-up
reads. `--scope` limits results to a file or directory inside the indexed project.
Keyword mode works without model files once an index exists.

Use `--json` for structured output. The default limit is five files and an
estimated 2,000 tokens, including JSON metadata; adjust it with `--max-files` and
`--max-tokens`. Relevance scores describe matches, not whether the results contain
everything needed to answer the question.

## Read the source

Replace `docs/configuration.md` with a path returned by your search:

```sh
lore outline docs/configuration.md --json
lore read docs/configuration.md --json
```

`outline` lists the file's headings and exact anchors. To read one section, use
an anchor from that output. For example, if it includes `settings`:

```sh
lore read 'docs/configuration.md#settings' --context --json
```

This reads the section and its child headings. `--context` also includes
introductory text from enclosing headings. Reads verify that the source still
matches the index.

Long reads are paginated. When `has_more` is `true`, copy `next_cursor` into the
next call, keeping the same target and context flag:

```sh
lore read 'docs/configuration.md#settings' --context --json --cursor 'RETURNED_CURSOR'
```

Continue until `has_more` is `false`. After refreshing a changed index, restart
the read without the old cursor. Outline and reading-plan pages also have their
own cursors; use each with the command that returned it.

## Refresh after edits

```sh
lore status --check --json
lore init
```

The status report identifies added, changed, removed, or unreadable documents.
Initialization refreshes with saved paths and options, reusing cached embeddings
where possible. There is no automatic file watcher. Existing initialized indexes
remain initialized when stale; inspect `needs_reindex` before relying on results.
Before initialization, `status` reports `initialized: false` and
`state: "not_initialized"` without creating a database. Search and reading require
an initialized index.

Lore reads UTF-8 `.md` and `.markdown` files up to 2 MiB each. It respects
`.gitignore` and `.loreignore`, skips build/cache directories, and does not follow
symlinks. Refresh after changing ignore rules too.

See [operations](operations.md) for storage and shared models,
[MCP setup](mcp.md) for coding agents, or [troubleshooting](troubleshooting.md)
when a command fails.
