# Zunel User Guide

A task-oriented walkthrough that takes you from a clean machine to a running
local agent + Slack gateway with realistic tool configuration. Reach for this
when you want a single page that ties everything together. For terse
install-and-go, see [`quick-start.md`](./quick-start.md); for an exhaustive
per-field reference of every config knob, see [`configuration.md`](./configuration.md).

> Tracks the v2.2.1 schema and the new defaults from the security/perf work
> on `main`. If you've been running an older release, run
> `zunel onboard --force` and merge your existing tokens back in.

---

## 1. Install

```bash
brew tap zunel-bot/tap
brew install zunel
zunel --version          # 2.2.1 or later
```

`.deb` and `cargo install` paths are documented in
[`quick-start.md`](./quick-start.md#install).

## 2. First-run setup

```bash
zunel onboard
```

Creates:

| Path | Contents |
|---|---|
| `~/.zunel/config.json` | Workspace-wide config (mode `0600`). Provider keys, channel tokens, tool tuning all live here. |
| `~/.zunel/workspace/` | Default workspace root. Sessions, skills, memory live under it. |
| `~/.zunel/workspace/{SOUL,USER,HEARTBEAT}.md` | Starter persona / user facts / heartbeat files. Edit freely. |
| `~/.zunel/workspace/memory/MEMORY.md` | Durable cross-session memory ledger. |

> The onboarder writes `config.json` at `0600` from the very first invocation.
> If you migrated a config from a pre-v2.2 install, `chmod 0600` it once and
> the subsequent token-refresh writes will keep it locked.

## 3. The annotated full config

Below is a realistic `~/.zunel/config.json` that exercises every major
subsystem: OpenAI-compatible provider, Slack channel with DM allowlist,
shell tool with workspace-scoped `PATH`, web search via Brave, a stdio MCP
server (filesystem), a remote MCP server (Atlassian over OAuth2), and the
human-in-the-loop approval gate scoped to shell commands. Drop the parts you
don't want.

```jsonc
{
  // ──────────────────────────────────────────────────────────────────────
  // 1. Providers — pick exactly one via agents.defaults.provider below.
  //    Each block here only takes effect when that provider is selected.
  // ──────────────────────────────────────────────────────────────────────
  "providers": {
    "custom": {
      // Any OpenAI-compatible chat.completions endpoint: OpenAI itself,
      // Together, Groq, vLLM, llama.cpp's server, Anthropic via a proxy, …
      "apiBase": "https://api.openai.com/v1",
      // `apiKey` is read at startup. Prefer an env-var indirection
      // (see §4) over a literal string in this file.
      "apiKey": "${OPENAI_API_KEY}",
      "extraHeaders": {}
    },

    // Uncomment to use ChatGPT Codex via the local `codex` CLI's OAuth
    // token. No API key — Zunel reads `~/.codex/auth.json`. The gateway
    // refreshes it automatically on a 30-min tick.
    // "codex": {},

    // Uncomment to use Amazon Bedrock. Credentials come from the
    // standard AWS chain (env, profile, IMDS); see configuration.md.
    // "bedrock": { "region": "us-west-2", "profile": "default" }
  },

  // ──────────────────────────────────────────────────────────────────────
  // 2. Agents — per-turn behaviour. `defaults` applies to every channel
  //    unless overridden below.
  // ──────────────────────────────────────────────────────────────────────
  "agents": {
    "defaults": {
      "provider": "custom",
      "model": "gpt-4o-mini",

      // Workspace root for files, sessions, skills, memory. `~` is
      // expanded; relative paths resolve against $HOME.
      "workspace": "~/.zunel/workspace",

      // Conversation hygiene — see §5 below.
      "sessionHistoryWindow": 40,   // recent messages replayed per turn
      "idleCompactAfterMinutes": 30, // compact older history if idle ≥ 30m
      "compactionKeepTail": 8,       // last N messages always kept verbatim
      "compactionModel": "gpt-4o-mini", // cheap model for the compaction LLM call

      // Bounds on the per-turn provider payload.
      "maxToolIterations": 15,
      "maxToolResultChars": 16000,
      "contextWindowTokens": 65536,

      // Optional: thinking-capable models. "low" / "medium" / "high".
      // "reasoningEffort": "medium",

      "timezone": "America/Los_Angeles"
    }
  },

  // ──────────────────────────────────────────────────────────────────────
  // 3. Channels — only `slack` is built in today. Omit the whole block
  //    for a CLI-only setup.
  // ──────────────────────────────────────────────────────────────────────
  "channels": {
    "showTokenFooter": false,    // when true, every Slack reply appends usage stats
    "slack": {
      "enabled": true,
      "mode": "socket",          // Socket Mode is the only supported transport

      // Slack tokens — prefer env-var indirection.
      "botToken": "${SLACK_BOT_TOKEN}",
      "appToken": "${SLACK_APP_TOKEN}",

      // Inbound allowlist. `["*"]` allows everyone; replace with
      // explicit Slack user IDs once you know who should reach the bot.
      "allowFrom": ["UABCDEF12", "UXYZ987"],

      // Group-channel posture: `mention` (only when @-mentioned) or
      // `open` (every message in the channel is seen).
      "groupPolicy": "mention",
      "groupAllowFrom": ["C0123456789"], // channel IDs the bot is in

      // UX touches.
      "replyInThread": true,
      "reactEmoji": "eyes",          // ack reaction on inbound
      "doneEmoji": "white_check_mark", // finalize reaction on turn end

      // DM posture. `dm.policy = "open"` means anyone who can DM the bot
      // (and is in `dm.allowFrom`, when set) gets a session.
      "dm": {
        "enabled": true,
        "policy": "open",
        "allowFrom": []
      },

      // Slack user-token MCP server hardening (see §6.2).
      "userTokenReadOnly": false,
      "writeAllow": []
    }
  },

  // ──────────────────────────────────────────────────────────────────────
  // 4. Tools — what the agent can call. Every tool defaults to OFF
  //    until the corresponding block sets `enable: true`.
  // ──────────────────────────────────────────────────────────────────────
  "tools": {
    // Human-in-the-loop approval. When `approvalRequired` is true, the
    // listed scope of tool calls pauses for an operator OK before
    // running. Scopes: "shell" (just `exec`), "writes" (file mutations
    // — write_file, edit_file, notebook_edit), or "all" (both).
    "approvalRequired": true,
    "approvalScope": "shell",

    "exec": {
      "enable": true,
      "defaultTimeoutSecs": 60,
      "maxTimeoutSecs": 600,
      // Extra env injected into every shell. Values support ${VAR} and
      // ${VAR:-fallback} expansion. Use this to extend PATH rather than
      // replace the parent process's environment.
      "env": {
        "PATH": "$HOME/.cargo/bin:$HOME/.local/bin:${PATH}",
        "EDITOR": "${EDITOR:-vim}"
      }
    },

    "web": {
      "enable": true,
      // "brave", "duckduckgo", or "stub" (returns a clear "not enabled"
      // error — useful for dev without an API key).
      "searchProvider": "brave",
      "braveApiKey": "${BRAVE_API_KEY}"
    },

    "filesystem": {
      // Media attachments downloaded from Slack land here. Leave unset
      // for `<workspace>/media/`.
      // "mediaDir": "/tmp/zunel-media"
    },

    // MCP servers — one entry per external tool surface. Stdio for
    // local processes (most npm-packaged MCP servers), `streamableHttp`
    // or `sse` for remote services.
    "mcpServers": {
      "filesystem": {
        "type": "stdio",
        "command": "npx",
        "args": [
          "-y",
          "@modelcontextprotocol/server-filesystem",
          "${HOME}/Documents/zunel-share"
        ],
        // Stdio MCP children inherit a minimal baseline env (PATH,
        // HOME, USER, LANG, LC_ALL, TZ) plus whatever you put here.
        // No other parent-process secrets reach the child. Use
        // `passthroughEnv` (see configuration.md) to forward extra
        // names like AWS_PROFILE on a per-server basis.
        "env": {},
        "toolTimeout": 60
      },

      "atlassian": {
        "type": "streamableHttp",
        "url": "https://mcp.atlassian.com/v1/sse",
        // OAuth2 with PKCE. `zunel mcp login atlassian` opens a browser
        // and writes the cached token to
        // `~/.zunel/mcp-oauth/atlassian/token.json` (mode 0600).
        // The gateway then refreshes it on a 30-min tick.
        "oauth": {
          "enabled": true,
          // clientId is optional — when missing, dynamic client
          // registration runs against the IdP's `/register` endpoint.
          "scope": "read:jira-work read:confluence-space.summary",
          "callbackHost": "127.0.0.1",
          "callbackPort": 33419
        },
        "enabledTools": ["*"],
        "toolTimeout": 120
      }
    }
  },

  // ──────────────────────────────────────────────────────────────────────
  // 5. CLI — local-only knobs.
  // ──────────────────────────────────────────────────────────────────────
  "cli": {
    "showTokenFooter": true   // append usage stats after every agent reply
  },

  // ──────────────────────────────────────────────────────────────────────
  // 6. AWS — only needed if you use `providers.bedrock` or the gateway's
  //    background SSO refresh.
  // ──────────────────────────────────────────────────────────────────────
  "aws": {
    "autoDiscoverProfiles": true,
    "ssoProfiles": [],
    "ssoExcludeProfiles": []
  }
}
```

## 4. Secrets via env vars

Every string value in `config.json` runs through `${VAR}` /
`${VAR:-fallback}` expansion. The recommended pattern: keep tokens in your
shell profile (or a 1Password CLI hook), reference them by name in
`config.json`:

```bash
# ~/.zshrc / ~/.bashrc
export OPENAI_API_KEY="$(op read 'op://Private/openai/key')"
export SLACK_BOT_TOKEN="xoxb-..."
export SLACK_APP_TOKEN="xapp-..."
export BRAVE_API_KEY="..."
```

```jsonc
{
  "providers": {
    "custom": { "apiKey": "${OPENAI_API_KEY}" }
  },
  "channels": {
    "slack": {
      "botToken": "${SLACK_BOT_TOKEN}",
      "appToken": "${SLACK_APP_TOKEN}"
    }
  }
}
```

`config.json` is `chmod 0600` from `zunel onboard` onwards, so a literal
token inside isn't a disaster — but env-var indirection keeps secrets out
of dotfile backups, `git` accidents, and `cat config.json`-in-a-screenshare.

## 5. Session hygiene at a glance

Every chat turn appends to `<workspace>/sessions/<key>.jsonl`. The full
history is on disk forever; the **per-turn payload** sent to the provider
is bounded by three knobs under `agents.defaults`:

| Knob | Default | What it does |
|---|---|---|
| `sessionHistoryWindow` | `40` | Replay only the most recent N unconsolidated messages each turn. |
| `idleCompactAfterMinutes` | unset | When the user has been idle for ≥ N minutes, LLM-summarise everything older than `compactionKeepTail` into a single system message. |
| `compactionKeepTail` | `8` | Tail size that compaction always preserves verbatim. |
| `compactionModel` | falls back to `model` | Cheaper model used for the compaction call (saves tokens on a $$$ main model). |

The Slack gateway funnels every per-user-or-channel session through a
single async task, so two messages in flight at once for the same key are
already serialised. Two separate `zunel` processes pointed at the same
workspace are also safe — sessions now hold an `fd-lock` for the
duration of each write.

## 6. Running it

### 6.1 Local agent (interactive REPL)

```bash
zunel agent
```

Type at the `you ›` prompt. Slash commands:

| Command | Effect |
|---|---|
| `/status` | Provider, model, session message count |
| `/sessions` | List session files |
| `/reload mcp [<server>]` | Live-reload MCP servers without restarting |
| `/clear` | Wipe the active session |
| `/exit` | Quit |

**Ctrl+C during a streaming turn** now cancels the turn cleanly (returns
`(turn cancelled)`); a second Ctrl+C at the line-edit prompt quits.

For one-shot prompts (suitable for shell scripts / cron):

```bash
zunel agent -m "what did I push to main today?"
```

### 6.2 Slack gateway (long-running)

The gateway is a single long-running process. The simplest production
path on macOS is the Homebrew service unit:

```bash
brew services start zunel-bot/tap/zunel
brew services restart zunel-bot/tap/zunel
tail -f /opt/homebrew/var/log/zunel-gateway.{out,err}.log
```

On Linux, a systemd unit:

```ini
# /etc/systemd/system/zunel-gateway.service
[Unit]
Description=Zunel Slack gateway
After=network.target

[Service]
Type=simple
ExecStart=/usr/bin/zunel gateway
Restart=on-failure
RestartSec=5s
User=zunel
Environment=RUST_LOG=info,zunel=info

[Install]
WantedBy=multi-user.target
```

The gateway runs five background refresh loops on top of the inbound
loop:

| Loop | Tick | What it refreshes |
|---|---|---|
| Slack bot token rotation | 30 min | The bot token if `<30 min` to expiry |
| MCP OAuth refresh | 30 min | Every remote MCP server with `oauth.enabled = true` |
| MCP auto-reconnect | 5 min | Re-dial any server that registered no tools at startup |
| AWS SSO refresh | 10 min | Every profile resolved from `~/.aws/config` + `aws.ssoProfiles` |
| Codex OAuth refresh | 30 min | `~/.codex/auth.json` when within 1 h of expiry |

Each loop is individually disable-able via env var (see
`docs/configuration.md`).

### 6.3 Slack user MCP (`slack_*` tools the agent can call as you)

When `channels.slack.enabled = true`, the runtime exposes a built-in MCP
server that lets the agent read Slack as you. Tools: `slack_search_public`,
`slack_search_threads`, `slack_read_channel`, `slack_read_thread`,
`slack_post_as_me`, `slack_dm_self`, plus a few helpers.

The user OAuth token is stored at `~/.zunel/slack-app-mcp/user_token.json`
(mode 0600). Log in once with:

```bash
zunel slack login
```

Two safety knobs sit on the Slack channel config:

```jsonc
"slack": {
  // Drop the write tools entirely. The agent never sees a handle to
  // chat.postMessage on your user token.
  "userTokenReadOnly": true,

  // OR: keep write tools but allowlist the destinations.
  "writeAllow": ["U_YOUR_USER_ID", "C_TEAM_CHANNEL"]
}
```

## 7. MCP servers in depth

MCP (Model Context Protocol) is how Zunel pulls in tools the runtime
doesn't ship natively: filesystem helpers, GitHub / Atlassian / Linear
search, internal company APIs, etc. Each entry under
`tools.mcpServers` becomes one or more tools on the agent's tool list,
prefixed `mcp_<name>_`:

```bash
zunel agent
you › what filesystem tools do I have?
zunel: mcp_filesystem_read_text_file, mcp_filesystem_list_directory, …
```

### 7.1 Transports

Three are supported. The runtime picks the default based on which fields
are present, but pass `"type"` explicitly when it matters:

| `type` | When to use | Required fields |
|---|---|---|
| `stdio` | Local process you spawn — the canonical npm-published MCP servers all run this way. | `command`, optional `args`, `env`, `passthroughEnv` |
| `streamableHttp` | Hosted MCP service that supports the newer streaming protocol (Atlassian, Linear, most enterprise SaaS). | `url`, optional `headers`, `oauth`, `enabledTools` |
| `sse` | Hosted MCP service that uses long-lived Server-Sent Events for the response side. | Same as `streamableHttp` |

`"type"` defaults to `stdio` when only `command` is set and
`streamableHttp` when only `url` is set, so you usually don't have to
write it.

### 7.2 Stdio servers — the full surface

```jsonc
"tools": {
  "mcpServers": {
    "filesystem": {
      "type": "stdio",
      "command": "npx",
      "args": [
        "-y",
        "@modelcontextprotocol/server-filesystem",
        "${HOME}/Documents/zunel-share"
      ],

      // Explicit env vars the child sees in addition to the minimal
      // baseline (PATH, HOME, USER, LANG, LC_ALL, TZ). Use this for
      // static values like API keys *to* the MCP server.
      "env": {
        "GITHUB_TOKEN": "${GITHUB_TOKEN}"
      },

      // Names of parent-process env vars to forward (their VALUE comes
      // from the gateway's env at spawn time). Use this when the
      // child needs a *dynamic* value the operator rotates, e.g.
      // `AWS_PROFILE` switched per shell session.
      "passthroughEnv": ["AWS_PROFILE"],

      // Subset filter on the tools/list response. ["*"] = expose all
      // (default). Useful when an MCP server exposes 40 tools and the
      // agent only needs three — fewer tool definitions = smaller
      // per-turn provider payload.
      "enabledTools": ["read_text_file", "list_directory", "search_files"],

      // Connection-time timeout (initialize handshake + tools/list).
      // Defaults to 10s.
      "initTimeout": 10,

      // Per-tool-call timeout. Defaults to 30s. Bump for slow servers
      // (e.g. a fileserver that scans large trees).
      "toolTimeout": 60
    }
  }
}
```

**Stdio child sandboxing** (v2.2.0+). The child process gets
`env_clear()` applied before spawn. Only the minimal baseline +
explicit `env` + `passthroughEnv` names reach it. A compromised
npm-published MCP server can no longer read `AWS_SECRET_ACCESS_KEY`,
`SLACK_BOT_TOKEN`, or any other parent secret on first connect. If a
server you trust genuinely needs (say) `OPENAI_API_KEY`, add it to
`passthroughEnv` — never assume environment inheritance.

### 7.3 Remote servers (HTTP / SSE)

Static bearer-token auth:

```jsonc
"github-mcp": {
  "type": "streamableHttp",
  "url": "https://api.github.example.com/mcp",
  "headers": {
    // ${VAR} expansion works here too. Headers with a `null`
    // expansion are dropped entirely, not sent as empty.
    "Authorization": "Bearer ${GITHUB_MCP_TOKEN}",
    "X-Org": "acme-corp"
  },
  "enabledTools": ["*"],
  "toolTimeout": 60
}
```

OAuth2 with PKCE (most enterprise SaaS):

```jsonc
"atlassian": {
  "type": "streamableHttp",
  "url": "https://mcp.atlassian.com/v1/sse",
  "oauth": {
    "enabled": true,

    // Optional: when omitted, dynamic client registration runs
    // against the IdP's /.well-known/oauth-authorization-server.
    "clientId": "registered-client-id",
    "clientSecret": "${ATLASSIAN_OAUTH_SECRET}",

    "scope": "read:jira-work read:confluence-space.summary",

    // The localhost callback Zunel binds for the redirect URI.
    "callbackHost": "127.0.0.1",
    "callbackPort": 33419,

    // Optional: override the redirect URI if you've registered a
    // public one with the IdP. Defaults to
    // `http://${callbackHost}:${callbackPort}/callback`.
    // "redirectUri": "https://oauth.your-tenant.example/callback"
  },
  "enabledTools": ["*"],
  "toolTimeout": 120
}
```

The cached token lives at
`~/.zunel/mcp-oauth/<server>/token.json` (mode `0600`). The gateway's
**MCP OAuth refresh loop** (30 min tick) rotates it transparently;
you don't need to relog unless the refresh token itself was revoked.

**SSE same-origin guard.** After the SSE handshake the server returns
the JSON-RPC endpoint URL. The runtime rejects any endpoint that
crosses origin (scheme + host + port) from the configured `url`, so a
compromised remote can't steer your bearer at `attacker.example`.

**Body-size cap.** Every remote MCP response is streamed through a 16 MiB
cap; the per-server `enabledTools` filter happens *after* parse, so
oversized payloads fail fast.

### 7.4 Logging in to a remote MCP server

For OAuth servers, the first tool call returns a synthetic
`MCP_AUTH_REQUIRED: <server>: <reason>` result. There are two ways to
complete the login:

**Option A — from a chat turn (Slack or REPL):**

The runtime ships a `mcp-oauth-login` skill. Ask the agent to "log in
to atlassian"; it'll call `mcp_login_start` (returns an authorize
URL), wait for you to paste back the full callback URL, and then call
`mcp_login_complete`. The token cache is written and `mcp_reconnect`
is triggered automatically.

**Option B — from the CLI:**

```bash
zunel mcp login atlassian          # opens browser, waits for callback
zunel mcp login atlassian --force  # re-login even if a token is cached
```

After a successful login the next `zunel agent` or running gateway
sees the server live (auto-reconnect picks it up on the 5-min tick,
or run `/reload mcp atlassian` to apply immediately).

### 7.5 Auto-registered `zunel_self` MCP server

When `tools.mcpServers` doesn't define an entry named `zunel_self`,
Zunel auto-registers one that lets the agent inspect its own runtime:
listed tools, recent sessions, active workspace path, channel
status. The tools are prefixed `mcp_zunel_self_*`. Disable by
defining an `mcpServers.zunel_self` with `"enabled": false`, or
override its config to point at a different self-MCP binary.

See [`self-tool.md`](./self-tool.md) for the full list of self-MCP
tools and what each returns.

### 7.6 Live operations

| Need to… | Run |
|---|---|
| Add or change a server without restarting | Edit `~/.zunel/config.json`, then in the REPL: `/reload mcp <name>` (or omit the name to reload every server) |
| Force-reconnect a server that came online after startup | Call the agent's `mcp_reconnect` tool with `server: "<name>"`, or `/reload mcp <name>` in the REPL |
| See which tools each server exposes right now | `zunel mcp doctor` (also reports connect + auth failures with their original error strings) |
| Inspect the OAuth token cache | `zunel mcp tokens` — prints expiry, refresh_at, and `obtainedAt` per server |
| Drop the cached OAuth token (force re-login next call) | `rm ~/.zunel/mcp-oauth/<server>/token.json` |
| Remove a server | Delete the entry from `tools.mcpServers`, then `/reload mcp <name>` |

The gateway's **MCP auto-reconnect loop** (5 min tick) periodically
re-dials any server that registered no tools at startup — e.g. an
internal MCP service that wasn't up yet when the gateway booted. No
manual intervention needed; the agent just starts seeing the server's
tools appear on the next turn after the loop reconnects.

### 7.7 Common gotchas

- **`npx` not on PATH.** `command: "npx"` needs `npx` reachable via
  `which` from the gateway's env. Set
  `tools.exec.env.PATH = "/opt/homebrew/bin:${PATH}"` (or the
  appropriate node prefix) so the child inherits a usable PATH. The
  baseline that reaches stdio MCP children already includes `PATH`
  from the gateway env; it's the *value of `PATH`* that needs to
  include node's bin dir.
- **`zunel mcp doctor` says "registered no tools" for a healthy
  server.** Usually means the server is hitting the `initTimeout` —
  bump it. Slow servers (Python-bootstrapped ones especially) can
  need 30+ seconds for first-call cold start.
- **OAuth callback fails on the IdP side.** The default redirect URI
  is `http://127.0.0.1:33419/callback`. Most IdPs require a literal
  registration of this URL on their side; check the OAuth app's
  "Authorized redirect URIs" config.
- **`MCP_AUTH_REQUIRED: …: token_expired` after a refresh window.**
  The refresh token itself is expired (typically 30 days idle for
  most SaaS IdPs). Run `zunel mcp login <server> --force`.

## 8. Verifying the setup

```bash
zunel status         # provider, model, workspace, channel count
zunel channels status # per-channel connect state
zunel tokens         # lifetime token usage per session
zunel sessions list  # active session keys
```

If `zunel status` says `channels: 0` and you expected `1`, the Slack
channel didn't load — usually a tokens issue. `RUST_LOG=info zunel status`
will surface the underlying error.

## 9. Security posture (what's protected, what isn't)

These are the controls the runtime ships with on v2.2.1. Most are
already-on defaults; a few opt-in knobs are called out.

| Concern | Protection |
|---|---|
| Shell tool abuse | **Approval gate** (`tools.approvalRequired = true`, scope `shell` / `writes` / `all`). On Linux with `bwrap` on `$PATH`, commands also run inside a mount namespace. On macOS, `sandbox-exec` wraps the shell and blocks reads against `~/.ssh`, `~/.aws`, `~/.zunel`, `~/.gnupg`, `~/.config`, `~/.docker`, `/Library/Keychains`, and `/private/var/db/sudo`. |
| Workspace escape via symlink | `PathPolicy` canonicalises through the filesystem before any read/write. A symlink inside the workspace pointing outside it is rejected. |
| SSRF / DNS rebinding | Every reqwest client in the workspace (web_fetch, MCP HTTP, OAuth discovery, openai_compat, codex) runs through `SsrfSafeResolver`, which re-validates each resolved IP against the private/loopback/link-local/IMDS blocklist. |
| MCP child env leak | Stdio MCP children get a scrubbed env: only `PATH`, `HOME`, `USER`, `LANG`, `LC_ALL`, `TZ`, plus the names you list under `passthroughEnv`, plus the explicit `env` map. `AWS_*`, `*_TOKEN`, provider API keys are *not* inherited. |
| Hostile MCP server | Stdio framing capped at 16 MiB; per-read inactivity timeouts (15 s) prevent slow-loris; non-conformant servers that stringify JSON-RPC ids are handled. |
| OAuth state CSRF / PKCE | Both use `getrandom::fill` for 32-byte secrets (no silent fallback to timestamp+pid). |
| Session corruption on crash | Session writes go via temp + fsync + rename, with cross-process advisory locking via `fd-lock`. Power-loss between rename and writeback can't publish a 0-byte file. |
| Credential file perms | `~/.zunel/config.json`, every cached OAuth token under `~/.zunel/mcp-oauth/`, the Slack app info, the codex auth blob, and the Slack user token are all `chmod 0600`. |

What's **not** protected by default:

- **Tool calls without approval.** Running with `approvalRequired = false`
  means the LLM can `exec`, `write_file`, fetch URLs, and call remote MCP
  servers without confirmation. Default config sets it `false` for
  convenience; flip to `true` for unattended deployments.
- **Network egress filtering.** The shell tool can still reach any
  reachable host outside the SSRF blocklist when run inside the
  sandbox. If you need an egress firewall, wire it in at the OS layer.
- **`zunel-mcp-self` on `0.0.0.0` with no `--api-key`.** Possible but
  warns loudly on startup; don't.

## 10. Common operations

```bash
# Inspect what the agent will see this turn.
zunel agent --dry-run -m "your prompt here"

# Trim a session to its last 20 messages (free space, drop early noise).
zunel sessions compact <key> --keep-tail 20

# Force-rotate the Slack bot token now (rather than waiting for the
# 30-min refresh tick).
zunel slack refresh-bot

# Validate that every configured MCP server connects and registers tools.
zunel mcp doctor

# See every OAuth-cached MCP token's expiry.
zunel mcp tokens

# Drop a stale lock file (sessions/<key>.jsonl.lock left over from a
# crashed process).
rm ~/.zunel/workspace/sessions/<key>.jsonl.lock
```

## 11. Where to go next

- [`configuration.md`](./configuration.md) — every config field in detail.
- [`chat-commands.md`](./chat-commands.md) — full slash-command reference.
- [`deployment.md`](./deployment.md) — systemd, launchd, Docker, Compose.
- [`memory.md`](./memory.md) — how `MEMORY.md` and `USER.md` feed the loop.
- [`self-tool.md`](./self-tool.md) — inspect the agent's runtime state from
  inside a chat turn.
- [`multiple-instances.md`](./multiple-instances.md) — running several
  isolated zunel processes (different model, different workspace) on the
  same host.
