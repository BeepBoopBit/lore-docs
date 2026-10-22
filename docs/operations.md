# Operations

Lore needs its executable, an index, and the original Markdown documents.
Indexing and hybrid/semantic searches also need the local model files.

## Initialize and refresh saved settings

Run `lore init` from a project to prepare its model, index, and ignore rule.
Settings live in `.lore/project.json`, including custom index/model paths, thread
count, and optional content preparation. Commands from this project or its
subdirectories reuse those settings; explicit flags override them. Repeating
`lore init` refreshes the saved plan. `lore init --offline` requires cached assets.

Existing valid indexes remain usable without a project settings file. Run init to
adopt them and save settings. `status` works before initialization and creates no
index; search/read commands report `not_initialized` until an index is ready.

## Know which index you are using

| Workflow | Default index | Default search model directory |
| --- | --- | --- |
| Native `init`, `setup`, and terminal commands | Saved index, or `.lore/index.sqlite` under the project root | Saved model, or `~/.lore/models/bge-small` |
| `dev.sh setup` | `.lore/mcp/<connection-name>/index.sqlite` in the Lore checkout | `~/.lore/models/bge-small` |

For direct CLI commands, `--index-dir` selects the state directory.
Models are independent of `--index-dir`: project commands reuse a model path
saved by initialization, otherwise `~/.lore/models/bge-small`; `--model-dir`
overrides either. Standalone `model install` uses the home default unless its
own `--model-dir` is supplied; it does not read or save project preferences.
The optional reranker defaults to `~/.lore/models/minilm-reranker`, or
`--reranker-dir` when supplied.
One model installation serves all projects for that user.

## Reuse existing model downloads

Older versions stored models inside the index directory; the checkout helper
used `<Lore checkout>/.lore/models/`. To reuse those downloads without fetching
them again, copy the existing model directories into your home directory:

```sh
mkdir -p "$HOME/.lore/models"
# Run from the project or Lore checkout containing the downloaded models.
if [ ! -e "$HOME/.lore/models/bge-small" ]; then
  cp -R .lore/models/bge-small "$HOME/.lore/models/"
fi
# If you previously installed the optional reranker:
# cp -R .lore/models/minilm-reranker "$HOME/.lore/models/"
lore model install
```

The copy step keeps any model already installed in your home directory.
`model install` verifies the files and only downloads missing or
damaged assets. Existing indexes can use the shared files without reindexing.
Alternatively, keep the old location and pass `--model-dir` to `lore init` once
to save it for the project. For manual indexes without saved settings, repeat
that flag on model-dependent commands. Repeat `--reranker-dir` when needed.

Existing MCP registrations with an explicit `--model-dir` keep using that path.
To use the new default with the checkout helper, rerun
`./dev.sh setup /path/to/project --name CONNECTION_NAME --replace` without a
custom `--model-dir`, then restart the MCP client.

## Check and refresh

For a native project index, run from the project or a subdirectory:

```sh
lore status --check --json
lore init
```

`init` repeats saved preparation, including custom model/index paths and content
settings. MCP clients can call `lore_init` instead. For a manually maintained
index without saved settings, use `lore index ROOT` with its model/index flags.

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

Both the index and `.lore/project.json` record absolute paths. A moved settings
file is rejected to avoid writing to the old project. Stop its MCP processes,
inspect the saved paths, and move `.lore/project.json` aside before relocating
(for example, to an unused `.lore/project.json.before-move`). Keep that copy until
you have checked the new setup. Then use the index's new location explicitly:

```sh
lore --index-dir /path/to/existing-index --model-dir /path/to/models/bge-small index /new/path/to/project --relocate
lore --index-dir /path/to/existing-index --model-dir /path/to/models/bge-small init /new/path/to/project --offline
```

Relocation refreshes the index and reuses compatible embeddings; initialization
saves the new paths and updates ignore rules. Repeat any saved `--content` or
`--threads` settings you want to retain. Begin new reads without old cursors.
For a different project, create a separate index instead.

Rerun `lore setup codex /new/path/to/project --replace` or the Claude equivalent
and reload the client. Global registrations use root-derived names, so remove
the old connection separately. For helper connections, use
`dev.sh setup ... --replace` with the new paths and original connection name.

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
| A project's `.lore/` directory | Removes its saved settings, default index, and any manually saved artifacts or models from older versions. Custom external indexes remain. |
| `~/.lore/models/` | Removes shared models used by all projects; restore or provision them before indexing or model-based search. |

Stop processes using an index before removing its files. Review `.lore/` before
deleting it; locally saved snapshots or reports may not be reproducible.

Remove a Cargo installation with `cargo uninstall lore-docs`. For an installation
made with the checkout helper, use `./dev.sh uninstall`. Both preserve indexes,
models, saved project settings, and client registrations. Remove unused entries
from the appropriate project or user client configuration, and remove the managed
Lore instruction block if no longer needed. Manually registered Codex connections
can also be removed with `codex mcp remove CONNECTION_NAME`.
