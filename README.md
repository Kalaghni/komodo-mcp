# komodo-mcp

An MCP server for [Komodo](https://komo.do), written in Rust. It runs on your machine as a
stdio server that your MCP client (Claude Code, Claude Desktop, or any other) launches, and
talks to Komodo Core over its HTTPS API with an API key.

It's **read-only by default**. Deploys and restarts, and configuration changes, are separate
tiers you switch on with an environment variable. Tools for tiers that aren't enabled are
never offered to the model, so it can't call them.

## Tools

| Tier | Tool | What it does |
|---|---|---|
| read | `komodo_overview` | Core version plus a health summary for every resource type. A good first call. |
| read | `komodo_list` | List servers, swarms, stacks, deployments, builds, builders, repos, procedures, actions, resource syncs or alerters, with state. Filter by names, tags or search terms. |
| read | `komodo_get` | One resource's full config and info. |
| read | `komodo_server_stats` | Live CPU / memory / disk / network, plus optional system info. |
| read | `komodo_containers` | Every container on a server. |
| read | `komodo_stack_services` | A stack's services and their containers. |
| read | `komodo_stack_logs`, `komodo_deployment_logs`, `komodo_container_logs`, `komodo_swarm_service_logs` | Tail logs, or grep the whole log with `search` (AND/OR, invert). |
| read | `komodo_swarm` | Swarm info, nodes, services, tasks, stacks, configs, secrets (metadata only) or networks. |
| read | `komodo_updates`, `komodo_get_update` | Activity history (who deployed what, success or failure) and the full stage-by-stage logs of one run. |
| read | `komodo_alerts` | Open (or all) alerts. |
| read | `komodo_read` | Any other Komodo read request, e.g. `InspectSwarmService`, `ListImages`, `GetHistoricalServerStats`. |
| execute | `komodo_stack_action` | deploy, deploy_if_changed, pull, start, restart, stop, pause, unpause, destroy. |
| execute | `komodo_deployment_action` | The same set for Deployments. |
| execute | `komodo_run` | Run a Build, Procedure, Action (with args), Resource Sync, or Repo clone / pull / build. |
| execute | `komodo_execute` | Any other execute request, e.g. `UpdateSwarmNode` to drain a node. |
| write | `komodo_write` | Any write request: create / update / rename resources, change config and variables. |

Execute tools wait for the operation to finish (polling the Komodo Update) and return the
logs of each stage, so the model sees whether a deploy actually worked. Pass `wait: false`
to return straight away. The wait is capped by `KOMODO_MCP_WAIT_TIMEOUT_SECS`.

### Guard rails

- **Destructive requests are refused** unless `KOMODO_MCP_ALLOW_DESTRUCTIVE=true`. That's any
  request whose name contains Destroy, Delete, Remove, Prune, Rotate or Clear, in every tool,
  including the generic `komodo_execute` and `komodo_write`.
- **Credential, user and permission management is always refused**: anything touching
  ApiKey, OnboardingKey, User, Permission or Login. Do those in the Komodo UI.
- Request names must be plain PascalCase, so model input can't build odd URLs.
- Output is capped at `KOMODO_MCP_MAX_OUTPUT_CHARS`. Logs keep the newest lines, and
  JSON keeps the start, with a note saying how to narrow the request.
- The real limit is the API key's own Komodo permissions. See below.

## 1. Create an API key in Komodo

Create a dedicated **Service User** for this rather than using your own key, then give it
only the permissions you want the model to have. The key can never do more than that user can,
whatever `KOMODO_MCP_MODE` says.

1. In Komodo, create a Service User (for example `mcp`) and an API key for it from the
   Settings page. Copy the key and secret; the secret is only shown once.
2. Grant permissions to the Service User, directly or through a User Group:
   - For `read` mode, **Read** on the resources it should see (or on all resources).
   - For `execute` mode, **Execute** on the stacks and deployments it may deploy or restart.
   - For `write` mode, **Write** on what it may reconfigure.

For `read` mode, a Service User with Read permission only is the cleanest setup. Even a bug
in this server couldn't change anything then.

## 2. Build and install

You need Rust 1.88 or newer ([rustup.rs](https://rustup.rs)). On Windows, the default MSVC
toolchain works; `ring` needs the C compiler that comes with the Visual Studio Build Tools,
which rustup's installer offers to set up.

```sh
cargo install --path .
# installs komodo-mcp into ~/.cargo/bin (Windows: %USERPROFILE%\.cargo\bin)
```

A prebuilt Linux x86_64 binary is included in this bundle as `bin/komodo-mcp-linux-x86_64`.

Then test the connection:

```sh
export KOMODO_ADDRESS=https://komodo.example.com
export KOMODO_API_KEY=K-...
export KOMODO_API_SECRET=S-...
komodo-mcp --check
# Connected to https://komodo.example.com — Komodo Core 2.x.x
# API key works: can see 3 server(s).
# Mode: read. Tools enabled: komodo_alerts, komodo_container_logs, ...
```

PowerShell uses `$env:KOMODO_ADDRESS = "https://komodo.example.com"` and so on.

## 3. Add it to your MCP client

### Claude Code

```sh
claude mcp add --scope user --transport stdio \
  --env KOMODO_ADDRESS=https://komodo.example.com \
  --env KOMODO_API_KEY=K-... \
  --env KOMODO_API_SECRET=S-... \
  --env KOMODO_MCP_MODE=read \
  komodo -- komodo-mcp
```

Run `/mcp` inside Claude Code to confirm it's connected. Use the full path to the binary if
`~/.cargo/bin` isn't on your PATH.

To share the setup with the team without committing secrets, put it in the project's
`.mcp.json` and reference environment variables, which Claude Code expands:

```json
{
  "mcpServers": {
    "komodo": {
      "type": "stdio",
      "command": "komodo-mcp",
      "env": {
        "KOMODO_ADDRESS": "https://komodo.example.com",
        "KOMODO_API_KEY": "${KOMODO_API_KEY}",
        "KOMODO_API_SECRET": "${KOMODO_API_SECRET}",
        "KOMODO_MCP_MODE": "read"
      }
    }
  }
}
```

### Claude Desktop and other clients

Add an entry to the client's MCP config (for Claude Desktop, `claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "komodo": {
      "command": "/Users/you/.cargo/bin/komodo-mcp",
      "env": {
        "KOMODO_ADDRESS": "https://komodo.example.com",
        "KOMODO_API_KEY": "K-...",
        "KOMODO_API_SECRET_FILE": "/Users/you/.config/komodo-mcp/secret",
        "KOMODO_MCP_MODE": "read"
      }
    }
  }
}
```

On Windows use a path like `C:\\Users\\you\\.cargo\\bin\\komodo-mcp.exe`.

## Configuration

| Variable | Default | Meaning |
|---|---|---|
| `KOMODO_ADDRESS` | (required) | Komodo Core URL, e.g. `https://komodo.example.com` |
| `KOMODO_API_KEY` | (required) | API key |
| `KOMODO_API_SECRET` | (required) | API secret |
| `KOMODO_MCP_MODE` | `read` | `read`, `execute` (adds deploy/restart/run) or `write` (adds config changes) |
| `KOMODO_MCP_ALLOW_DESTRUCTIVE` | off | `true` to allow destroy / delete / remove / prune / rotate / clear |
| `KOMODO_CA_CERT` | none | PEM file with extra CA certificates, for a Komodo behind a private CA |
| `KOMODO_MCP_TIMEOUT_SECS` | `30` | Timeout for each API request |
| `KOMODO_MCP_WAIT_TIMEOUT_SECS` | `600` | How long execute tools wait for an operation to finish |
| `KOMODO_MCP_MAX_OUTPUT_CHARS` | `60000` | Truncate tool output beyond this |
| `RUST_LOG` | `komodo_mcp=info` | Logging to stderr, e.g. `komodo_mcp=debug` |

`KOMODO_ADDRESS`, `KOMODO_API_KEY` and `KOMODO_API_SECRET` can each be given as a file instead:
set `KOMODO_API_SECRET_FILE=/path/to/file` and leave `KOMODO_API_SECRET` unset. That keeps the
secret out of the client's JSON config.

TLS uses the operating system's certificate store, so an internal CA that's already trusted
by your machine works without `KOMODO_CA_CERT`. There's deliberately no option to turn
certificate verification off.

## Example prompts

- "Is anything unhealthy in Komodo right now?"
- "Show me the nodes in my swarm and which one is the leader."
- "Search the web stack's logs for 'timeout' in the last deploy and tell me what failed."
- "What did the last three deploys of the shop stack change, and did they succeed?"
- With `execute` mode: "Pull and redeploy the shop stack, then check its logs."

## How it works

Komodo's API is RPC-style: `POST /read`, `/write` or `/execute` with
`{"type": "RequestName", "params": {...}}` and the `X-Api-Key` / `X-Api-Secret` headers.
This server talks to it as plain JSON instead of linking the `komodo_client` crate, so new
Komodo request types and fields work through `komodo_read` / `komodo_execute` /
`komodo_write` without rebuilding. Request names and parameters are listed in the
[komodo_client API docs](https://docs.rs/komodo_client/latest/komodo_client/api/).

Tested against Komodo Core's v2 API (`komodo_client` 2.3.2 request shapes).

## Development

```sh
cargo test          # unit tests: tier gating and destructive / admin guards
cargo clippy
RUST_LOG=komodo_mcp=debug komodo-mcp   # speaks MCP on stdin/stdout, logs to stderr
```

Source layout:

- `src/main.rs`: CLI, startup, stdio transport
- `src/config.rs`: environment configuration
- `src/komodo.rs`: the HTTP client
- `src/server.rs`: the MCP tools and guard rails
- `src/format.rs`: rendering logs, updates and JSON for the model

## License

[MIT](LICENSE)
