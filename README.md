# Lore

Search Markdown documentation in plain language. Lore returns relevant source
passages with file paths and section references.

Use it from the terminal, as a Rust library, or through MCP in a coding agent.
It runs a small model locally and works offline after setup.

## Install

Requires Rust 1.88+ and a C/C++ toolchain. Tested on Linux x86-64.

```sh
cargo install lore-docs --locked
```

The package is `lore-docs`; the command is `lore`. Add `$CARGO_HOME/bin`
(usually `~/.cargo/bin`) to your `PATH`.

To install from this checkout, run `cargo install --path . --locked`.
The Bash helper `./dev.sh install` is also available for development.

## Use from the terminal

```sh
# Download the search model once (~67 MB).
lore model install

# Index a project's Markdown files.
lore index /path/to/project

# Search the docs.
lore brief "How do I configure this project?"
```

Run these commands from the same directory. Lore stores its index and models
in `.lore/`; use `--index-dir PATH` for a separate index.

Use a file path returned by the search to explore its headings or read its text:

```sh
lore outline README.md
lore read README.md --json
```

Add `--json` to `brief` for structured results. After editing docs, rerun
`lore index /path/to/project` to refresh the index.

## Use with AI agents

Give your agent the [setup guide](docs/agents.md), or paste this request:

```text
Set up Lore for this project using:
https://raw.githubusercontent.com/BeepBoopBit/lore-docs/main/docs/agents.md
Install lore-docs with Cargo, prepare the index, verify it, and run a search.
Connect it through MCP if this client supports local MCP servers.
```

Agents can use JSON CLI output or the read-only MCP tools. See
[MCP setup](docs/mcp.md) for Codex and other clients.

## Development

```sh
./dev.sh doctor       # Check tools and dependencies.
./dev.sh develop      # Fetch locked Rust dependencies.
./dev.sh run --help   # Run this checkout.
./dev.sh test         # Run Rust and installer tests.
./dev.sh check        # Check formatting and lint.
./dev.sh fmt          # Format Rust code.
./dev.sh build        # Build the release binary.
```

`run` forwards arguments to Lore and keeps your working directory. `test` with
arguments forwards them to Cargo and runs only Rust tests. Installer tests need
Python 3; formatting and lint checks need rustfmt and Clippy.

Run `./dev.sh` for help. The older `install --prod` form still works.

## Guides

- [AI agents](docs/agents.md) — install, verify setup, search, and read sources.
- [Usage](docs/usage.md) — index, search, and read documentation.
- [MCP setup](docs/mcp.md) — connect coding agents and multiple projects.
- [Operations](docs/operations.md) — refresh, move, back up, and clean up state.
- [Troubleshooting](docs/troubleshooting.md) — resolve common errors.
- [Development](docs/development.md) — tools, tests, and code layout.
- [Publishing](docs/publishing.md) — validate and release the Cargo package.

## License

[MIT](LICENSE) © 2026 beepboopbit.
