# The `self` Tool

The `self` tool lets the agent inspect safe read-only runtime state in
local CLI sessions, the Slack gateway, and the library facade.

Normal tools act on the outside world: files, shells, search, web, cron, and
MCP. `self` is for agent self-inspection:

- active model and provider
- resolved workspace
- current and maximum iteration count
- registered tool names
- subagent status

## Availability

`self` is registered automatically by the runtime. There is no `tools.my`
or `tools.self` config block to enable it.

The tool is intentionally read-only. It accepts `action: "check"` and returns a
runtime summary. The `action: "set"` path is rejected so runtime state cannot be
mutated through this tool.

## Example

```text
self(action="check")
# -> model: gpt-4o-mini
#    provider: custom
#    workspace: /Users/you/.zunel/workspace
#    iterations: 3/200
#    tools: 12 registered - read_file, write_file, edit_file, list_dir, glob, grep, ...
#    subagents: none
```

## Transports

The bundled `zunel-mcp-self` binary speaks two MCP transports:

- **stdio** (default) — the runtime spawns the binary as a child process and
  exchanges Content-Length-framed JSON-RPC over stdin/stdout. No flags needed.
- **Streamable HTTP / HTTPS** — pass `--http <addr>` (or set
  `ZUNEL_MCP_SELF_HTTP=<addr>`) to bind a single `POST /` endpoint. Use
  `127.0.0.1:0` to let the OS pick a port; the bound URL is printed on startup
  as `zunel-mcp-self listening on <scheme>://HOST:PORT`. Responses are delivered
  as `text/event-stream` (chunked SSE) when the client's `Accept` header
  includes `text/event-stream`, and as `application/json` otherwise.
  Notifications are acknowledged with `202 Accepted`. The server issues an
  `Mcp-Session-Id` header on every response.

Both transports route through the same handler set, so the tool surface is
identical regardless of how the host connects.

### HTTPS

Pass a PEM-encoded certificate and private key to terminate TLS in-process. The
server uses the rustls `ring` provider that the rest of the workspace already
links.

```text
zunel-mcp-self \
  --http 127.0.0.1:8443 \
  --https-cert /path/to/cert.pem \
  --https-key  /path/to/key.pem
```

Both flags must be provided together. The banner switches to
`zunel-mcp-self listening on https://HOST:PORT`. For local development, generate
a cert with `mkcert -install && mkcert 127.0.0.1 localhost`.

### API key authentication

Set one or more bearer tokens to require authentication on every `POST /`
request. The server compares tokens in constant time and accepts either header
form below; mismatches and missing credentials return `401 Unauthorized` with
`WWW-Authenticate: Bearer realm="zunel-mcp-self"`.

```text
zunel-mcp-self --http 127.0.0.1:8080 --api-key supersecret
# or
ZUNEL_MCP_SELF_API_KEY=supersecret zunel-mcp-self --http 127.0.0.1:8080
# or load tokens from a file (one per line; '#' comments and blank lines
# ignored). The flag may be repeated and stacks with --api-key.
zunel-mcp-self --http 127.0.0.1:8080 --api-key-file /run/secrets/zunel-self-keys
```

Clients then send one of:

- `Authorization: Bearer supersecret`
- `X-API-Key: supersecret`

In Zunel's MCP server config, attach the bearer token via the existing `headers`
map so `RemoteMcpClient` includes it on every request. Header values support
shell-style `${VAR}` substitution against the agent's process environment so the
literal token never has to live in `config.json`:

```jsonc
{
  "tools": {
    "mcp_servers": {
      "self-remote": {
        "transport": "streamableHttp",
        "url": "https://self.local:8443",
        "headers": {
          "Authorization": "Bearer ${ZUNEL_SELF_TOKEN}"
        }
      }
    }
  }
}
```

Substitution rules:

- `${VAR}` — replaced with the value of the named environment variable. If the
  variable is unset (or empty), the entire header is **dropped** with a
  `tracing::warn!` so a literal `${VAR}` is never sent on the wire. Drop the
  `Authorization` header that way and the existing OAuth-token cache fallback
  takes over for that server.
- `${VAR:-fallback}` — same as above, but uses the literal `fallback` text when
  the variable is unset or empty. Useful for non-secret defaults like trace
  IDs.
- `$$` is an escape for a literal `$`. A bare `$` not followed by `{` or `$` is
  passed through unchanged so values such as PHC hashes (`$argon2id$...`) keep
  working.

### Key rotation

Every API-key source stacks: `--api-key`, `--api-key-file`, and
`ZUNEL_MCP_SELF_API_KEY` all contribute entries to the allowlist, and the
server accepts a request whose token matches **any** entry. That means a
zero-downtime rotation looks like this:

1. Append the new token on a new line in `/run/secrets/zunel-self-keys` (or
   add a second `--api-key new-token` flag) and reload the server. Both old
   and new tokens are now valid.
2. Roll clients to the new token — they keep working before, during, and after
   each restart because the old token is still accepted.
3. Once every client is on the new token, remove the old line / flag and
   reload again. The retired token now returns `401`.

The match-any check walks the entire allowlist on every request so timing of a
401 versus a 200 leaks no information about which slot held the matching key.

### Request body cap

Every `POST /` request must declare a `Content-Length` no larger than the
configured cap (default 4 MiB). Oversized requests are rejected with
`413 Payload Too Large` **before** any body bytes are read off the socket, so a
malicious client cannot make the server page through gigabytes of garbage just
by tagging the right header onto the request line. Override with
`--max-body-bytes <N>` (or `ZUNEL_MCP_SELF_MAX_BODY_BYTES`); the value accepts
plain integers and the friendly `K`/`M`/`G` suffixes (base-1024).

```text
# Accept up to 16 MiB request bodies on this listener.
zunel-mcp-self --http 127.0.0.1:8080 --max-body-bytes 16M
```

### Graceful shutdown

The HTTP server cooperates with process supervisors. On `SIGINT` (Ctrl-C) or
`SIGTERM` (the signal `systemd`, `launchd`, and `docker stop` send) the
listener stops accepting new connections immediately and the binary then waits
up to **5 seconds** for in-flight requests to finish before aborting any
stragglers. A clean exit-zero is the expected outcome for a healthy shutdown;
non-zero only appears when a request was forcibly terminated past the grace
window.

### Access logging

`--access-log <path>` (or `ZUNEL_MCP_SELF_ACCESS_LOG`) emits one JSON object
per served request to the given sink. Use `-` to direct output to stdout
(handy under `journalctl`/`docker logs`/`launchd`); any other value is treated
as a file path opened in **append mode**, which is compatible with
`logrotate`'s `copytruncate` strategy without the agent needing to re-open on
a signal.

Each entry follows this shape (fields are stable, optional fields omitted
when not applicable):

```jsonc
{
  "ts":     "2026-04-26T20:34:12.123456Z", // RFC 3339 UTC, microsecond precision
  "peer":   "127.0.0.1:54321",             // remote socket addr (post-TLS)
  "method": "tools/call",                  // JSON-RPC method, "*batch" for batches
  "tool":   "self_status",                 // present only when method == tools/call
  "rpc_id": 7,                             // JSON-RPC id passed through verbatim
  "depth":  2,                             // Mcp-Call-Depth header (omitted when absent)
  "key":    "ab12cd34",                    // first 8 hex chars of SHA-256(matched bearer)
  "status": 200,                           // HTTP status returned
  "ms":     14                             // wall-clock latency in milliseconds
}
```

**Secrets policy.** The `key` field is a stable 4-byte fingerprint of the
matched bearer token, never the token itself, so logs can safely be shipped to
a SIEM. Failed-auth attempts (`status: 401`) deliberately omit `key` so
credential-stuffing probes can't flood the file with attacker-chosen
fingerprints.

```text
# Append per-request lines to a rotatable file.
zunel-mcp-self --http 127.0.0.1:8080 --access-log /var/log/zunel-mcp-self.jsonl

# Stream to stdout for systemd/journalctl.
zunel-mcp-self --http 127.0.0.1:8080 --access-log -
```

### Quick reference

```text
# stdio (default)
zunel-mcp-self

# Streamable HTTP, ephemeral port
zunel-mcp-self --http 127.0.0.1:0

# HTTPS + bearer auth
zunel-mcp-self \
  --http 0.0.0.0:8443 \
  --https-cert cert.pem --https-key key.pem \
  --api-key-file /run/secrets/zunel-self-key

# Tighten the request-body ceiling
zunel-mcp-self --http 127.0.0.1:0 --max-body-bytes 64K

# Audit every request to a JSON-line log
zunel-mcp-self --http 127.0.0.1:0 --access-log access.jsonl
```

Recognized environment variables:

| Variable                          | Equivalent CLI flag           |
| --------------------------------- | ----------------------------- |
| `ZUNEL_MCP_SELF_HTTP`             | `--http`                      |
| `ZUNEL_MCP_SELF_TLS_CERT`         | `--https-cert`                |
| `ZUNEL_MCP_SELF_TLS_KEY`          | `--https-key`                 |
| `ZUNEL_MCP_SELF_API_KEY`          | adds one entry to `--api-key` |
| `ZUNEL_MCP_SELF_MAX_BODY_BYTES`   | `--max-body-bytes`            |
| `ZUNEL_MCP_SELF_ACCESS_LOG`       | `--access-log`                |

## Safety

`self` never exposes secrets. It reports only safe runtime metadata and summary
state, not API keys, OAuth tokens, raw provider clients, or channel credentials.
