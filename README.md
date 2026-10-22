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
# Run inside the project you want to search.
# Prepare the model, index Markdown, and update .gitignore.
lore init

# Search the docs.
lore brief "How do I configure this project?"
```

Initialization saves project settings in `.lore/project.json`; commands from the
project or its subdirectories reuse them. Repeat `lore init` to refresh. Each
project's index lives in `.lore/`; use `--index-dir PATH` during initialization
for a separate index. Models live in `~/.lore/models/` and are shared across
projects. Initialization downloads missing assets once; use `--offline` to
require cached assets, or `--model-dir PATH` to save a custom model directory.
The local search model is separate from your AI client's model; no API key is needed.

Already have models in a project's `.lore/models/`? See
[reusing existing downloads](docs/operations.md#reuse-existing-model-downloads).

Use a file path returned by the search to explore its headings or read its text:

```sh
lore outline README.md
lore read README.md --json
```

Add `--json` to `brief` for structured results. After editing docs, rerun
`lore init` to refresh using saved settings. Run `lore --help` or
`lore <command> --help` for options and examples.

## Use with AI agents

Initialize and connect your current project in one command:

```sh
lore setup codex
# Or:
lore setup claude
```

Setup verifies the local MCP server, preserves existing client settings, and adds
a managed instruction block to `AGENTS.md` or `CLAUDE.md`. Project configuration
uses `.codex/config.toml` or `.mcp.json`. Use `--global` for user configuration,
`--name NAME` for a distinct server entry, or `--replace` for a conflicting entry.
Reload the client after setup and check `lore_status`. See [MCP setup](docs/mcp.md)
for configuration paths, trust, and initialization through MCP.

You can also give your agent the [setup guide](docs/agents.md), or paste this request:

```text
Set up Lore for this project using:
https://raw.githubusercontent.com/BeepBoopBit/lore-docs/main/docs/agents.md
Install lore-docs with Cargo. Run lore setup codex or lore setup claude for
this client, reload its MCP configuration, and verify lore_status and a search.
For shell-only use, run lore init and verify lore status --check --json.
```

Agents can use JSON CLI output or the MCP search and reading tools. See
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
