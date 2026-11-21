# MCP servers

oxa is an MCP client. It connects to Model Context Protocol servers, over a local process (stdio) or over streamable HTTP, and offers their tools to the model. Servers are configured in `mcp.json`, signed in to with OAuth where needed, and listed with `/mcp`.

## Configuration

Two files are read, the user's and the project's:

| File | Scope |
|---|---|
| `~/.oxa/agent/mcp.json` | user servers |
| `<project>/.oxa/mcp.json` | project servers; read only in a trusted project |

Both have the same shape:

```json
{
  "mcpServers": {
    "docs": {
      "type": "http",
      "url": "https://mcp.example.com/mcp",
      "oauth": { "clientRegistration": "auto" }
    },
    "files": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/home/me/notes"],
      "env": { "TOKEN": "${NOTES_TOKEN}", "REGION": "!cat ~/.config/notes/region" },
      "exposure": "codemode",
      "timeout": 60,
      "description": "notes folder"
    }
  }
}
```

Each server is either a stdio server (`command`, `args`, `env`, `cwd`) or an HTTP server (`url`, `headers`, `oauth`). `type` may be `stdio`, `http` or `streamable-http`; the legacy SSE transport is refused. Other keys:

| Key | Meaning |
|---|---|
| `enabled` | `false` keeps the entry but does not connect |
| `description` | shown in `/mcp` and `oxa mcp list` |
| `timeout` | seconds for a request; a progress report resets it |
| `exposure` | how the server's tools are offered: `codemode` (default), `deferred`, `direct`, `hidden` |
| `toolExposure` | exposure per tool: `{ "<tool name or pattern>": "direct" }`. A pattern uses `*`. An exact name wins over patterns; among patterns, the first match wins |
| `headers` | HTTP headers. Values may be `${VAR}` (unset is empty) or `!command` (the command's output) |
| `env` | environment of a stdio server; the same value forms as `headers` |
| `trustAnnotations` | `true` or `false`: whether oxa believes the server's `readOnlyHint` marks. Default `true` for a stdio server and `false` for an HTTP server. See [Permissions](#permissions) |

A stdio server is started with your rights, outside the bash sandbox. An HTTP server gets its network from the host network, subject to the rules in [security.md](security.md#network).

### Project servers

A project can add servers only when it is trusted. Its `mcp.json` may also override `enabled`, `exposure` and `toolExposure` of a server the user file defines. A trusted project's changed file loads with a notice under the default folder policy. With `projectTrustPolicy: "content"`, it needs fresh approval ([project trust](security.md#project-trust)).

## Commands

```console
$ oxa mcp add docs --url https://mcp.example.com/mcp --oauth-client-registration auto
$ oxa mcp add files -- npx -y @modelcontextprotocol/server-filesystem /home/me/notes
$ oxa mcp list
$ oxa mcp login docs
$ oxa mcp logout docs
$ oxa mcp remove files
```

`add` and `remove` write `mcp.json` and keep the other entries and their order. `-l` writes to the project file. `list` connects to each enabled server and reports its state, transport and tools; it exits 1 when an entry is invalid or a server does not connect. All options are in [cli.md](cli.md#mcp-commands).

Inside the interface, `/mcp` opens the server list. Each row shows the server, its state and its exposure. From there you can reconnect a server, enable or disable it, and change its exposure. A change to or from `direct` takes effect at the next prefix epoch. Signing in is done with `oxa mcp login`.

## Connecting

Servers connect in the background when oxa starts. The first prompt waits up to 10 seconds for any server whose tools are declared directly to the model, so the model sees them from the first request. A server that is still connecting after that is offered at the next prefix epoch.

Before oxa starts or connects to a server, the permission gate is asked: an `Exec` for a stdio server and a `Net` for an HTTP server ([security.md](security.md#permission-modes)). A server that the gate refuses shows as denied. In offline mode HTTP servers are refused. A stdio server is started through `bubblewrap` with the network unshared, and it is refused when `bubblewrap` cannot be used. Its file access is not limited: see [security.md](security.md#offline-mode).

Plan mode denies MCP startup even when the project is trusted. A server that answers with an authentication challenge and has no usable token shows as needing sign-in. Run `oxa mcp login <name>` to sign in. `oxa mcp login <name> -l` checks project trust and any content changes before connecting.

The HTTP client uses the `httpProxy` setting, or `HTTP_PROXY` and `HTTPS_PROXY`, when those are set.

Server activity and errors are logged to `mcp.log` in the agent directory. The log is rotated when it grows.

## OAuth

`oxa mcp login <name>` connects to the server, then runs the OAuth authorization-code flow with PKCE:

1. oxa discovers the server's authorization metadata. The server must advertise a `resource` that matches its URL and an issuer that matches the one returned in the redirect.
2. oxa gets a client id. Depending on `oauth.clientRegistration`, it uses `oauth.clientId` if set, registers a client dynamically (`dcr`), uses a client id metadata document (`cimd`), or picks whichever the server offers (`auto`, the default).
3. It opens the browser to the authorization page and listens for the redirect on a local port (`oauth.callbackPort` or a random port). On a terminal you can paste the redirect URL instead. The pasted URL must match the redirect address oxa registered.
4. Tokens are stored in `mcp-auth.json` in the agent directory, mode `0600`, keyed by server name and URL. oxa refreshes them on a 401 and asks for a broader scope when the server asks for one.

`oxa mcp login` waits up to 300 seconds by default (`--timeout`). `oxa mcp logout <name>` deletes the stored credentials for that name and URL.

Static credentials can be used instead of OAuth: `--bearer-token-env-var <VAR>` sends the value of an environment variable as a bearer token, and a `headers` entry can set any header.

## Exposure

The exposure decides how a server's tools reach the model.

| Exposure | Where the tool appears | Effect on the prompt |
|---|---|---|
| `codemode` (default) | callable only from a codemode script | none; enables codemode when no other setting does |
| `deferred` | found with `tool_search`, called with `call_tool` | none |
| `direct` | declared to the model in the tool list | the tool list changes at the next prefix epoch |
| `hidden` | not offered | none |

Codemode and `tool_search` are described in [tools.md](tools.md#codemode-tool_search-and-call_tool). Use `direct` for a few tools you want the model to see without a search, and `codemode` or `deferred` for a large server.

MCP tools are named `mcp__<server>__<tool>`. If two tools would get the same name, both get a short hash suffix. A server that offers resources also gets tools to list and read them.

Tool calls are checked by the permission gate on the tool's class. A tool that its server marks read-only counts as a read, and any other MCP tool counts as a write. In workspace mode a write asks for approval, and plan mode refuses it. The mark is explained under [Permissions](#permissions).

## Permissions

Before oxa starts a stdio server, the permission gate is asked to allow a process with network access. Before it connects to an HTTP server, the gate is asked to allow that host. A server the gate refuses shows as denied.

Each tool call is checked by its class:

- a tool whose server sets `readOnlyHint: true` is a read. Plan and workspace modes run it without asking, and ask mode asks;
- any other MCP tool is a write. Workspace mode asks, and plan mode refuses.

The `readOnlyHint` mark is the server's own claim. oxa does not check it, so a server can mark a tool that changes things as read-only. oxa believes the mark for a stdio server by default, because that server already runs with your rights. For an HTTP server oxa does not believe it unless the entry says `"trustAnnotations": true`. Set that only for a server you trust.

## Disabling

- `--no-mcp` for one run: no server connects and no MCP tool is offered.
- `"enabled": false` in an entry, or `oxa mcp remove <name>`.
- In a project, `enabled` can switch off a server defined in the user file.

## Troubleshooting

- `/mcp` shows the state of each server. `oxa mcp list` gives the same state with the full error text.
- `mcp.log` in the agent directory records server activity and errors.
- A server that shows as denied was refused by the permission gate. Check the mode, or the offline switch.
- A project server that does not appear may be waiting for the project to be trusted, or for its changed content to be approved. Start oxa there interactively to see the question.
