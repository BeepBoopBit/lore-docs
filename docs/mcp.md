# MCP setup

Lore serves one indexed project over MCP stdio. The client starts the process
and communicates through stdin/stdout; there is no HTTP server or port to open.

## Connect Codex

Install Codex CLI and [install Lore](../README.md#install). From your project:

```sh
cd /path/to/project
lore model install
lore index .
codex mcp add lore-project -- "$(command -v lore)" \
  --index-dir "$PWD/.lore" mcp
```

This registers an absolute executable and index path, so the client can start
Lore from any directory. Models default to `models/bge-small` inside the index
directory. Use a distinct connection name for each project.

Start a new Codex session and check `/mcp`. Ask the agent to call `lore_status`.
Its `root` should be your project's absolute path, `root_available` should be
`true`, and `needs_reindex` should be `false`.

After editing documents, run `lore index .` from the same project directory.
Running MCP processes see the refreshed index on their next call. To change
the registration, run `codex mcp remove lore-project`, add it again with the
new paths, and restart the Codex session.

## Use the checkout helper

From the Lore checkout, you can automate model setup, indexing, and registration:

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
All connections share `.lore/models/bge-small` there. These are separate from a
terminal index created by running `lore index .` inside another project.

## Connect another MCP client

Prepare an index and model directory first:

```sh
lore --model-dir /path/to/models/bge-small model install
lore --index-dir /path/to/index --model-dir /path/to/models/bge-small index /path/to/project
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
        "mcp"
      ]
    }
  }
}
```

Set `command` to the absolute path of the Lore executable. Set the index and
model paths to the directories prepared above. The executable can be the
installed binary or the release build from `./dev.sh build`.

The client should launch the equivalent of:

```sh
lore --index-dir /path/to/index --model-dir /path/to/models/bge-small mcp
```

A server started in a terminal waits for protocol input. Startup errors go to
stderr; stdout is reserved for MCP messages.

## Available tools

| Tool | Purpose |
| --- | --- |
| `lore_status` | Check the configured project and source freshness. |
| `lore_brief` | Search for evidence and suggested follow-up reads. |
| `lore_read` | Read a file or section, optionally with enclosing context. |
| `lore_outline` | List exact heading anchors. |
| `lore_plan` | Show the source sections used for a contextual read. |
| `lore_diagnose` | Explain omitted content and possible follow-up reads. |

Start with `lore_brief`, then follow returned read targets. Keep the same target
and `context` value when continuing a read cursor. Tool budgets cover Lore's
JSON payload; the MCP message and compatibility text copy add overhead.

The tools read documentation; they do not refresh indexes or install models.
Perform those operations through the CLI or dev helper.

## Optional source preferences

Pass `--usage-config /path/to/roles.json` to setup to make a `usage` source profile
available. Agents select it per query with `source_profile: "usage"`; ordinary
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
