# Usage

Lore finds passages in Markdown documentation and returns their source locations.
Start with a question, then read the relevant files or sections.

## Index a project

First [install Lore](../README.md#install), then run:

```sh
cd /path/to/project
lore model install
lore index .
```

`model install` downloads the search model (~67 MB). Indexing creates
`.lore/index.sqlite` and leaves your documents unchanged. Later indexing and
searches run locally.

Run the following examples from that same directory. To use the index from
elsewhere, pass `--index-dir /path/to/project/.lore` to each Lore command.
Each index belongs to one project.

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
lore index .
```

The status report identifies added, changed, removed, or unreadable documents.
Indexing refreshes changed content and reuses cached embeddings where possible.
There is no automatic file watcher.

Lore reads UTF-8 `.md` and `.markdown` files up to 2 MiB each. It respects
`.gitignore` and `.loreignore`, skips build/cache directories, and does not follow
symlinks. Refresh after changing ignore rules too.

See [operations](operations.md) for storage and shared models,
[MCP setup](mcp.md) for coding agents, or [troubleshooting](troubleshooting.md)
when a command fails.
