---
description: Walks the user through OAuth login for a remote MCP server (Atlassian Jira, Glean, etc.) over chat. Use when the user asks to log in / reauth / sign in to a server, or when an MCP tool call returns an error that starts with `MCP_AUTH_REQUIRED:`.
metadata:
  zunel:
    always: false
---

# MCP OAuth Login (chat flow)

This skill drives the chat-side handshake for getting a fresh OAuth access token
for a remote MCP server. It is **additive** to `zunel mcp login` on the CLI —
the CLI path is still the primary one. Use this when:

- The user explicitly says "log me into `<server>`", "reauth `<server>`",
  "sign in to `<server>`", "connect `<server>`", or similar.
- An MCP tool you tried to call returned an error string that starts with
  `MCP_AUTH_REQUIRED:`. That means the server is configured but has no
  cached token (or the cached token can't be refreshed). Don't pretend you
  invoked the tool successfully — surface the auth requirement and offer
  to start a login.

## Auth-required error contract

When zunel can't talk to a remote MCP server because of auth, you'll see a
tool error of the shape:

```
MCP_AUTH_REQUIRED:server=<server_name>; reason=<short_reason>
```

Common reasons: `not_cached`, `no_refresh_token`, `no_token_url`, `invalid_token`.
Treat any string with the `MCP_AUTH_REQUIRED:` prefix as a structured signal:
parse out the server name, drop everything else, and start the login flow
below.

## Conversation pattern

1. **Tell the user briefly what you're doing.** One sentence — e.g.
   "I'll start an OAuth login for `atlassian-jira` now."
2. **Call `mcp_login_start` with `{ "server": "<server_name>" }`.** The tool
   returns JSON like:
   ```json
   {
     "ok": true,
     "server": "atlassian-jira",
     "authorize_url": "https://auth.atlassian.com/authorize?...",
     "redirect_uri": "http://127.0.0.1:53682/callback",
     "expires_in": 600,
     "instructions": "Open the URL above..."
   }
   ```
3. **Post the `authorize_url` to the user, plus the paste-back instructions
   verbatim.** The link MUST be clickable in chat — render it as the bare URL
   on its own line, not hidden behind markdown link text. Then quote the
   `instructions` field as the next line(s). Do not add your own paraphrase
   of the instructions; the server-side text is intentionally precise about
   what the user has to copy back.
4. **Wait for the user's reply.** It will look like one of:
   - The full redirect URL — `http://127.0.0.1:53682/callback?code=abc&state=xyz`
     (the page they land on tells them to copy this).
   - Just the query string — `?code=abc&state=xyz` or `code=abc&state=xyz`.
   - Some IdPs may print the code on the page; the user pastes that back as
     `code=...&state=...`.
   Accept any of those — `mcp_login_complete` parses all three.
5. **Call `mcp_login_complete` with `{ "server": "<server_name>", "callback_url": "<what they pasted>" }`.**
   On success the tool returns:
   ```json
   { "ok": true, "server": "atlassian-jira", "scopes": "...", "expires_in": 28800, "token_path": "/Users/.../mcp-oauth/atlassian-jira/token.json" }
   ```
   Confirm to the user: "Logged in to `atlassian-jira` — token cached, scopes
   `<scopes>`, expires in `<minutes>` minutes." Then, if there was a tool
   call that triggered the auth flow, **retry it once now** without asking
   for permission; that's almost always what the user wanted.
6. **On failure** the tool returns `{ "ok": false, "error": "..." }`. Surface
   the error verbatim and offer two recovery paths: (a) try `mcp_login_start`
   again (most state-mismatch / expired-pending errors are fixed by a fresh
   `start`), or (b) run `zunel mcp login <server>` from the CLI if they have
   shell access.

## Worked example

> **User**: I keep getting `MCP_AUTH_REQUIRED:server=atlassian-jira; reason=not_cached`
> when I ask you to look at JIRA-1234. Fix it.

> **Assistant**: I'll start the OAuth login for `atlassian-jira` now.
>
> *[calls `mcp_login_start` with `{"server": "atlassian-jira"}`]*
>
> Open this URL to approve the connection:
>
> https://auth.atlassian.com/authorize?response_type=code&client_id=...&state=...&code_challenge=...
>
> Open the URL above in your browser. After you approve in your browser,
> the page will show the redirect URL (or your browser will land on a
> `127.0.0.1` page). Copy that full URL and paste it back to me as your
> next message — I'll finish the login by calling `mcp_login_complete`.
> The pending login expires in 10 minutes.

> **User**: http://127.0.0.1:53682/callback?code=abc123&state=xyz789

> **Assistant**: *[calls `mcp_login_complete` with
> `{"server": "atlassian-jira", "callback_url": "http://127.0.0.1:53682/callback?code=abc123&state=xyz789"}`]*
>
> Logged in to `atlassian-jira` — token cached, scopes `read:jira-work
> read:jira-user offline_access`, expires in 480 minutes. Retrying
> JIRA-1234 now.
>
> *[retries the original tool call]*

## Don'ts

- **Don't open a browser yourself or run `zunel mcp login` as a shell
  command.** This skill is for the chat path; the user does the browser
  step. If you're tempted to spawn anything with the `exec` tool, you're
  on the wrong path.
- **Don't ask the user for their `code`, `state`, or `verifier` separately.**
  The pasted redirect URL contains everything `mcp_login_complete` needs.
- **Don't strip or rewrite the `authorize_url`.** It includes a state +
  PKCE challenge that must round-trip exactly.
- **Don't skip the retry of the original failing tool call** after a
  successful login — the user's intent is the original task, not the
  authentication ceremony.
- **Don't repeat the login flow on cosmetic 401s mid-conversation** — the
  background refresh task in `zunel gateway` rotates tokens automatically.
  If you get a single transient 401, retry the tool call once before
  starting `mcp_login_start`.
