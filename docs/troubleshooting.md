# Troubleshooting

Check the installed command with `lore --version`. From a Lore checkout,
`./dev.sh doctor` also checks development tools. For index problems, start with:

```sh
lore --index-dir /path/to/index status --check --json
```

Confirm that `root` points to the intended project. Helper-managed indexes are
under the Lore checkout's `.lore/mcp/<connection-name>` directory.

## `lore: command not found`

Install with `cargo install lore-docs --locked` (or `cargo install --path . --locked`
from a checkout). If you used the Bash helper, update with
`./dev.sh reinstall`. Add the installation directory to your shell's `PATH`:

```sh
export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
lore --version
```

If you set `LORE_DEV_INSTALL_DIR`, use that directory instead. The helper does
not edit shell configuration. `./dev.sh run --help` runs the checkout directly.

If Cargo reports that `lore` already exists from a `dev.sh` installation, remove
that copy with `./dev.sh uninstall`, then rerun `cargo install lore-docs --locked`.

## Initialization or client setup

Run `lore init`, or `lore setup codex` / `lore setup claude` for a complete client
connection. A `not_initialized` response can also be resolved through `lore_init`
when using MCP. `--offline` never downloads missing assets; omit it while online
or supply a complete `--model-dir`.

Setup preserves other servers and instructions. A conflicting connection needs
`--replace` or a distinct `--name`. Malformed client configuration and incomplete
managed instruction blocks must be corrected before setup can merge changes.
After setup, reload the client, accept project trust if required, and call
`lore_status`. Verification confirms the local MCP process; it does not reload an
existing client session. See [MCP setup](mcp.md).

## Missing model assets or checksum mismatch

Install or repair the exact model directory used by the failing command:

```sh
lore --model-dir /path/to/models/bge-small model install
```

Valid assets are reused; missing or damaged ones are downloaded. In an offline
environment, copy a complete provisioned directory and pass `--model-dir`.
Both terminal commands and helper-managed connections default to
`~/.lore/models/bge-small`, independent of the index directory. If you already
downloaded models into a project's `.lore/models/`, follow
[reusing existing downloads](operations.md#reuse-existing-model-downloads).

## No index, wrong project, or stale source

| Message or symptom | Action |
| --- | --- |
| `not_initialized`, `No Lore index`, or no indexed root | Run `lore init /path/to/project`, with any custom `--index-dir` and `--model-dir` flags. MCP clients can call `lore_init`. |
| `needs_reindex: true` or a source hash mismatch | Run `lore init` or call `lore_init` to refresh saved preparation. |
| Source root unavailable or saved configuration belongs to another root after a move | Follow [project relocation](operations.md#move-a-project), including moving old settings aside and saving new paths. |
| Index belongs to another repository | Select a separate `--index-dir` for that repository. |
| Index created by a newer Lore | Use a compatible Lore version or create a fresh index in another directory. |

For helper-managed connections, use `./dev.sh refresh /path/to/project`, repeating
the name and paths from setup. See [operations](operations.md) for complete
refresh and relocation examples.

## Empty results or missing sections

Check `documents` in the status report and confirm the file is UTF-8 Markdown,
at most 2 MiB, and not excluded by `.gitignore` or `.loreignore`. Symlinks are
not indexed. An overly narrow `--scope` can also exclude the file.

Try a specific term with `--mode keyword`, or pass a known indexed file path to
`brief`. Use `outline FILE --json` to find exact heading anchors. Anchors must
exist; use the returned value rather than guessing it.

If a page cannot fit its metadata, increase `--max-tokens`. If a cursor is stale
or belongs to another target, restart without it. Keep the same target and
`--context` setting while continuing a read.

## MCP connection fails

Confirm the configured executable exists and the configured root is accessible.
The server may start before indexing; call `lore_status` and then `lore_init`
when needed. Inspect the client's server stderr for the actual error.
`lore mcp` waits for protocol input when started directly in a terminal.

For Codex, `codex mcp get CONNECTION_NAME --json` shows the saved executable and
arguments. Use absolute paths: an agent client may launch Lore outside the project
directory. Installing the binary alone does not register an MCP server.

After setup or reinstall, reload the client or start a new session. Native setup
uses `lore init` to refresh documents and `lore setup codex --replace` (or Claude)
to replace a conflicting registration. A local setup probe cannot confirm that
an already-running client has reloaded or trusted the configuration.

For the older checkout helper, use `./dev.sh refresh` and repeat the original
`--name`, `--index-dir`, and `--model-dir` options; it does not save a refresh profile.

Ask the agent to call `lore_status` to verify the project. A `usage` source profile
is available only when the server was started with `--usage-config`; otherwise
use the default profile. See [MCP setup](mcp.md).
