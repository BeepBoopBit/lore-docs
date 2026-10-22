# MCP setup

Lore binds one project to an MCP stdio server. Search and reading require an
initialized index; status and initialization work before an index exists.
The client starts the process and communicates through stdin/stdout; there is
no HTTP server or port to open.

For an agent doing the installation, start with the [AI agent guide](agents.md).
The published `lore-docs` package supplies the executable used in these examples.

## Automatic client setup

Run inside the project you want to search:

```sh
lore setup codex
# Or:
lore setup claude
```

Either command runs initialization, updates `.gitignore`, verifies the local MCP
server, and adds a managed usage block to `AGENTS.md` or `CLAUDE.md`. Codex project
settings are merged into `.codex/config.toml`; Claude Code project settings are
merged into `.mcp.json`. Other settings, servers, and instructions are preserved.
Repeating identical setup is safe. Use `--replace` for a conflicting server entry,
`--name NAME` for a distinct entry, or `--global` for user configuration.
The project server name defaults to `lore`. Global setup uses a project-specific
name derived from the root and writes `$CODEX_HOME/config.toml` (default
`~/.codex/config.toml`) or `$CLAUDE_CONFIG_DIR/.claude.json` (default
`~/.claude.json`). Managed instructions stay in the project even with `--global`.

For example:

```sh
lore setup codex /path/to/project --offline --name project-docs
lore setup claude /path/to/project --global
```

Use `--offline` to prevent model downloads. Custom `--index-dir`, `--model-dir`,
and initialization options are remembered in `.lore/project.json`. Reload your
client or start a new session after registration; project trust may be required.
The verification checks the local process, not whether an already-running client
has reloaded its configuration.

## Initialize through MCP

A manually configured server can bind a project before it has an index:

```sh
lore --index-dir /absolute/path/to/project/.lore mcp --root /absolute/path/to/project
```

`lore_status` returns `initialized: false` and `state: "not_initialized"` without
creating a database. Call `lore_init` with `{}` to initialize, or
`{"offline": true}` to require cached assets. Initialization adds the ignore rule,
prepares the model, indexes documents, and saves settings. The project root is
fixed by the server configuration; tool arguments cannot redirect initialization.

Only `lore_init` performs setup writes and may download pinned model assets.
Search and reading tools remain read-only. They return a `not_initialized` error
until an index is ready. Existing valid indexes are recognized without a marker.
After changes, rerun `lore init` or call `lore_init`; a stale index stays initialized.

## Connect Codex

For manual registration instead of `lore setup codex`, install Codex CLI and
[install Lore](../README.md#install). From your project:

```sh
cd "/path/to/project"
lore init
lore status --check --json
codex mcp add lore-project -- "$(command -v lore)" \
  --index-dir "$PWD/.lore" mcp --root "$PWD"
codex mcp get lore-project --json
```

This registers an absolute executable and index path, so the client can start
Lore from any directory. Models default to `~/.lore/models/bge-small`, shared
across all projects for the same user. Use a distinct connection name for each
project.

The final command shows the saved registration. Check its executable and index
paths before starting the agent session.

Start a new Codex session and check `/mcp`. Ask the agent to call `lore_status`.
Its `root` should be your project's absolute path, `root_available` should be
`true`, and `needs_reindex` should be `false`.

After editing documents, run `lore init` from the same project directory.
Running MCP processes see the refreshed index on their next call. To change
the registration, run `codex mcp remove lore-project`, add it again with the
new paths, and restart the Codex session.

## Use the checkout helper

The installed `lore setup` commands above need no checkout. The older development
helper offers a separate Codex workflow from the Lore checkout:

```sh
./dev.sh setup /path/to/project --name lore-project
```

Setup prepares the model, indexes the project, checks its status, and registers
the connection. Start a new Codex session and check `/mcp`.

Ask the agent to call `lore_status`. Its `root` should be your project's absolute
path, `root_available` should be `true`, and `needs_reindex` should be `false`.

Without `--name`, the connection is named `lore-<project-directory-name>`.
Use distinct names for projects with the same directory name.

### Refresh or change a helper connection

```sh
./dev.sh refresh /path/to/project --name lore-project
```

Refresh uses existing model assets and updates the index. Running MCP processes
see the new index on their next call.

To change an existing registration, run setup with `--replace`, then restart
the Codex session:

```sh
./dev.sh setup /path/to/project --name lore-project --replace
```

Setup and refresh accept `--index-dir` and `--model-dir`. Relative paths resolve
from your current directory. Repeat any custom name and paths when refreshing;
the helper does not save them as a refresh profile.

By default, indexes live in the Lore checkout at `.lore/mcp/<connection-name>`.
All connections use `~/.lore/models/bge-small`, shared with terminal commands.
Helper-managed indexes are separate from the project-local index created by
`lore init` or native `lore setup`. The helper does not write the native
`.lore/project.json` settings file or manage project instructions.

## Connect another MCP client

Prepare an index and model directory first:

```sh
lore --index-dir /path/to/index --model-dir /path/to/models/bge-small init /path/to/project
```

Add a server entry to your client's MCP configuration, adapting its layout if
the client uses a different format:

```json
{
  "mcpServers": {
    "lore": {
      "command": "/absolute/path/to/lore",
      "args": [
        "--index-dir", "/path/to/index",
        "--model-dir", "/path/to/models/bge-small",
        "mcp", "--root", "/path/to/project"
      ]
    }
  }
}
```

Set `command` to the absolute path of the Lore executable. Set the index and
model paths to the directories prepared above. The executable can be the
installed binary or the release build from `./dev.sh build`.

Reload the client's MCP configuration or restart it, then call `lore_status`
to verify the connected project.

The client should launch the equivalent of:

```sh
lore --index-dir /path/to/index --model-dir /path/to/models/bge-small mcp --root /path/to/project
```

A server started in a terminal waits for protocol input. Startup errors go to
stderr; stdout is reserved for MCP messages.

## Available tools

See the [agent workflow](agents.md#use-mcp-tools) for the search/read sequence and
how to handle structured responses and stale sources.

| Tool | Purpose |
| --- | --- |
| `lore_init` | Initialize or refresh the bound project and its local search model. |
| `lore_status` | Check the configured project and source freshness. |
| `lore_brief` | Search for evidence and suggested follow-up reads. |
| `lore_read` | Read a file or section, optionally with enclosing context. |
| `lore_outline` | List exact heading anchors. |
| `lore_plan` | Show the source sections used for a contextual read. |
| `lore_diagnose` | Explain omitted content and possible follow-up reads. |

Start with `lore_status`, initialize if needed, then use `lore_brief` and follow
returned read targets. Keep the same target and `context` value when continuing
a read cursor. Tool budgets cover Lore's JSON payload; the MCP message and
compatibility text copy add overhead.

Search and reading tools read documentation. The explicit `lore_init` tool can
prepare or refresh this bound project; the CLI and dev helper remain available.

## Optional source preferences

Start `lore mcp` with `--usage-config /path/to/roles.json` to make a `usage`
source profile available. Native `lore setup` does not accept this flag; append
it to the saved server's MCP arguments for a manual configuration. The checkout
helper also accepts `./dev.sh setup ... --usage-config /path/to/roles.json`.
Agents select the profile per query with `source_profile: "usage"`; ordinary
ranking remains the default. For example, save this as `roles.json`, adjusting
the prefix to a directory in your project:

```json
{
  "roles": [
    {"prefix": "docs/guides", "role": "consumer", "weight": 1.1}
  ]
}
```

This preference is intended for consumer documentation. Other MCP clients can
append `--usage-config` and the absolute JSON file path after `mcp` in `args`.

See [troubleshooting](troubleshooting.md) for connection errors and
[operations](operations.md) for updating or moving an installation.
