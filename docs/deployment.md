# Deployment

Zunel has two deployment modes:

- run `zunel agent` locally when you want an interactive CLI
- run `zunel gateway` as a long-lived Slack gateway

Containers and services run the same CLI and gateway entrypoints documented
here.

## Docker Compose

The bundled `docker-compose.yml` defines two services:

- `zunel-gateway` for the long-running Slack gateway
- `zunel-cli` for one-off interactive or maintenance commands (gated behind
  the `cli` Compose profile so `docker compose up` does not start it)

First-time setup:

```bash
docker compose --profile cli run --rm zunel-cli onboard
$EDITOR ~/.zunel/config.json
docker compose up -d zunel-gateway
```

Common operations:

```bash
docker compose --profile cli run --rm zunel-cli agent -m "Hello!"
docker compose --profile cli run --rm zunel-cli status
docker compose logs -f zunel-gateway
docker compose down
```

The `--profile cli` flag is required because `zunel-cli` is gated behind the
`cli` Compose profile in `docker-compose.yml`. Without the flag, recent
Docker Compose versions warn and skip the service.

### Sandbox / capability trade-off

Both services in `docker-compose.yml` start with `cap_drop: [ALL]` and then
add back `cap_add: [SYS_ADMIN]`, plus `security_opt: [apparmor=unconfined,
seccomp=unconfined]`. This is required because zunel's `exec` tool wraps
every untrusted command in `bubblewrap`, which creates new user / mount /
PID namespaces via `unshare(CLONE_NEW*)`. Docker's default seccomp and
AppArmor profiles block those syscalls, and the default `--cap-drop ALL`
strips `CAP_SYS_ADMIN`, so without the relaxations bubblewrap fails and the
agent's `exec` calls silently degrade.

The trade-off is that the container is *less* isolated from the host kernel
than a default Docker container. The mitigations are:

- the agent process still runs unprivileged (UID 1000)
- `bubblewrap` re-imposes its own sandbox (read-only `/`, ephemeral writable
  `/tmp`, no host network unless the tool explicitly requests it) on every
  `exec` call
- the only writable host path is the bind-mounted `~/.zunel`

In practice this puts the container at roughly the isolation level of
running `zunel` directly on the host. If you need stricter isolation (for
example, running on a multi-tenant host) prefer running `zunel-gateway`
inside a dedicated VM.

## Direct Docker

```bash
# Build the image
docker build -t zunel .

# Initialize config (first time only)
docker run -v ~/.zunel:/home/zunel/.zunel --rm zunel onboard

# Edit config on the host
$EDITOR ~/.zunel/config.json

# Start the Slack gateway
docker run -v ~/.zunel:/home/zunel/.zunel zunel gateway

# Run one-off CLI commands
docker run -v ~/.zunel:/home/zunel/.zunel --rm zunel agent -m "Hello!"
docker run -v ~/.zunel:/home/zunel/.zunel --rm zunel status
```

> [!TIP]
> Mount `~/.zunel` into the container so config, workspace, cron data, and logs
> persist across restarts.
>
> The image runs as user `zunel` (UID 1000). If you hit a permission error on
> the mounted directory, either fix ownership on the host with
> `sudo chown -R 1000:1000 ~/.zunel` or run the container as your own UID with
> `--user $(id -u):$(id -g)`. Podman users can use `--userns=keep-id`.

## Communication Surfaces

`zunel gateway` does not bind any network port. The only ways to talk to a
running gateway are:

- the **Slack channel** configured under `channels.slack` (if enabled)
- the **`zunel` CLI** on the same host against the same `--instance` or
  `ZUNEL_HOME`

There is no HTTP server, health endpoint, or webhook listener. If you need a
liveness signal, use the process supervisor (systemd, Docker, launchd) — if
`zunel gateway` is running, the services are up.

## Linux Service

For a persistent Slack gateway on Linux, run `zunel gateway` as a systemd user
service.

### 1. Find the binary path

```bash
which zunel
```

### 2. Create `~/.config/systemd/user/zunel-gateway.service`

```ini
[Unit]
Description=Zunel Gateway
After=network.target

[Service]
Type=simple
ExecStart=%h/.local/bin/zunel gateway
Restart=always
RestartSec=10
NoNewPrivileges=yes
ProtectSystem=strict
ReadWritePaths=%h

[Install]
WantedBy=default.target
```

### 3. Enable and start it

```bash
systemctl --user daemon-reload
systemctl --user enable --now zunel-gateway
```

Common operations:

```bash
systemctl --user status zunel-gateway
systemctl --user restart zunel-gateway
journalctl --user -u zunel-gateway -f
```

If you edit the service file itself, run `systemctl --user daemon-reload`
before restarting.

To keep the user service running after logout:

```bash
loginctl enable-linger $USER
```

## macOS Service (Homebrew)

For a persistent Slack gateway on macOS, the simplest path is
`brew services`. The published Homebrew formula at
[`zunel-bot/homebrew-tap`](https://github.com/zunel-bot/homebrew-tap)
already declares a `service` block that runs `zunel gateway` with
`keep_alive true`, so all you need is:

```bash
brew tap zunel-bot/tap
brew install zunel
brew services start zunel-bot/tap/zunel
```

Common operations:

```bash
brew services list                                   # status
brew services restart zunel-bot/tap/zunel
brew services stop zunel-bot/tap/zunel
tail -f /opt/homebrew/var/log/zunel-gateway.{out,err}.log
```

The Homebrew formula sets two `EnvironmentVariables` for the gateway:

| Var       | Value                                                                                       | Why                                                                                                           |
| --------- | ------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------- |
| `RUST_LOG`| `info,zunel=info`                                                                           | Enable info-level tracing for the gateway loops.                                                              |
| `PATH`    | `/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin:/usr/local/sbin:/usr/bin:/bin:/usr/sbin:/sbin` | macOS launchd starts brew-services jobs with a stripped PATH (`/usr/bin:/bin:/usr/sbin:/sbin`) that the agent's `ExecTool` would otherwise inherit. Pre-seeding the canonical brew + system prefixes lets the agent invoke `brew`-installed binaries (`git`, `gh`, `jq`, `node`, `psql`, `kubectl`, …) when driven via Slack. Both Apple Silicon (`/opt/homebrew/...`) and Intel (`/usr/local/...`) prefixes are included so the same formula serves both. |

To extend (e.g. add `~/.cargo/bin` for cargo-installed tooling), edit the
local plist and reload — but note that `brew upgrade` / `brew services
restart` regenerates the plist from the formula and overwrites manual
edits:

```bash
plutil -insert EnvironmentVariables.PATH \
  -string "$HOME/.cargo/bin:/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin:/usr/local/sbin:/usr/bin:/bin:/usr/sbin:/sbin" \
  ~/Library/LaunchAgents/homebrew.mxcl.zunel.plist
launchctl bootout gui/$UID/homebrew.mxcl.zunel
launchctl bootstrap gui/$UID ~/Library/LaunchAgents/homebrew.mxcl.zunel.plist
```

The persistent path is to bake new env vars into the formula via
`.github/workflows/release.yml` (search `service_block = """`) and
ship them in the next release; that survives upgrades for everyone.

### Slack bot token rotation: handled in-runtime

If you've enabled bot-token rotation on your Slack app
(`<zunel_home>/slack-app/app_info.json` exists with
`bot_refresh_token` and `bot_token_expires_at` populated), the gateway
runtime spawns a background task that refreshes the rotating bot
token whenever it has less than 30 minutes of life left. The check
runs at gateway startup and then every 30 minutes for the lifetime of
the process. Refresh failures are logged at `WARN` and never crash
the gateway.

When a refresh succeeds, the new token is **spliced into the live
`SlackChannel` in-process** in addition to being written to disk. The
next outbound `chat.postMessage` and any inbound reaction or file
download immediately use the fresh token — no gateway restart is
required. The boot log line records this with `in_process_swap=true`.

This means **no external wrapper script is required for bot rotation**:
`brew services start zunel` is enough. The runtime calls exactly the
same code path `zunel slack refresh-bot --if-near-expiry 1800` does.

You can still invoke `zunel slack refresh-bot` from the shell or a
launchd / systemd timer if you prefer eager refreshes outside the
30-minute window — the next gateway tick (or the next outbound send)
will pick the new token off disk on its own.

Tunables (env vars on the gateway process):

| Env var                          | Default | Meaning                                      |
| -------------------------------- | ------- | -------------------------------------------- |
| `ZUNEL_BOT_REFRESH_TICK_SECS`    | `1800`  | How often the Slack bot-token refresh task wakes up |
| `ZUNEL_BOT_REFRESH_WINDOW_SECS`  | `1800`  | Refresh the Slack bot token when it has less than this many seconds left |
| `ZUNEL_MCP_REFRESH_TICK_SECS`    | `1800`  | How often the remote-MCP OAuth refresh task wakes up. Walks every OAuth-enabled server in `tools.mcpServers` and rotates `~/.zunel/mcp-oauth/<server>/token.json` via the cached `refresh_token`. |
| `ZUNEL_MCP_REFRESH_DISABLED`     | unset   | Set to `1` / `true` / `yes` to skip spawning the remote-MCP refresh task entirely |
| `ZUNEL_MCP_RECONNECT_TICK_SECS`  | `300`   | How often the MCP auto-reconnect task wakes up. For each configured MCP server with no live tools (and no `mcp_<name>_login_required` stub), tries to register it again so a server that was down at boot heals itself once it comes back online. |
| `ZUNEL_MCP_RECONNECT_DISABLED`   | unset   | Set to `1` / `true` / `yes` to skip spawning the MCP auto-reconnect task |
| `ZUNEL_CODEX_REFRESH_TICK_SECS`  | `1800`  | How often the ChatGPT-Codex access_token refresh task wakes up. Decodes `~/.codex/auth.json`'s cached `tokens.access_token` JWT and rotates via `auth.openai.com/oauth/token` when inside `ZUNEL_CODEX_REFRESH_WINDOW_SECS` of expiry. Only spawns when `agents.defaults.provider = "codex"`. |
| `ZUNEL_CODEX_REFRESH_WINDOW_SECS`| `3600`  | Refresh the Codex access_token when it has fewer than this many seconds left. Default 1h; the access_token's full lifetime is 10 days, so the loop fires ~6 ticks before any agent reaches for a stale token. |
| `ZUNEL_CODEX_REFRESH_DISABLED`   | unset   | Set to `1` / `true` / `yes` to skip spawning the Codex refresh task entirely. |
| `CODEX_HOME`                     | `~/.codex` | Override the codex CLI home directory (mirrors the codex CLI). Used to locate `auth.json`. |
| `CODEX_REFRESH_TOKEN_URL_OVERRIDE` | `https://auth.openai.com/oauth/token` | Override the Codex refresh endpoint (mirrors the codex CLI). Used by tests and air-gapped deployments. |
| `ZUNEL_DISABLE_SELF_MCP`         | unset   | Set to `1` / `true` / `yes` to skip auto-registering the built-in self stdio MCP server (`zunel_self`). Use when you've manually wired a `--server self` entry of your own or want a stripped tool registry. |

Users without bot rotation (no `slack-app/app_info.json` on disk) pay
no cost — the bot-refresh task simply doesn't spawn. The MCP-refresh
task is similarly cheap: it self-disables when no remote MCP server has
`oauth.enabled = true` in `config.json`. The Codex-refresh task is
similarly self-gating: it skips spawning when `agents.defaults.provider`
is anything other than `"codex"`, and a single tick is one HTTP POST
(no filesystem write) when the cached access_token is comfortably fresh.

### Custom LaunchAgent (advanced)

You can still hand-roll a `~/Library/LaunchAgents/com.zunel.gateway.plist`
LaunchAgent if you need finer control (different `RUST_LOG`, custom
log paths outside `/opt/homebrew/var/log`, etc.). The minimum plist
mirrors what `brew services` generates:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
  <key>Label</key>            <string>com.zunel.gateway</string>
  <key>ProgramArguments</key> <array><string>/opt/homebrew/bin/zunel</string><string>gateway</string></array>
  <key>RunAtLoad</key>        <true/>
  <key>KeepAlive</key>        <dict><key>Crashed</key><true/><key>SuccessfulExit</key><false/></dict>
  <key>ThrottleInterval</key> <integer>30</integer>
  <key>StandardOutPath</key>  <string>/Users/you/.zunel/logs/gateway.out.log</string>
  <key>StandardErrorPath</key><string>/Users/you/.zunel/logs/gateway.err.log</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>RUST_LOG</key> <string>info,zunel=info</string>
  </dict>
  <key>ProcessType</key>      <string>Background</string>
</dict>
</plist>
```

Bot-token rotation is still handled by the in-runtime task above — you
don't need an external `zunel slack refresh-bot` wrapper script or a
periodic kicker LaunchAgent.

Don't run both `brew services start zunel` and a custom LaunchAgent
against the same `~/.zunel/` workspace: two `zunel gateway`
processes will race over Slack Socket connections and session writes.
