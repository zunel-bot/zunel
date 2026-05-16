use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "zunel",
    version,
    about = "zunel — a lean personal AI assistant"
)]
pub struct Cli {
    /// Override the config file path (default: ~/.zunel/config.json).
    #[arg(long, global = true, env = "ZUNEL_CONFIG")]
    pub config: Option<PathBuf>,

    /// Use a named instance home under ~/.zunel/instances/ (ignored when ZUNEL_HOME is set).
    #[arg(short = 'i', long, global = true)]
    pub instance: Option<String>,

    /// Bypass the workspace foot-gun guard. Without this, `zunel`
    /// refuses to start the agent (or onboard) when the resolved
    /// workspace is the filesystem root, the user's `$HOME`, or
    /// an ancestor of the active instance's `~/.zunel` directory —
    /// any of which would let `write_file`/`exec`/etc. punch
    /// through the workspace sandbox.
    ///
    /// Equivalent to setting `ZUNEL_ALLOW_UNSAFE_WORKSPACE=1`
    /// for the duration of this invocation.
    #[arg(long = "i-know-what-im-doing", global = true)]
    pub i_know_what_im_doing: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Initialize or refresh the zunel home directory.
    Onboard(OnboardArgs),
    /// Run the agent against a one-shot prompt.
    Agent(AgentArgs),
    /// Run the long-lived gateway service.
    Gateway(GatewayArgs),
    /// Show provider, model, and workspace status.
    Status,
    /// MCP helper commands.
    Mcp(McpArgs),
    /// Manage side-by-side zunel instances.
    Instance(InstanceArgs),
    /// Slack user-token helper commands.
    Slack(SlackArgs),
    /// Inspect configured gateway channels.
    Channels(ChannelsArgs),
    /// Inspect, compact, or prune persisted chat sessions.
    Sessions(SessionsArgs),
    /// Inspect token usage across persisted sessions.
    Tokens(TokensArgs),
}

#[derive(Debug, Parser)]
pub struct OnboardArgs {
    /// Overwrite an existing config file.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Parser)]
pub struct AgentArgs {
    /// One-shot message to send. Without this, drops into an interactive REPL.
    #[arg(short = 'm', long = "message")]
    pub message: Option<String>,

    /// Session ID (channel:chat_id). Defaults to `cli:direct`.
    #[arg(short = 's', long = "session", default_value = "cli:direct")]
    pub session: String,

    /// Print a one-line token-usage footer after every assistant reply.
    /// Equivalent to setting `cli.showTokenFooter: true` in config but
    /// scoped to this invocation only.
    #[arg(long = "show-tokens")]
    pub show_tokens: bool,
}

#[derive(Debug, Parser)]
pub struct GatewayArgs {
    /// Validate gateway wiring and exit without connecting external services.
    #[arg(long)]
    pub dry_run: bool,
    /// Start channels and exit immediately; intended for automated tests.
    #[arg(long, hide = true)]
    pub startup_only: bool,
    /// Process at most N inbound messages before exiting; intended for automated tests.
    #[arg(long, hide = true)]
    pub max_inbound: Option<usize>,
}

#[derive(Debug, Parser)]
pub struct ChannelsArgs {
    #[command(subcommand)]
    pub command: ChannelsCommand,
}

#[derive(Debug, Parser)]
pub struct McpArgs {
    #[command(subcommand)]
    pub command: McpCommand,
}

#[derive(Debug, Subcommand)]
pub enum McpCommand {
    /// Run a built-in MCP server over stdio.
    Serve(McpServeArgs),
    /// Authenticate a configured remote MCP server.
    Login(McpLoginArgs),
    /// Serve the active instance's tool registry as a Streamable
    /// HTTP/HTTPS MCP server.
    ///
    /// Use the global `--instance NAME` flag to choose which instance's
    /// configuration and workspace to expose. Defaults bind to
    /// loopback only; expose on a non-loopback address only when
    /// pairing `--https-cert/--https-key` with `--api-key` (or
    /// `--api-key-file`).
    ///
    /// `McpAgentArgs` is boxed to keep the enum's stack footprint
    /// small — the struct grew past 300 bytes once the Mode 2 flags
    /// were added, and clippy flags the size delta between variants.
    Agent(Box<McpAgentArgs>),
}

#[derive(Debug, Parser)]
pub struct McpServeArgs {
    /// Built-in MCP server to run.
    #[arg(long, default_value = "self", value_parser = ["self", "slack"])]
    pub server: String,
}

#[derive(Debug, Parser)]
pub struct McpAgentArgs {
    /// HOST:PORT to bind. Default `127.0.0.1:0` selects an OS port on
    /// loopback so the command always starts safely.
    #[arg(long, default_value = "127.0.0.1:0")]
    pub bind: String,

    /// PEM-encoded TLS certificate. Pair with `--https-key` to
    /// terminate HTTPS in-process. Required when `--bind` resolves to
    /// a non-loopback address.
    #[arg(long = "https-cert")]
    pub https_cert: Option<PathBuf>,

    /// PEM-encoded TLS private key. Pair with `--https-cert`.
    #[arg(long = "https-key")]
    pub https_key: Option<PathBuf>,

    /// Bearer token to require on every `POST /` request. May be
    /// repeated to allow several tokens at once for zero-downtime
    /// rotation. Required when `--bind` resolves to a non-loopback
    /// address.
    #[arg(long = "api-key")]
    pub api_key: Vec<String>,

    /// Path to a file containing one bearer token per non-blank line
    /// (lines starting with `#` are treated as comments). May be
    /// repeated; tokens stack across `--api-key` and
    /// `--api-key-file`.
    #[arg(long = "api-key-file")]
    pub api_key_file: Vec<PathBuf>,

    /// Allowed `Origin` header value. May be repeated. When provided,
    /// any literal `Origin` header on a `POST /` request must match
    /// (case-insensitively); requests without an `Origin` header (or
    /// with the literal `null`) bypass the check.
    #[arg(long = "allow-origin")]
    pub allow_origin: Vec<String>,

    /// Maximum permitted `Mcp-Call-Depth` header value. Requests with
    /// a depth `>=` this value are rejected with `403`. Default is
    /// the library default (currently 8).
    #[arg(long = "max-call-depth")]
    pub max_call_depth: Option<u32>,

    /// Hard cap on the request body, in bytes. Requests with a
    /// `Content-Length` greater than this are answered with
    /// `413 Payload Too Large` *before* the body is read off the
    /// socket. Accepts `K`/`M`/`G` suffixes (base-1024). When
    /// omitted, the library default applies.
    #[arg(long = "max-body-bytes")]
    pub max_body_bytes: Option<String>,

    /// Emit one JSON line per served request to the given path.
    /// Use `-` for stdout. The file is opened in append mode so
    /// `logrotate` `copytruncate` rotation works without the agent
    /// needing to re-open on a signal. Each entry contains the
    /// timestamp, peer addr, JSON-RPC method, tool name (when
    /// applicable), `Mcp-Call-Depth`, the matched bearer token's
    /// fingerprint (never the token itself), HTTP status, and
    /// wall-clock latency. See `docs/instance-as-mcp.md` for the
    /// full schema.
    #[arg(long = "access-log")]
    pub access_log: Option<String>,

    /// Expose `write_file`, `edit_file`, and `cron`. Off by default;
    /// without it the agent server is read-only.
    #[arg(long = "allow-write")]
    pub allow_write: bool,

    /// Expose `exec`. Off by default.
    #[arg(long = "allow-exec")]
    pub allow_exec: bool,

    /// Expose `web_fetch` and `web_search`. Off by default.
    #[arg(long = "allow-web")]
    pub allow_web: bool,

    /// Enable Mode 2 (`helper_ask`) — register a single tool that
    /// runs a full `AgentLoop` inside this instance and returns the
    /// answer. Off by default; Mode 1's filtered registry stays
    /// unchanged. See `docs/instance-as-mcp-mode2.md`.
    #[arg(long = "mode2")]
    pub mode2: bool,

    /// Approval policy for tool calls *inside* the helper's
    /// `AgentLoop`. `reject` (default) fails the helper turn the
    /// moment any tool requires approval — there is no human in the
    /// loop on the helper side. `allow_all` auto-approves every
    /// gated call; only sensible for fully read-only helpers run by
    /// trusted operators. `forward` enqueues the request on a
    /// shared queue and exposes `helper_pending_approvals` /
    /// `helper_approve` so the hub can resolve it via additional
    /// MCP calls (polling-based — the spec doesn't define
    /// bidirectional state transfer).
    #[arg(
        long = "mode2-approval",
        default_value = "reject",
        value_parser = ["reject", "allow_all", "forward"],
        requires = "mode2"
    )]
    pub mode2_approval: String,

    /// Per-approval wallclock ceiling (in seconds) for
    /// `--mode2-approval forward`. After this duration with no
    /// matching `helper_approve` decision the queued request flips
    /// to "deny" so the helper's tool loop unblocks. Defaults to
    /// 300s. Ignored under any other approval policy.
    #[arg(
        long = "mode2-approval-timeout-secs",
        default_value_t = 300u64,
        requires = "mode2"
    )]
    pub mode2_approval_timeout_secs: u64,

    /// Hard upper bound on iterations a single `helper_ask` call
    /// can spend in the helper's tool-call loop. The caller's
    /// `max_iterations` arg is `min()`-ed against this. Defaults
    /// to the helper's own `agents.defaults.max_tool_iterations`.
    #[arg(long = "mode2-max-iterations", requires = "mode2")]
    pub mode2_max_iterations: Option<usize>,

    /// Drop any `system_prompt` arg passed to `helper_ask`. The
    /// helper still answers the call but runs with no operator
    /// persona override; `_meta.system_prompt = "ignored"` tells
    /// the hub why its prompt didn't take. Use this when the
    /// helper's persona must be fixed by the operator running
    /// `zunel mcp agent` (compliance, fleet roll-out).
    #[arg(long = "mode2-disable-system-prompt", requires = "mode2")]
    pub mode2_disable_system_prompt: bool,

    /// Wallclock ceiling (in seconds) for a single `helper_ask`
    /// call. On expiry the helper's loop is cancelled and the
    /// caller sees a structured error. Defaults to no timeout —
    /// the call runs until the helper finishes, the hub cancels,
    /// or the underlying provider errors out.
    #[arg(long = "mode2-call-timeout-secs", requires = "mode2")]
    pub mode2_call_timeout_secs: Option<u64>,

    /// Print a paste-ready `tools.mcpServers.<name>` snippet for this
    /// instance to stdout and exit without binding the socket. The
    /// snippet uses an `${ENV}` reference for the bearer token rather
    /// than the literal key so secrets never end up on disk via
    /// shell-redirected output.
    #[arg(long = "print-config")]
    pub print_config: bool,

    /// Override the URL embedded in `--print-config` output. Use this
    /// when the bind address isn't a routable hostname (for example,
    /// `0.0.0.0:9000` behind a load balancer at `https://agent.example.com`).
    #[arg(long = "public-url", requires = "print_config")]
    pub public_url: Option<String>,

    /// Override the env-var name embedded in the `--print-config`
    /// `Authorization` header. Defaults to `ZUNEL_<INSTANCE>_TOKEN`.
    #[arg(long = "public-env", requires = "print_config")]
    pub public_env: Option<String>,

    /// Override the `mcpServers` key in `--print-config` output.
    /// Defaults to the active instance name. Use this when two instances
    /// share an instance name and would otherwise collide on the hub.
    #[arg(long = "public-name", requires = "print_config")]
    pub public_name: Option<String>,
}

#[derive(Debug, Parser)]
pub struct McpLoginArgs {
    /// Configured MCP server name.
    pub server: String,
    /// Re-run even if a cached access token already exists.
    #[arg(long)]
    pub force: bool,
    /// Non-interactive full pasted callback URL.
    #[arg(long = "url")]
    pub url_in: Option<String>,
    /// Deterministic OAuth state, intended for tests.
    #[arg(long, hide = true)]
    pub state: Option<String>,
}

#[derive(Debug, Parser)]
pub struct InstanceArgs {
    #[command(subcommand)]
    pub command: InstanceCommand,
}

#[derive(Debug, Subcommand)]
pub enum InstanceCommand {
    /// List discovered instances.
    List,
    /// Set the sticky default instance.
    Use(InstanceUseArgs),
    /// Remove an instance directory.
    Rm(InstanceRmArgs),
    /// Show the active instance and home.
    Show,
}

#[derive(Debug, Parser)]
pub struct InstanceUseArgs {
    pub name: String,
}

#[derive(Debug, Parser)]
pub struct InstanceRmArgs {
    pub name: String,
    /// Skip confirmation.
    #[arg(long, short = 'f')]
    pub force: bool,
}

#[derive(Debug, Parser)]
pub struct SlackArgs {
    #[command(subcommand)]
    pub command: SlackCommand,
}

#[derive(Debug, Subcommand)]
pub enum SlackCommand {
    /// Mint a Slack user token for the read-only Slack MCP.
    Login(SlackLoginArgs),
    /// Print the cached Slack user-token identity.
    Whoami,
    /// Delete the cached Slack user token.
    Logout,
    /// Refresh the rotating Slack **bot** token used by `zunel gateway`.
    ///
    /// Reads the cached `bot_refresh_token` + `client_id` / `client_secret`
    /// from `<zunel_home>/slack-app/app_info.json`, exchanges them at
    /// Slack's `oauth.v2.access`, and writes the new bot token + refresh
    /// token + expiry back to that file as well as `channels.slack.botToken`
    /// in `config.json`. Distinct from `zunel slack login`, which manages
    /// the **user** token under `slack-app-mcp/`.
    RefreshBot(SlackRefreshBotArgs),
    /// Post a Slack message via the user OAuth token (the same token the
    /// built-in Slack MCP uses). Honors `channels.slack.userTokenReadOnly`
    /// and `channels.slack.writeAllow` — if the agent isn't allowed to post
    /// to a target, neither is this CLI. Useful for cron jobs, shell
    /// pipelines, and one-off "remind me" messages without spinning up the
    /// agent loop.
    Post(SlackPostArgs),
}

#[derive(Debug, Parser)]
pub struct SlackLoginArgs {
    /// Comma-separated user scopes to request.
    #[arg(long)]
    pub scopes: Option<String>,
    /// Optional Slack team/enterprise ID to pin the authorize page to.
    #[arg(long)]
    pub team: Option<String>,
    /// Re-run even if the cached user token already exists.
    #[arg(long)]
    pub force: bool,
    /// OAuth redirect URI. Defaults to an HTTPS loopback callback so the flow
    /// completes automatically in your browser (Slack rejects plain http://
    /// redirect URIs). The URI must be registered on the Slack app's
    /// "OAuth & Permissions → Redirect URLs" page; the CLI generates a
    /// self-signed certificate on the fly so your browser will warn once
    /// before forwarding.
    #[arg(
        long = "redirect-uri",
        default_value = "https://127.0.0.1:53682/slack/callback"
    )]
    pub redirect_uri: String,
    /// Skip auto-opening the system browser; print the authorize URL instead.
    #[arg(long = "no-browser")]
    pub no_browser: bool,
    /// Non-interactive full pasted callback URL (skips the local callback server).
    #[arg(long = "url")]
    pub url_in: Option<String>,
    /// Deterministic OAuth state, intended for tests.
    #[arg(long, hide = true)]
    pub state: Option<String>,
}

#[derive(Debug, Parser)]
pub struct SlackPostArgs {
    /// Slack channel ID, DM ID, or user ID. Mutually exclusive with
    /// `--to-self`. Examples: `C0123456789` (public channel),
    /// `D0AUX99UNR0` (DM), `U12F7K329` (DM that user, Slack opens it
    /// for you).
    #[arg(long, conflicts_with = "to_self")]
    pub channel: Option<String>,

    /// Shortcut for "DM yourself". Resolves the authenticated user ID
    /// from the cached user token at
    /// `~/.zunel/slack-app-mcp/user_token.json` and uses it as the
    /// channel. Equivalent to the agent's `slack_dm_self` MCP tool.
    #[arg(long = "to-self", conflicts_with = "channel")]
    pub to_self: bool,

    /// Optional Slack thread `ts` to reply into. Format like
    /// `1713974400.000100`. Without this the message lands as a new
    /// top-level message in the channel.
    #[arg(long = "thread-ts")]
    pub thread_ts: Option<String>,

    /// Message body. Mutually exclusive with `--stdin`. Required unless
    /// `--stdin` is set.
    #[arg(long, conflicts_with = "stdin")]
    pub text: Option<String>,

    /// Read the message body from stdin instead of `--text`. Useful for
    /// piping in command output (`uptime | zunel slack post --to-self
    /// --stdin`).
    #[arg(long, conflicts_with = "text")]
    pub stdin: bool,

    /// Print the JSON envelope returned by the Slack tool (channel, ts,
    /// permalink) instead of the human one-liner.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Parser)]
pub struct SlackRefreshBotArgs {
    /// Skip the refresh when the cached bot token still has more than this
    /// many seconds of life left. Without this flag the refresh always runs.
    /// The launchd wrapper at `~/.zunel/bin/run-gateway.sh` uses
    /// `--if-near-expiry 1800` so it can be invoked on every gateway start
    /// without hammering Slack's `oauth.v2.access` endpoint.
    #[arg(long = "if-near-expiry", value_name = "SECS")]
    pub if_near_expiry: Option<i64>,

    /// Print one JSON object on stdout instead of a human one-liner.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Subcommand)]
pub enum ChannelsCommand {
    /// Show channel connection status.
    Status,
}

#[derive(Debug, Parser)]
pub struct SessionsArgs {
    #[command(subcommand)]
    pub command: SessionsCommand,
}

#[derive(Debug, Subcommand)]
pub enum SessionsCommand {
    /// List persisted sessions sorted by file size desc.
    List,
    /// Pretty-print the most recent rows of a session.
    Show(SessionsShowArgs),
    /// Truncate a session to its metadata header.
    Clear(SessionsClearArgs),
    /// LLM-summarize a session's stale head, keeping the most recent N rows.
    Compact(SessionsCompactArgs),
    /// Delete sessions whose last update is older than the cutoff (e.g. `30d`, `12h`).
    Prune(SessionsPruneArgs),
}

#[derive(Debug, Parser)]
pub struct SessionsShowArgs {
    /// Session key, e.g. `slack:D0AUX99UNR0` or the on-disk file stem.
    pub key: String,
    /// Number of most-recent rows to print.
    #[arg(long, default_value_t = 20)]
    pub tail: usize,
}

#[derive(Debug, Parser)]
pub struct SessionsClearArgs {
    /// Session key, e.g. `slack:D0AUX99UNR0` or the on-disk file stem.
    pub key: String,
    /// Skip interactive confirmation.
    #[arg(long, short = 'y')]
    pub yes: bool,
}

#[derive(Debug, Parser)]
pub struct SessionsCompactArgs {
    /// Session key, e.g. `slack:D0AUX99UNR0` or the on-disk file stem.
    pub key: String,
    /// Most-recent rows to leave intact.
    #[arg(long, default_value_t = 8)]
    pub keep: usize,
    /// Override the model used for summarization (defaults to
    /// `agents.defaults.compaction_model` then `agents.defaults.model`).
    #[arg(long)]
    pub model: Option<String>,
}

#[derive(Debug, Parser)]
pub struct SessionsPruneArgs {
    /// Cutoff like `30d`, `12h`, `45m`. Sessions older than this are deleted.
    #[arg(long = "older-than")]
    pub older_than: String,
    /// Print what would be deleted without removing anything.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Parser)]
pub struct TokensArgs {
    #[command(subcommand)]
    pub command: Option<TokensCommand>,
    /// Emit JSON instead of a human-readable table. Applies to the
    /// no-subcommand totals output as well.
    #[arg(long, global = true)]
    pub json: bool,
}

#[derive(Debug, Subcommand)]
pub enum TokensCommand {
    /// Per-session table sorted by total tokens desc.
    List,
    /// Per-turn breakdown for one session (most recent rows by default).
    Show(TokensShowArgs),
    /// Roll up turns from `metadata.turn_usage` newer than the cutoff.
    Since(TokensSinceArgs),
}

#[derive(Debug, Parser)]
pub struct TokensShowArgs {
    /// Session key, e.g. `slack:D0AUX99UNR0` or the on-disk file stem.
    pub key: String,
    /// Number of most-recent turns to print.
    #[arg(long, default_value_t = 50)]
    pub tail: usize,
    /// Print every recorded turn.
    #[arg(long)]
    pub all: bool,
}

#[derive(Debug, Parser)]
pub struct TokensSinceArgs {
    /// Cutoff like `7d`, `24h`, `45m`.
    pub cutoff: String,
}
