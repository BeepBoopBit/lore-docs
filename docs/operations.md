# Operations

Lore needs its executable, an index, and the original Markdown documents.
Indexing and hybrid/semantic searches also need the local model files.

## Know which index you are using

| Workflow | Default index | Default search model directory |
| --- | --- | --- |
| Terminal commands | `.lore/index.sqlite` in the current directory | `.lore/models/bge-small` in that directory |
| `dev.sh setup` | `.lore/mcp/<connection-name>/index.sqlite` in the Lore checkout | `.lore/models/bge-small` in the Lore checkout |

For direct CLI commands, `--index-dir` selects the state directory.
Without `--model-dir`, models are loaded from `models/bge-small` inside that
directory. When querying an MCP index from the CLI, supply the helper's shared
model directory for hybrid/semantic searches.

## Check and refresh

For a terminal index, run from the directory where you created it:

```sh
lore status --check --json
lore index /path/to/project
```

For a helper-managed connection, run from the Lore checkout:

```sh
lore --index-dir .lore/mcp/lore-project status --check --json
./dev.sh refresh /path/to/project --name lore-project
```

Use the name chosen during setup, and repeat any custom index/model paths.
`status --check` reports freshness without loading a model. Inspect
`root_available`, `needs_reindex`, and `source_check`; a successful exit alone
does not mean the sources are current. Plain `status` does not scan for edits.

`needs_content_index` concerns optional experimental content scoring. It does
not need to be `false` for ordinary search.

## Update Lore

For a Cargo installation:

```sh
cargo install lore-docs --locked
```

For an installation made with `dev.sh`, run from the updated checkout:

```sh
./dev.sh reinstall
```

Use the same installer for updates. Both commands build and replace the installed
executable. Restart MCP clients to load the new executable.
Check index status and refresh if the new version requests reindexing.

## Move a project

An index records its project's absolute root. After moving that project, update
the existing index explicitly:

```sh
lore --index-dir /path/to/existing-index --model-dir /path/to/models/bge-small index /new/path/to/project --relocate
```

Relocation refreshes the index and reuses compatible embeddings. Begin new reads
without old continuation cursors. For a different project, create a separate
index instead.

If the Lore checkout, executable, or configured index path moved, update the MCP
registration with `dev.sh setup ... --replace` or edit your client's configuration.

## Work offline

Run `lore model install` while online, or copy the complete provisioned
`models/bge-small` directory from another installation. Use `--model-dir` to
select the copied directory. Lore verifies the pinned asset checksums.

After provisioning, indexing and querying use local files. `read`, `outline`,
`plan`, `status`, and keyword searches can use an existing index without a model.
Building Lore may still need dependency downloads. MCP clients have their own
network behavior; Lore's local execution does not determine that behavior.

## Back up or clean up

Back up source documents separately. To copy an index, stop processes using it
and copy its entire directory, including any SQLite WAL/SHM sidecar files.
Shared model files only need one copy.

| Item | Effect of removing it |
| --- | --- |
| `target/` | Removes local builds; Cargo can rebuild them. Clients using a binary there will need that rebuild. |
| An index's SQLite files | Removes indexed state; recreate it from the source documents. |
| A model directory | Requires restoring or provisioning those assets before indexing or model-based search. |
| The entire `.lore/` directory | Removes all state stored there, potentially including shared models, multiple indexes, and any manually saved artifacts. |

Stop processes using an index before removing its files. Review `.lore/` before
deleting it; locally saved snapshots or reports may not be reproducible.

Remove a Cargo installation with `cargo uninstall lore-docs`. For an installation
made with the checkout helper, use `./dev.sh uninstall`. Both preserve indexes,
models, and Codex registrations. Remove an unused Codex connection separately
with `codex mcp remove CONNECTION_NAME`.
