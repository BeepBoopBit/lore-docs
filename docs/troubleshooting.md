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

## Missing model assets or checksum mismatch

Install or repair the exact model directory used by the failing command:

```sh
lore --model-dir /path/to/models/bge-small model install
```

Valid assets are reused; missing or damaged ones are downloaded. In an offline
environment, copy a complete provisioned directory and pass `--model-dir`.
For helper-managed connections, the default is `.lore/models/bge-small` in the
Lore checkout, not inside the individual MCP index.

## No index, wrong project, or stale source

| Message or symptom | Action |
| --- | --- |
| `No Lore index` or no indexed root | Create the index with `lore --index-dir /path/to/index --model-dir /path/to/models/bge-small index /path/to/project`. |
| `needs_reindex: true` or a source hash mismatch | Refresh that same index after editing documents or ignore rules. |
| Source root unavailable after a move | Use `index /new/path --relocate` with the existing index/model paths. |
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

Confirm the configured executable exists and the index was prepared before the
client started it. Inspect the client's server stderr for the actual error.
`lore mcp` waits for protocol input when started directly in a terminal.

For Codex, `codex mcp get CONNECTION_NAME --json` shows the saved executable and
arguments. Use absolute paths: an agent client may launch Lore outside the project
directory. Installing the binary alone does not register an MCP server.

After setup or reinstall, start a new Codex session. If setup reports an existing
entry, use `refresh` to update documents or `setup --replace` to change the
registration. Keep custom `--name`, `--index-dir`, and `--model-dir` values
consistent across commands.

Ask the agent to call `lore_status` to verify the project. A `usage` source profile
is available only when the server was started with `--usage-config`; otherwise
use the default profile. See [MCP setup](mcp.md).
