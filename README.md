<p align="center">
  <img src="assets/avatar.png" alt="zunel avatar" width="128" height="128"/>
</p>

# zunel

[![CI](https://github.com/zunel-bot/zunel/actions/workflows/rust-ci.yml/badge.svg?branch=main)](https://github.com/zunel-bot/zunel/actions/workflows/rust-ci.yml)
[![Release](https://img.shields.io/github/v/release/zunel-bot/homebrew-tap?include_prereleases&sort=semver&display_name=tag&color=blue)](https://github.com/zunel-bot/homebrew-tap/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

Zunel is a personal AI assistant. It ships as a single binary and provides:

- a local chat REPL (`zunel agent`)
- a Slack gateway (`zunel gateway`) backed by Socket Mode
- pluggable LLM providers — any OpenAI-compatible endpoint, ChatGPT Codex via OAuth, or Amazon Bedrock
- built-in MCP servers for self-introspection and Slack
- cron, heartbeat, and durable memory consolidation ("Dream")
- approvals for sensitive tool calls and a local file/search/shell toolset

Config, runtime state, logs, cron data, Slack tokens, and the default workspace
live under `~/.zunel`.

## Install

```bash
# macOS / Linux — Homebrew
brew tap zunel-bot/tap && brew install zunel

# Debian / Ubuntu — pre-built .deb
ARCH=$(dpkg --print-architecture)
TAG=$(curl -sL https://api.github.com/repos/zunel-bot/homebrew-tap/releases/latest \
        | grep -o '"tag_name":[^,]*' | head -n1 | cut -d'"' -f4)
curl -fsSL -o /tmp/zunel.deb \
  "https://github.com/zunel-bot/homebrew-tap/releases/download/${TAG}/zunel-${ARCH}.deb"
sudo dpkg -i /tmp/zunel.deb

# Any platform with a Rust toolchain
cargo install --path rust/crates/zunel-cli
```

For development without installing, run directly out of the checkout:

```bash
cargo run --manifest-path rust/Cargo.toml -p zunel-cli -- agent
```

See [`docs/quick-start.md`](docs/quick-start.md) for the full install and
first-run walkthrough.

## Quick Start

1. Create the default config and workspace:

   ```bash
   zunel onboard
   ```

   This creates `~/.zunel/config.json` and `~/.zunel/workspace/`.

2. Edit `~/.zunel/config.json` and configure `providers.custom`:

   ```json
   {
     "providers": {
       "custom": {
         "apiKey": "sk-...",
         "apiBase": "https://api.openai.com/v1"
       }
     },
     "agents": {
       "defaults": {
         "provider": "custom",
         "model": "gpt-4o-mini"
       }
     }
   }
   ```

   Point `apiBase` at any OpenAI-compatible endpoint you want to use.

   Alternatively, if you are already signed in with the `codex` CLI, use the
   ChatGPT Codex Responses API without an API key:

   ```json
   {
     "providers": {
       "codex": {}
     },
     "agents": {
       "defaults": {
         "provider": "codex",
         "model": "gpt-5.4"
       }
     }
   }
   ```

3. Start a local chat session:

   ```bash
   zunel agent
   ```

   For a one-shot prompt:

   ```bash
   zunel agent -m "Hello, who are you?"
   ```

## Slack Gateway

Add Slack credentials to `~/.zunel/config.json`:

```json
{
  "channels": {
    "slack": {
      "enabled": true,
      "mode": "socket",
      "botToken": "xoxb-...",
      "appToken": "xapp-...",
      "allowFrom": ["*"]
    }
  }
}
```

Then start the gateway:

```bash
zunel gateway
```

The gateway runs Slack Socket Mode, cron, Dream, heartbeat, built-in MCP
servers, and remote approvals using the same workspace and agent defaults as
the CLI.

## Commands

```text
zunel onboard                  # initialize or refresh config + workspace
zunel agent                    # interactive CLI chat
zunel agent -m "..."           # one-shot prompt
zunel agent --show-tokens      # print a per-turn token-usage footer
zunel gateway                  # start the Slack-backed gateway
zunel status                   # show provider, model, workspace, channel count
zunel channels status          # show Slack channel status
zunel sessions list            # heaviest persisted sessions on disk
zunel sessions show <key>      # tail of a specific session
zunel sessions compact <key>   # LLM-summarize a bloated session
zunel sessions prune --older-than 30d  # delete stale sessions
zunel tokens                   # lifetime grand total across all sessions
zunel tokens list              # per-session token table sorted by total
zunel tokens show <key>        # per-turn breakdown for one session
zunel tokens since 7d          # rolling window roll-up
zunel slack login              # mint a Slack user token for the Slack MCP
zunel mcp serve --server self  # run the built-in self MCP server over stdio
zunel mcp serve --server slack # run the built-in Slack MCP server over stdio
zunel mcp login <server>       # OAuth-login a configured remote MCP server
zunel instance list            # list side-by-side instances
zunel instance use <name>      # set an instance as the sticky default
zunel instance show            # show the active instance and home dir
zunel instance rm <name>       # delete an instance (asks to confirm)
```

### Global flags

| Flag | Description |
|------|-------------|
| `--config <path>` | Override the config file path. Also readable from the `ZUNEL_CONFIG` environment variable; the flag wins when both are set |
| `-i <name>` / `--instance <name>` | Run any subcommand under `~/.zunel/instances/<name>/` (ignored when `ZUNEL_HOME` is set) |

See `docs/cli-reference.md` for the full per-subcommand options table.

`zunel slack login` opens your browser and captures the OAuth redirect on a
local HTTPS loopback (`https://127.0.0.1:53682/slack/callback`). The TLS cert
is auto-loaded from `~/.zunel/oauth-callback/{cert,key}.pem` if present;
otherwise the CLI generates a self-signed cert per run and your browser
warns once. Drop a `mkcert`-issued pair at that path to silence the warning
permanently — see
[`docs/configuration.md#slack-user-mcp-read-as-you`](docs/configuration.md#slack-user-mcp-read-as-you)
for the full setup and troubleshooting.

## Features

- local CLI agent (`zunel agent`)
- Slack Socket Mode gateway (`zunel gateway`)
- `providers.custom` OpenAI-compatible provider path
- `providers.codex` ChatGPT Codex OAuth path, requiring a working `codex` CLI login
- `providers.bedrock` Amazon Bedrock path, using the standard AWS credential chain
- built-in `self` and Slack MCP servers
- cron, heartbeat, Dream memory consolidation, approvals, and local tools
- config, workspace, and runtime state under `~/.zunel`

## Attribution

See [`NOTICE.md`](NOTICE.md) for the third-party libraries and tools this
project builds on.
