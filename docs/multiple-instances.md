# Multiple Instances

Run multiple Zunel instances with separate configs, workspaces, cron state, and
Slack credentials.

This is useful when you want to keep a local CLI instance separate from one or
more long-running Slack gateways.

The main switch is `--instance` or `ZUNEL_HOME`. Each instance maps to a distinct
home directory with its own `config.json`, workspace, OAuth tokens, cron data,
and Slack settings.

## Quick Start

Initialize a few isolated instances:

```bash
zunel --instance local onboard
zunel --instance slack onboard
zunel --instance staging onboard
```

Typical roles:

- `~/.zunel/instances/local/` for local CLI work
- `~/.zunel/instances/slack/` for your primary Slack gateway
- `~/.zunel/instances/staging/` for a second gateway, test endpoint, or alternate model

## Run Instances

```bash
# Local-only CLI instance
zunel --instance local agent

# Main Slack gateway
zunel --instance slack gateway

# Second gateway against a different workspace / Slack app
zunel --instance staging gateway
```

`zunel agent` always starts a local agent loop. It does not attach to a running
gateway process.

## Path Resolution

| Component | Resolved from | Example |
|-----------|---------------|---------|
| Home | `--instance` or `ZUNEL_HOME` | `~/.zunel/instances/slack/` |
| Config | home directory | `~/.zunel/instances/slack/config.json` |
| Workspace | `agents.defaults.workspace` | `~/.zunel/instances/slack/workspace/` |
| Cron data | workspace directory | `~/.zunel/instances/slack/workspace/cron/` |
| Media/runtime state | home directory | `~/.zunel/instances/slack/media/` |

## Minimal Per-Instance Config

Each instance can point to a different model, endpoint, workspace, or Slack app:

```json
{
  "agents": {
    "defaults": {
      "workspace": "~/.zunel/instances/slack/workspace",
      "provider": "custom",
      "model": "gpt-4o-mini"
    }
  },
  "providers": {
    "custom": {
      "apiKey": "sk-...",
      "apiBase": "https://api.openai.com/v1"
    }
  },
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

## One-Off Overrides

For an arbitrary home directory, set `ZUNEL_HOME`:

```bash
ZUNEL_HOME=/tmp/zunel-local-test zunel onboard
ZUNEL_HOME=/tmp/zunel-local-test zunel agent
ZUNEL_HOME=/tmp/zunel-staging-test zunel gateway
```

To change only the workspace while keeping the same home, edit
`agents.defaults.workspace` in that instance's `config.json`.

## Common Uses

- Keep personal CLI work separate from a long-running Slack gateway
- Split production and staging gateways
- Try different OpenAI-compatible endpoints without mixing memory or cron state
- Isolate experiments in temporary workspaces

## Notes

- Keep each instance on its own workspace if you want isolated sessions and memory.
- Each gateway must use distinct Slack credentials (`botToken` / `appToken`) if
  you run them at the same time.
- Empty Slack `allowFrom` lists deny access; use explicit IDs or `["*"]`.
- The gateway does not bind a port. If you need a liveness check, use your
  process supervisor (systemd, Docker, launchd).

## Migrating from `profile`

If you upgraded from a build that used `--profile`, your data lives at
`~/.zunel/profiles/<name>/`. The new resolver refuses to start while the legacy
directory is present and prints the exact `mv` command to run. For most setups:

```bash
mv ~/.zunel/profiles ~/.zunel/instances
mv ~/.zunel/active_profile ~/.zunel/active_instance   # if it exists
```

After the rename, switch any shell aliases or service unit files from
`--profile` / `zunel profile` to `--instance` / `zunel instance`.
