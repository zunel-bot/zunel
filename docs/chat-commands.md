# In-Chat Commands

These commands work inside Slack conversations and local interactive agent sessions:

| Command | Description |
|---------|-------------|
| `/new` | Stop current task and start a new conversation |
| `/stop` | Stop the current task |
| `/restart` | Restart the bot |
| `/status` | Show bot status |
| `/reload` | Re-discover every configured MCP server without restarting the process. Use after restarting an MCP backend (or editing `~/.zunel/config.json`) so the agent picks up the freshly listed tools immediately. |
| `/reload <server>` | Re-discover one MCP server by name (the same key under `tools.mcpServers` in `config.json`). Useful when only one backend went unhealthy. |
| `/dream` | Run Dream memory consolidation now |
| `/dream-log` | Show the latest Dream memory change |
| `/dream-log <sha>` | Show a specific Dream memory change |
| `/dream-restore` | List recent Dream memory versions |
| `/dream-restore <sha>` | Restore memory to the state before a specific change |
| `/help` | Show available in-chat commands |

> **Slack note:** `/reload` and the other `/`-prefixed commands above are
> processed by the **local CLI REPL** (`zunel agent`). Slack channels feed
> messages straight to the agent loop without slash-command parsing, so in
> Slack you ask the agent in plain language ("reconnect my-server", "reload
> all MCP servers") and it calls the [`mcp_reconnect`](#mcp-reconnect)
> tool against the same live registry. The end effect is identical — no
> gateway restart needed.

## `mcp_reconnect`

The `mcp_reconnect` tool is registered automatically alongside `self` and
`spawn` in the local agent (`zunel agent`), the Slack gateway
(`zunel gateway`), and the library facade. It re-runs MCP discovery
against `~/.zunel/config.json` and splices the freshly listed tools into
the live registry the agent reads from on every turn.

**Arguments:**

- `server` *(string, optional)* — one MCP server name. Omit to reload
  every configured server (matches the boot-time `register_mcp_tools`
  pass).

**Returns:** a JSON object with `attempted`, `succeeded`, and `failed`
arrays. The `failed` array carries `{server, error}` objects so the
agent can explain to the user exactly which backend is still broken.

Network I/O happens off the registry write lock, so concurrent turns
keep running against the previous snapshot until the swap completes
(usually low single-digit milliseconds for the swap itself).

### Background auto-reconnect

Both `zunel gateway` and `zunel agent` (REPL mode) also run a periodic
**auto-reconnect** task in the background. Every 5 minutes (tunable via
`ZUNEL_MCP_RECONNECT_TICK_SECS`, disable with
`ZUNEL_MCP_RECONNECT_DISABLED=1`) it walks every configured MCP server
and quietly retries any that aren't currently serving tools. So a
backend that was down at boot heals itself once it comes back online,
without anyone running `/reload` or asking for `mcp_reconnect`.

Servers stuck on the `mcp_<name>_login_required` stub are deliberately
skipped — those need a chat-driven login (`mcp_login_complete` skill)
or `zunel mcp login --force`, not a periodic re-dial. See
[`docs/configuration.md`](configuration.md#background-auto-reconnect).

## Periodic Tasks

The gateway wakes up every 30 minutes and checks `HEARTBEAT.md` in your
workspace (`~/.zunel/workspace/HEARTBEAT.md`). If the file has tasks, the agent
executes them and delivers results to your most recently active Slack
conversation.

**Setup:** edit `~/.zunel/workspace/HEARTBEAT.md` (created automatically by
`zunel onboard`):

```markdown
## Periodic Tasks

- [ ] Check weather forecast and send a summary
- [ ] Scan inbox for urgent emails
```

The agent can also manage this file itself — ask it to "add a periodic task" and it will update `HEARTBEAT.md` for you.

> **Note:** The gateway must be running (`zunel gateway`) and you must have
> chatted with the bot at least once so it knows which Slack target to deliver
> to.
