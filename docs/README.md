# Zunel Docs

These docs cover the supported Zunel surfaces:

- local CLI usage with `zunel agent`
- Slack gateway deployment with `zunel gateway`
- OpenAI-compatible provider setup through `providers.custom`
- ChatGPT Codex (OAuth) provider setup through `providers.codex`
- Amazon Bedrock provider setup through `providers.bedrock`

## Start Here

| Topic | Repo doc | What it covers |
|---|---|---|
| Install and quick start | [`quick-start.md`](./quick-start.md) | Installation, onboarding, first local chat, and first Slack gateway run |
| User guide | [`user-guide.md`](./user-guide.md) | Task-oriented walkthrough: annotated full `config.json`, MCP servers in depth (transports, OAuth, env scrubbing, login flows), gateway/service setup, and the v2.2.1 security posture |
| Configuration | [`configuration.md`](./configuration.md) | Agent defaults, providers, Slack, tools, MCP, and security |
| CLI reference | [`cli-reference.md`](./cli-reference.md) | Supported commands and common flags |
| Deployment | [`deployment.md`](./deployment.md) | Docker, Compose, and systemd setup for the gateway and CLI |

## Day-To-Day Usage

| Topic | Repo doc | What it covers |
|---|---|---|
| In-chat commands | [`chat-commands.md`](./chat-commands.md) | Slash commands, heartbeat behavior, and Dream commands |
| Multiple instances | [`multiple-instances.md`](./multiple-instances.md) | Separate configs, workspaces, and gateway ports |
| Memory | [`memory.md`](./memory.md) | How Zunel summarizes history and maintains durable memory |
| Self tool | [`self-tool.md`](./self-tool.md) | Inspect the agent's read-only runtime state |
| Instance-as-MCP | [`instance-as-mcp.md`](./instance-as-mcp.md) | Expose a named instance's tool registry as a Streamable HTTP/HTTPS MCP server (`zunel mcp agent`) |
| Instance-as-MCP Mode 2 | [`instance-as-mcp-mode2.md`](./instance-as-mcp-mode2.md) | Draft design for `helper_ask` — running a helper instance's full agent loop as a single MCP tool. Not yet implemented. |
