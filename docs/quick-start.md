# Install and Quick Start

## Install

Zunel ships as a single statically-linked binary (`zunel`). Pick whichever
install path matches your OS — all four produce the same `zunel` on `$PATH`
and the same `~/.zunel/` runtime state on first run.

### macOS / Linux — Homebrew

```bash
brew tap zunel-bot/tap
brew install zunel
```

The tap repo at `github.com/zunel-bot/homebrew-tap` is auto-updated by the
release pipeline (`.github/workflows/release.yml`) on every `vN.N.N` tag.
The formula points at the per-arch tarballs published as a GitHub
Release on the **same** tap repo (the tap doubles as the public binary
host), so `brew install` downloads a pre-built binary instead of
compiling from source.

### Debian / Ubuntu — `.deb`

The release pipeline (`.github/workflows/deb.yml`) attaches per-arch `.deb`
files to every GitHub Release. Install with plain `dpkg`:

```bash
ARCH=$(dpkg --print-architecture)               # amd64 or arm64
TAG=$(curl -sL https://api.github.com/repos/zunel-bot/homebrew-tap/releases/latest \
        | grep -o '"tag_name":\s*"[^"]*"' | head -n1 | cut -d'"' -f4)
curl -fsSL -o /tmp/zunel.deb \
  "https://github.com/zunel-bot/homebrew-tap/releases/download/${TAG}/zunel-${ARCH}.deb"
sudo dpkg -i /tmp/zunel.deb
```

The package depends on nothing beyond `ca-certificates` (recommended,
already present on most systems); the binary is statically linked
against musl + rustls, so there's no `libssl` / `libc` version coupling.

### From source — any platform with Rust

```bash
cargo install --path rust/crates/zunel-cli
```

Or run directly out of a checkout without installing:

```bash
cargo run --manifest-path rust/Cargo.toml -p zunel-cli -- agent
```

No extra browser-side or multi-channel setup is required.

## Quick Start

### 1. Initialize

```bash
zunel onboard
```

This creates:

- `~/.zunel/config.json`
- `~/.zunel/workspace/`

Use `zunel onboard --force` to regenerate the default config.

### 2. Configure a provider

Zunel ships three provider paths. Pick one by editing `~/.zunel/config.json`.

#### Option A — OpenAI-compatible endpoint (`providers.custom`)

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

`apiBase` can point at any OpenAI-compatible service you trust. `apiKey` is
required by the current runtime, even if your endpoint uses a placeholder value.

#### Option B — ChatGPT Codex via local OAuth (`providers.codex`)

First, sign in with the `codex` CLI (once, using your ChatGPT account). Then:

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

No API key is needed — `zunel` reads the local Codex OAuth token. If you
haven't signed in yet, `zunel` will return a clear error pointing you to
`codex` CLI login.

### 3. Start the local agent

```bash
zunel agent
```

For a one-shot prompt:

```bash
zunel agent -m "Summarize this repo."
```

### 4. Optional: enable Slack and start the gateway

Add a Slack block to `~/.zunel/config.json`:

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

Then run:

```bash
zunel gateway
```

Slack is the only built-in gateway/channel path.

### 5. Sanity-check the setup

```bash
zunel status
zunel channels status
```

`zunel status` prints the active runtime summary:

```text
provider: custom
model: gpt-4o-mini
workspace: /Users/you/.zunel/workspace
channels: 1
```

`channels` is `1` when the Slack channel is configured and `0` when it is not.

At this point you are ready to use:

- `zunel agent` for local work
- `zunel gateway` for Slack-backed automation and chat
