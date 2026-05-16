# Notices and Attributions

Zunel is released under the [MIT License](LICENSE). It builds on a
broader open-source ecosystem; this file collects the attributions and
notices that ecosystem makes available. Nothing here changes the
licensing of Zunel itself — it documents the licenses of components
Zunel depends on or interoperates with.

For the complete, exact, per-version dependency manifest see
`rust/Cargo.lock`. To regenerate the per-crate license summary on a
given checkout:

```bash
cargo install cargo-license
cd rust && cargo license
```

## External tools Zunel integrates with at runtime

These are not bundled with Zunel — users install them separately — but
Zunel's documentation and integrations reference them.

- [`codex` CLI](https://github.com/openai/codex) (OpenAI). Required for
  the `providers.codex` ChatGPT Codex provider; Zunel reads the OAuth
  token the `codex` CLI maintains under `$CODEX_HOME/auth.json` (or
  `~/.codex/auth.json`).
- [`mkcert`](https://github.com/FiloSottile/mkcert) (Filippo Valsorda).
  Optional helper referenced in the Slack OAuth callback setup so that
  the loopback HTTPS callback uses a browser-trusted certificate.
- [`cargo-dist`](https://opensource.axo.dev/cargo-dist/) (Axo). Release
  tooling referenced in CI configuration.

## Notable third-party Rust crates

Zunel depends on a large slice of the Rust ecosystem; the entries below
call out the crates whose vendors and projects most directly shape what
Zunel can do. License is the primary upstream license; some crates may
be additionally available under other terms — see the upstream `LICENSE`
file for full details.

### Networking, runtime, and TLS

- [`tokio`](https://github.com/tokio-rs/tokio) — async runtime
  (MIT).
- [`reqwest`](https://github.com/seanmonstar/reqwest) — HTTP client
  (Apache-2.0 OR MIT).
- [`hyper`](https://github.com/hyperium/hyper) — HTTP implementation
  (MIT).
- [`rustls`](https://github.com/rustls/rustls),
  [`tokio-rustls`](https://github.com/rustls/tokio-rustls),
  [`rustls-pemfile`](https://github.com/rustls/pemfile),
  [`webpki-roots`](https://github.com/rustls/webpki-roots) — TLS
  implementation, configured with the `ring` crypto provider
  ([`ring`](https://github.com/briansmith/ring), ISC license + others).
- [`rustls-platform-verifier`](https://github.com/rustls/rustls-platform-verifier)
  (Apache-2.0 OR MIT).

### AWS SDK for Rust

The `providers.bedrock` integration uses the AWS SDK for Rust
(Apache-2.0). Specifically:

- [`aws-config`](https://github.com/awslabs/aws-sdk-rust)
- [`aws-sdk-bedrockruntime`](https://github.com/awslabs/aws-sdk-rust)
- [`aws-smithy-types`](https://github.com/smithy-lang/smithy-rs)
- [`aws-sigv4`](https://github.com/smithy-lang/smithy-rs)

### MCP (Model Context Protocol)

Zunel speaks the [Model Context Protocol](https://modelcontextprotocol.io/),
an open specification originally published by Anthropic. The MCP
implementation in this repo (`rust/crates/zunel-mcp*`) is original code
written for Zunel; it interoperates with any MCP-compliant client or
server.

### Serialization, CLI, and TUI

- [`serde`](https://github.com/serde-rs/serde),
  [`serde_json`](https://github.com/serde-rs/json),
  [`serde_yaml`](https://github.com/dtolnay/serde-yaml) — serialization
  (Apache-2.0 OR MIT).
- [`clap`](https://github.com/clap-rs/clap) — CLI argument parser
  (Apache-2.0 OR MIT).
- [`reedline`](https://github.com/nushell/reedline) — line editor for
  the REPL (MIT).
- [`crossterm`](https://github.com/crossterm-rs/crossterm) — terminal
  control (MIT).
- [`ratatui`](https://github.com/ratatui/ratatui) — TUI rendering (MIT).
- [`minijinja`](https://github.com/mitsuhiko/minijinja) — Jinja-style
  template engine used for system prompts (Apache-2.0).

### Tracing, time, and supporting libraries

- [`tracing`](https://github.com/tokio-rs/tracing) (MIT).
- [`chrono`](https://github.com/chronotope/chrono) (Apache-2.0 OR MIT).
- [`tiktoken-rs`](https://github.com/zurawiki/tiktoken-rs) — token
  counting (MIT).

### Testing

- [`wiremock`](https://github.com/LukeMathWalker/wiremock-rs)
  (Apache-2.0 OR MIT).
- [`tempfile`](https://github.com/Stebalien/tempfile)
  (Apache-2.0 OR MIT).

## Trademarks

Names of products and services Zunel integrates with — including but not
limited to ChatGPT, Codex, Slack, AWS, Amazon Bedrock, Anthropic, and
Claude — are trademarks or registered trademarks of their respective
owners. Zunel is not affiliated with, endorsed by, or sponsored by any
of these vendors. Their names appear in this repository only to identify
the protocols and APIs Zunel speaks.
