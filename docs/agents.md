# AI agents

Install the Cargo package **`lore-docs`** and run the **`lore`** command.
Agents can search local Markdown through shell commands or MCP tools.

## Install and prepare a project

The agent needs shell access, Rust 1.88+, and a C/C++ toolchain. Linux x86-64 is
the tested platform. Installation and the first model download need network
access; indexing and searches then run locally. No API key or crates.io token is
needed to install or use Lore.

Run this in one shell, replacing the project path:

```sh
cargo install lore-docs --locked
export PATH="${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}/bin:$PATH"
lore --version

cd "/absolute/path/to/project"
lore model install
lore index .
lore status --check --json
```

If Cargo's install directory is customized through its configuration, use that
directory's `bin` in `PATH`. Keep `.lore/` in the project's `.gitignore`; it holds
the index and downloaded models. Each project needs its own index.

Inspect the status JSON before reporting setup complete:

| Field | Expected value |
| --- | --- |
| `root` | The intended project's absolute path. |
| `root_available` | `true`. |
| `needs_reindex` | `false`. |
| `documents` | Greater than zero for a project containing indexable Markdown. |

Then run a search about that project's documentation. If setup fails or no
documents were indexed, use [troubleshooting](troubleshooting.md).

## Use MCP tools

Follow [MCP setup](mcp.md) to connect the installed executable to the agent's
client. Use absolute executable and index paths. The MCP process needs access
to the original project files, index, and models on the same machine.

1. Call `lore_status` with `{}` to check the project and source freshness.
2. Call `lore_brief` with `{"query": "How do I configure this project?"}`.
3. Read the returned `guidance.next_reads`, then call `lore_read` using a
   suggestion's `read_target` as `target` and its `context` value.
4. Follow `next_cursor` while `has_more` is true, keeping the same target and
   context. Cite the source paths and sections that support the answer.

Read the tool's `structuredContent` JSON; its text content repeats that payload.
`lore_brief` returns evidence under `brief` and follow-up suggestions under
`guidance`. If there are no suggestions, read a relevant path from `brief.files`.
Use `lore_outline` to discover exact anchors when needed.

If sources are stale, refresh with `lore index .` from the project directory,
then restart affected reads without old cursors. MCP tools query existing
indexes; installation and refresh require shell access. An agent with only MCP
access needs the host to perform those setup operations.

## Use shell commands

An agent with shell access can use JSON output directly. Shell calls may start
in different directories, so pass the absolute index path on each call. Keep
Cargo's bin directory in `PATH`, or use the full path from `command -v lore`:

```sh
lore --index-dir "/absolute/path/to/project/.lore" status --check --json
lore --index-dir "/absolute/path/to/project/.lore" brief "How do I configure this project?" --with-guidance --json
```

Follow returned read targets using `lore read TARGET --json`, adding `--context`
when the suggestion requests it. Keep the same `--index-dir` on reads, outlines,
and refreshes. For example, refresh from any directory with:

```sh
lore --index-dir "/absolute/path/to/project/.lore" index "/absolute/path/to/project"
```

See [usage](usage.md) for reading and pagination examples. Separate model
directories also need the matching `--model-dir` on indexing and search commands.

## Project instructions

Add this to the project's existing agent instructions, adjusting the index path:

```text
Use Lore for questions about this project's Markdown documentation.
Check lore_status for the correct project and fresh sources. Search with
lore_brief, then use lore_read on returned targets before answering.
Preserve target/context across read cursors and cite the supporting paths and
sections. Refresh the index after documentation changes using the shell.
If MCP tools are unavailable, use lore --index-dir "/absolute/path/to/project/.lore"
with status --check --json, brief --with-guidance --json, and read --json.
```
