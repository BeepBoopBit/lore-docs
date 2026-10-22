# Development

Use `./dev.sh` from the checkout, or call it by its full path. With no arguments,
it shows help. The command names follow the same conventions as the other
projects, using Cargo for Lore's Rust code.

## Requirements

- Bash, Rust 1.88+, and a native C/C++ toolchain for builds.
- rustfmt and Clippy for formatting and lint checks.
- Python 3.9+ for the installer tests.
- Codex CLI for the older `./dev.sh setup` helper; optional for builds and tests.
  Native `lore setup codex/claude` writes client configuration directly.

Linux x86-64 is the tested platform. Start with:

```sh
./dev.sh doctor
./dev.sh develop
```

`doctor` reports tool availability, the Rust version, and manifest/lockfile
presence. `develop` fetches the dependencies pinned by `Cargo.lock`. The helper
does not install system tools or change your shell settings.

## Commands

| Command | Purpose |
| --- | --- |
| `install` | Build and install a release binary for your user. |
| `reinstall` | Rebuild and replace the installed copy; also works before the first install. |
| `uninstall` | Remove the installed binary, keeping local state. |
| `run ...` | Build a debug binary and forward arguments to it. |
| `check` | Run formatting checks and Clippy without rewriting source. |
| `fmt` | Format the Rust source. |
| `test` | Run Rust tests, followed by installer tests if they pass. |
| `test ...` | Forward arguments to Cargo and run only Rust tests. |
| `build` | Build `target/release/lore`. |
| `setup PATH` / `refresh PATH` | Prepare or refresh a checkout-managed [MCP connection](mcp.md#use-the-checkout-helper). |

```sh
./dev.sh run --help
./dev.sh test
./dev.sh test --test retrieval
./dev.sh check
./dev.sh build
```

Builds, tests, and code checks run in the checkout. `run` keeps your calling
directory, so relative project paths and the `.lore/` index resolve there. Models
default to `~/.lore/models/`, shared with installed Lore and the setup helper.
Build, fetch, test, and Clippy commands use `--locked`; update `Cargo.lock`
deliberately when changing dependencies.

Native `lore init` and `lore setup codex/claude` work from the installed executable;
they save settings in the target project's `.lore/project.json`. The older helper
uses separate `.lore/mcp/` indexes in this checkout and requires the same flags
on each refresh. Use the [native workflow](mcp.md#automatic-client-setup) for
ordinary project onboarding.

## Test the helper

```sh
python3 -B scripts/test_dev_installer.py
bash -n dev.sh scripts/verify-offline.sh
```

The Python tests use temporary directories and stand-in Cargo, Lore, and Codex
executables. They cover installation failures, argument forwarding, paths, and
MCP registration without modifying real installations or Codex settings.

`scripts/verify-offline.sh` is an optional Linux check requiring `unshare`,
provisioned models, and a corpus. Its example queries target the original
evaluation corpus; it is separate from the normal test suite.

## Paths and installation

`LORE_DEV_INSTALL_DIR` overrides the binary directory, normally `$CARGO_HOME/bin`
or `~/.cargo/bin`. `CARGO_TARGET_DIR` overrides build output. Both overrides must
be absolute paths. `install` refuses to overwrite an existing binary; use
`reinstall` after code changes. The older `install --prod` form remains accepted.

For a Cargo-managed local installation, use `cargo install --path . --locked`.
Use the same installer for updates and removal; see [operations](operations.md).
The Cargo package is `lore-docs`, while the executable and library crate are named
`lore`. See [publishing](publishing.md) for release steps.

GitHub Actions runs formatting, Clippy, tests, a Rust 1.88 build, and packaging
checks on pushes to `main` and pull requests. Matching version tags also publish
the crate after the checks pass.

## Code map

| Location | Responsibility |
| --- | --- |
| `src/main.rs`, `src/lib.rs` | CLI entry point and library exports. |
| `src/cli_help.rs` | Command examples and onboarding help. |
| `src/onboarding.rs`, `src/project.rs` | Saved project settings, initialization, safe client configuration, and managed instructions. |
| `src/index.rs`, `src/markdown.rs`, `src/embedding.rs` | Document parsing, SQLite indexes, and local embeddings. |
| `src/search.rs`, `src/retrieval.rs`, `src/selection.rs` | Retrieval, ranking, and passage selection. |
| `src/reading.rs`, `src/context.rs`, `src/mcp.rs` | Source reads, enclosing context, and MCP tools. |
| `tests/` | Retrieval, onboarding, model-path, and MCP integration tests. |
| `scripts/test_dev_installer.py` | Dev helper and installation tests. |
