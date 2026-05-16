# Instance-as-MCP-Server

**Status:** implemented (`zunel mcp agent`). Mode 2 (agent-loop-as-tool) is a
draft design only — see [Limitations](#limitations). Outbound
`Mcp-Call-Depth` forwarding is wired through the `zunel-mcp` client, so
nested chains are bounded end-to-end.

## Goal

Let any zunel instance expose itself to other agents (zunel or otherwise) as a
standard MCP server, so a "named agent" becomes a first-class, isolated unit
of compute the rest of the system can talk to over a normal MCP connection.

The most common shape: a default-instance agent calls one or more **helper
instances** (research, code-review, refactor, summarizer, etc.) by listing them
under `tools.mcpServers` in its `config.json`, exactly the way it would wire
in a third-party MCP server today.

## Non-goals

These are explicitly deferred and will get their own design notes:

- **Mode 2: agent-loop-as-tool** — a single `helper_ask({prompt})` tool that
  runs a full `AgentLoop` inside the helper instance. Powerful but introduces
  product questions (streaming, approvals, cancellation, session persistence,
  per-call iteration ceilings) that aren't worth tangling with v1. Drafted in
  [`instance-as-mcp-mode2.md`](./instance-as-mcp-mode2.md).
- Cross-instance **session sharing** (one logical session served by multiple
  instances).
- A discovery / registry mechanism. v1 wires instances by hand in `config.json`.
- Web UI / dashboard.

## In one sentence

`zunel mcp agent --instance NAME` boots a Streamable-HTTP MCP server that
exposes instance NAME's full default tool registry, defaulting to loopback +
read-only, so the default instance (or any MCP client) can connect to it as a
normal MCP server.

## What it exposes

The `build_default_registry_async(&cfg, &workspace)` output for the chosen
instance, served over the existing `zunel-mcp-self` HTTP/HTTPS code path. That
means the helper instance's:

- filesystem tools (subject to its `PathPolicy`)
- search tools (`glob`, `grep`)
- shell `exec` (gated, see below)
- web tools (gated)
- cron (`cron_jobs_list`, `cron_job_get`)
- the `self_*` family (`self_status`, etc.)
- **its own** `mcp_*` fan-outs to whatever it has wired up

The connecting client drives the LLM loop. The server does not run an LLM —
that's Mode 2.

## CLI surface

```text
zunel [--instance NAME] mcp agent [TRANSPORT] [AUTH] [ORIGIN] [DEPTH] [TOOL GATES]

TRANSPORT
  --bind <addr>           Default 127.0.0.1:0. Streamable HTTP/HTTPS only;
                          stdio is reserved for the existing `zunel mcp
                          serve` command.
  --https-cert <path>     PEM-encoded certificate. Pair with --https-key
                          to terminate HTTPS in-process.
  --https-key  <path>

AUTH
  --api-key <token>       Repeatable; tokens stack into an allowlist.
  --api-key-file <path>   Repeatable; one token per non-blank line.
                          Lines starting with '#' are comments.

ORIGIN
  --allow-origin <url>    Repeatable. When set, requests with an Origin
                          header must match (case-insensitive) one of
                          the listed entries. Missing or 'null' Origin
                          bypasses the check (RFC-6454-style).

DEPTH
  --max-call-depth N      Reject requests whose Mcp-Call-Depth header is
                          >= N. Default 8.

LIMITS
  --max-body-bytes N      Reject requests whose Content-Length exceeds N
                          (with 413 Payload Too Large) *before* the body
                          is read off the socket. Accepts K/M/G suffixes
                          (base-1024). Default 4 MiB.

OBSERVABILITY
  --access-log <path>     Emit one JSON object per served request to the
                          given sink. Use '-' for stdout (handy under
                          journalctl / docker logs); any other value is a
                          file path opened in append mode (logrotate
                          copytruncate-friendly). See "Access logging"
                          below for the schema and secrets policy.

TOOL GATES (disabled by default; opt-in per category)
  --allow-write           Expose write_file, edit_file, cron.
  --allow-exec            Expose exec.
  --allow-web             Expose web_search, web_fetch.
```

The active instance is selected via the global `--instance NAME` flag (same
plumbing as every other `zunel` subcommand), which sets `ZUNEL_HOME` for the
duration of the process. The agent server then loads that home's `config.json`
and workspace exactly like `zunel agent` does.

## Defaults and guardrails

These are deliberately **stricter** than `zunel agent`'s local defaults, because
the trust model is "another process is calling me over the wire" rather than
"I am the user".

| Concern | `zunel agent` (local) | `zunel mcp agent` (server) |
|---|---|---|
| Bind | n/a | `127.0.0.1:0` |
| Non-loopback bind | n/a | hard error without HTTPS + API key |
| Write tools (`write_file`, `edit_file`, `cron`) | enabled | **disabled** (need `--allow-write`) |
| Exec tool | enabled if `tools.exec.enable` | **disabled** (need `--allow-exec`) |
| Web tools | enabled if `tools.web.enable` | **disabled** (need `--allow-web`) |
| Read / search / `cron_*` (read) | enabled | enabled |
| `mcp_*` fan-outs | enabled | **disabled** (see Limitations) |
| Workspace policy | `PathPolicy::restricted(workspace)` | unchanged (still rooted at the instance's workspace) |

The opt-in flags are checked **before** mutating the registry returned by
`build_default_registry_async`, so a misconfigured `tools.exec.enable=true`
in the helper instance's `config.json` is **not** sufficient to expose `exec`
over MCP. Both must agree.

`cron` is a single tool with both read and write sub-actions, so it is
classified as a "write" tool: a network-exposed read-only registry should not
be able to schedule arbitrary jobs against the host's scheduler. Granting
read-only cron access would require splitting the tool rather than relaxing
the gate.

## Authentication

Reuses the existing `zunel-mcp-self` machinery verbatim:

- multiple `--api-key` / `--api-key-file` entries stack into an allowlist
- match-any constant-time comparison (no timing leak about which slot matched)
- `Authorization: Bearer <token>` and `X-API-Key: <token>` both accepted
- 401 with `WWW-Authenticate: Bearer realm="zunel-mcp-self"` on mismatch
- key rotation = deploy with `OLD,NEW`, roll clients, redeploy with just `NEW`

Non-loopback bind without auth is a hard startup error. Loopback without auth
is allowed but warned.

## Loop / fanout protection

Every request carries an optional `Mcp-Call-Depth: <int>` header. The server
rejects with `403 Forbidden` if the value is `>= --max-call-depth` (default 8).
This is cheap insurance against accidental A→B→A loops in chains of nested
MCP servers.

Both ends of the wire are now implemented:

- **Server side.** The HTTP transport in `zunel-mcp-self` parses
  `Mcp-Call-Depth` and rejects when it meets or exceeds the cap. The depth is
  threaded through the new `DispatchMeta` parameter on `McpDispatcher::dispatch`
  and stamped onto each tool call's `ToolContext::incoming_call_depth` so any
  outbound MCP fan-out from inside the dispatcher knows where it sits in the
  chain.
- **Client side.** `RemoteMcpClient::call_tool_with_depth` (and its
  trait-default-bridged twin on `McpClient`) attaches
  `Mcp-Call-Depth: <incoming + 1>` to outbound HTTP requests. The
  `McpToolWrapper` reads `ctx.incoming_call_depth` and forwards the incremented
  value automatically, so any tool routed through the wrapper participates in
  the chain without code-level changes. Top-level callers (CLI agent loop, ad-hoc
  invocations) start with `incoming_call_depth = None`, so the wrapper sends
  `Mcp-Call-Depth: 1` on their first hop.

The agent server strips `mcp_*` tools from its exposed registry — that's a
deliberate product call (see Limitations) rather than a depth concern.

## Origin handling

The HTTP server inspects the `Origin` header on every `POST /`. The default
allowlist is empty (i.e. Origin is *not* checked); when `--allow-origin <url>`
is passed (repeatable), any literal Origin must match (case-insensitively) at
least one allowlisted entry. A missing Origin or the literal string `null` —
the typical case for non-browser MCP clients — bypasses the check. Bearer auth
alone closes the obvious attack, but Origin checks are belt-and-suspenders
against drive-by browser exploitation when the server happens to bind a
reachable port.

## Graceful shutdown

The agent process cooperates with `systemd`, `launchd`, `docker stop`, and a
plain Ctrl-C alike: on `SIGINT` or `SIGTERM` the listener stops accepting new
connections and the process waits up to 5 seconds for in-flight requests to
finish. Anything still running past the grace window is aborted. A regular
exit-zero is the expected outcome — non-zero only appears when an in-flight
handler had to be torn down forcibly.

This means a typical operator restart looks like `systemctl restart` or
`docker restart` without dropping responses to mid-flight tool calls; only
truly stuck handlers (e.g. an `exec` that ignored cancellation) get aborted.

## Workspace foot-gun warning

At startup, when binding a non-loopback address against a workspace that is a
git repository, the server prints a loud `warning:` line to stderr noting that
any client with a valid API key will be able to read (and, with `--allow-write`,
modify) files in the tree. The server does **not** refuse to start; the operator
may genuinely want to expose a working repo across a trusted LAN. A stricter
"refuse to start unless `--i-know-what-im-doing`" mode for `$HOME`-rooted or
`/`-rooted workspaces is tracked under [Limitations](#limitations).

## Access logging

`--access-log <path>` emits one JSON line per served request to the given sink.
This is the recommended way to ship audit trails to a SIEM, drive a token-cost
dashboard, or grep for incident timelines.

Sinks:
- **`-`** writes to stdout. Pair with `journalctl`/`docker logs`/`launchd` so
  the supervisor handles rotation.
- **Any other value** is a file path opened in **append mode**. Per-emit
  writes are atomic (single `write_all` of a payload smaller than `PIPE_BUF`),
  so `logrotate`'s `copytruncate` strategy works without the agent needing
  to re-open on a signal.

Schema (fields are stable; optional fields are omitted when not applicable):

```jsonc
{
  "ts":     "2026-04-26T20:34:12.123456Z", // RFC 3339 UTC, microsecond precision
  "peer":   "127.0.0.1:54321",             // remote socket addr (post-TLS)
  "method": "tools/call",                  // JSON-RPC method, "*batch" for batches
  "tool":   "read_file",                   // present only when method == tools/call
  "rpc_id": 7,                             // JSON-RPC id passed through verbatim
  "depth":  2,                             // Mcp-Call-Depth (omitted when absent)
  "key":    "ab12cd34",                    // first 8 hex chars of SHA-256(matched bearer)
  "status": 200,                           // HTTP status returned
  "ms":     14                             // wall-clock latency in milliseconds
}
```

**Secrets policy.** `key` is a 4-byte SHA-256 fingerprint of the matched bearer
token, never the token itself, so logs are safe to ship as-is. Failed-auth
attempts (`status: 401`) deliberately omit `key` entirely, so credential-stuffing
probes can't flood the file with attacker-chosen fingerprints.

If the sink fails (disk full, permission flip, etc.) the server logs a single
warning to stderr and keeps serving. Logging is observability, not load-bearing.

## Worked example

Helper instance (`research`) is started under launchd / systemd:

```text
zunel --instance research mcp agent \
  --bind 127.0.0.1:8443 \
  --https-cert /etc/zunel/research/cert.pem \
  --https-key  /etc/zunel/research/key.pem \
  --api-key-file /etc/zunel/research/keys \
  --allow-web
```

`/etc/zunel/research/keys` (mode 0600):

```text
# active
2026-04-26-prod-token
# pending rotation, remove next deploy
2026-03-14-prod-token
```

Default instance's `config.json` wires it in like any other MCP server, with a
secret pulled from the environment so it never appears in the file:

```jsonc
{
  "tools": {
    "mcpServers": {
      "research": {
        "transport": "streamableHttp",
        "url": "https://127.0.0.1:8443",
        "headers": {
          "Authorization": "Bearer ${ZUNEL_RESEARCH_TOKEN}"
        }
      }
    }
  }
}
```

The default agent now sees `mcp_research_*` tools alongside everything else,
each scoped to the research instance's workspace, OAuth tokens, and config.

## Generating the snippet automatically

Hand-rolling the `tools.mcpServers.<name>` block is fine once. After that the
recommended path is `--print-config`, which prints the snippet to stdout and
exits without binding the listener. It reuses the same flag surface as a real
serve (so a typo in `--bind` or `--api-key-file` still surfaces here), but it
short-circuits before TLS file I/O and the loopback guard so you can preview
a snippet for a config you haven't fully provisioned yet.

```text
zunel --instance research mcp agent \
  --bind 127.0.0.1:8443 \
  --https-cert /etc/zunel/research/cert.pem \
  --https-key  /etc/zunel/research/key.pem \
  --api-key-file /etc/zunel/research/keys \
  --allow-web \
  --print-config > research-snippet.json
```

The output looks like the worked example above. Two key properties:

- **Secrets stay out.** Even though the command sees the API keys, the snippet
  always emits `Bearer ${ZUNEL_<INSTANCE>_TOKEN}` (or whatever you pass to
  `--public-env`), never the literal token. `--print-config > snippet.json` is
  safe to commit to the same repo as the rest of your zunel config.
- **Wildcard binds and `:0` ports become placeholders.** `0.0.0.0` / `::`
  render as `<HOST>` and port `0` renders as `<PORT>` in the URL, so the
  generated snippet fails loudly when pasted into a hub before the operator
  has decided what hostname/port the helper is reachable at. Pass
  `--public-url https://helper.example:8443/` to fix the URL up front
  (handy for binds behind a load balancer or reverse proxy).

Optional knobs:

| Flag | Purpose |
|---|---|
| `--public-name <name>` | Override the `mcpServers` entry key (defaults to the active instance name). Useful when two helper instances share a name across hosts. |
| `--public-url <url>`   | Override the URL emitted in the snippet. Required for wildcard binds you want to expose under a stable hostname. |
| `--public-env <VAR>`   | Override the env-var name in the `Authorization` placeholder. Handy when the consumer instance's deployment already standardizes on something other than `ZUNEL_<INSTANCE>_TOKEN`. |

## Implementation notes

- **Code reuse, not copy-paste.** `zunel-mcp-self` was refactored into a
  `[lib] + [bin]` crate so the existing Streamable-HTTP/HTTPS server, TLS
  loader, auth, session-id, and SSE/JSON content negotiation are reused
  verbatim. The new command sits in `zunel-cli` and just supplies a
  different `McpDispatcher` implementation.
- **`McpDispatcher` trait.** The transport in `zunel_mcp_self::http::run`
  is now generic over `D: McpDispatcher`. The two production dispatchers are
  `zunel_mcp_self::SelfDispatcher` (used by the binary and `zunel mcp serve`)
  and `zunel_cli::commands::mcp::registry_dispatcher::RegistryDispatcher`
  (used by `zunel mcp agent`).
- **Registry source of truth.** `zunel_core::build_default_registry_async`
  stays the single place that decides what tools an instance gets. The new
  command applies the `--allow-*` filters on top, then hands the resulting
  registry to the HTTP server.
- **Subagent / spawn.** The `SpawnTool` is registered by `zunel agent`,
  not `build_default_registry_async`, so the agent server inherits the right
  behavior for free: no nested LLM loops are exposed.
- **One server per instance per host.** Two simultaneous `zunel mcp agent`
  invocations on the same machine and same port fail loudly at `bind()`.
  No advisory lockfile.
- **Lazy startup is out of scope.** Each helper is a long-running process. If
  someone wants on-demand activation, that's systemd socket activation in the
  unit file, not zunel code.

## Limitations

These are deliberate cuts; each is tracked as a follow-up:

1. **No `mcp_*` re-export.** The exposed registry strips any tool whose
   name starts with `mcp_`. The depth-forwarding plumbing is in place
   (depth flows through `DispatchMeta` → `ToolContext::incoming_call_depth` →
   `RemoteMcpClient::call_tool_with_depth`), but cross-instance fanout raises
   product questions (per-call timeouts, OAuth token visibility, audit) that
   need a design pass before being exposed on the wire. Re-enabling the prefix
   is a one-line change in `agent::build_filtered_registry` once that design
   lands.
2. **No agent-loop-as-tool ("Mode 2").** A single `helper_ask({prompt})` tool
   that runs a full `AgentLoop` inside the helper instance. The design is
   drafted in [`instance-as-mcp-mode2.md`](./instance-as-mcp-mode2.md) and
   covers streaming, approvals, cancellation, session persistence, and
   per-call iteration ceilings; this surface deliberately leaves room for it
   but does not implement it.
3. ~~**No "refuse to start" workspace check.**~~ **Implemented.** The
   guard now refuses to start when the resolved workspace is `/`,
   `$HOME`, or an ancestor of the instance's `~/.zunel/` (which would
   let the agent loop mutate its own config/sessions). The
   non-loopback git-repo `warning:` from §SECURITY GUARDRAILS still
   fires because that's a soft heuristic about exposing real working
   trees; the new guard is a hard refuse-to-start. Escape hatch:
   `zunel --i-know-what-im-doing` (global flag) or
   `ZUNEL_ALLOW_UNSAFE_WORKSPACE=1`. See
   [`cli-reference.md`](./cli-reference.md#workspace-foot-gun-guard)
   for the full trigger table.
4. **No richer audit format beyond `--access-log`.** The current JSON-lines
   schema covers per-request basics; multi-tenant operators who need
   additional fields should open an issue describing the schema they need.
5. **No discovery.** Each consumer instance wires helpers by hand in
   `config.json`. A directory-style registry is out of scope.

## Out-of-scope reminders

- **No agent-as-tool today.** If someone asks "can I just send a prompt and
  let the helper figure it out", the answer is "not yet — wire the tools and
  let your local LLM drive the loop, or wait for Mode 2."
- **No multi-instance shared sessions.** Each helper has its own
  `SessionManager`, scoped to its own `ZUNEL_HOME`.
- **No discovery.** Hand-edit `config.json`.
