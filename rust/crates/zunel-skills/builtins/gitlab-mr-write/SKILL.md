---
description: Use the `glab` CLI (via the `exec` tool) to perform write actions on a GitLab merge request — approve, post a general note, post a threaded reply, merge, close, or reopen. Use when the user explicitly asks zunel to do one of those write actions on a GitLab MR; most configured `gitlab_*` MCP tools are read-only and don't cover these.
metadata:
  zunel:
    always: false
    requires:
      bins:
        - glab
---

# GitLab MR Write Operations (via `glab`)

Most GitLab MCP servers in the wild expose only read-side tools —
`gitlab_get_*`, `gitlab_search_*`, `gitlab_compare`. Anything that
mutates an MR (approve, comment, merge, close, reopen, label, assign)
has to go through the `glab` CLI via the `exec` tool. This skill tells
you how to do that safely.

If your environment happens to have an MCP server that exposes a real
`gitlab_approve_merge_request` / `gitlab_post_*` / `gitlab_create_*`
tool, prefer that — this skill is the fallback.

## When to use this skill

Invoke this skill when the user **explicitly** asks for one of these
on a GitLab MR (URL, IID, or "this MR" in context):

- "Approve [it / !123 / this MR]"
- "Comment on it with `<text>`" / "Post a note saying `<text>`"
- "Reply to `<reviewer>`'s comment with `<text>`"
- "Merge it" / "Squash and merge"
- "Close / reopen this MR"

Do **not** invoke this skill for read operations — `glab mr view` works
but a `gitlab_get_merge_request` MCP tool (when available) is preferred
for reads (structured output, no shell round-trip).

## Prerequisites

- `glab` must be on `PATH`. If not, this skill self-disables via
  `requires.bins`, so you won't see it.
- The user's `glab` must be authenticated for the relevant host. Check
  with `glab auth status` once if you're unsure. If the host shows
  `401 Unauthorized` or "Token revoked", **stop and surface the error
  to the user** — don't try to log in from inside this skill.

## Parsing a GitLab MR URL

A GitLab MR URL has the shape

    https://<host>/<group>[/<subgroup>...]/<project>/-/merge_requests/<iid>

Extract three pieces from whatever URL the user gives you:

- **Host**: e.g. `gitlab.com`, or your company's self-hosted instance.
- **Project path**: everything between the host and `/-/merge_requests/`,
  e.g. `mygroup/mysubgroup/myproject`.
- **MR IID**: the integer at the end (the number that shows up as
  `!<iid>` in the GitLab UI).

For `glab mr <subcmd>` calls, pass the IID positionally and the project
via `-R` using the **full URL form** (host + project path). The full URL
form is unambiguous when the user has multiple GitLab hosts configured:

    -R https://<host>/<group>/<project>

For `glab api` calls (used for threaded replies — see below), the
project path needs to be **URL-encoded**: `/` → `%2F`. So
`mygroup/mysubgroup/myproject` becomes
`mygroup%2Fmysubgroup%2Fmyproject`.

## Operations

All commands below run through the `exec` tool. The user's `exec`
config must allow these (it does by default; `glab` is not in the
deny-list).

The placeholders to substitute in every example:

- `<HOST>` — e.g. `gitlab.com`
- `<PROJECT>` — slash-separated project path, e.g. `mygroup/myproject`
- `<PROJECT_ENC>` — URL-encoded project path, e.g. `mygroup%2Fmyproject`
- `<IID>` — the merge-request integer ID

### Approve

```
glab mr approve <IID> -R https://<HOST>/<PROJECT>
```

Success prints something like `Approved !<IID> in <project>`. GitLab
typically resets approvals on a new push, so the user may have to ask
you to re-approve after they push more commits.

### Post a general note (top-level MR comment)

```
glab mr note <IID> \
  -R https://<HOST>/<PROJECT> \
  -m "LGTM, ready to merge once CI passes."
```

Use this for top-level commentary. **Do not use this for replies to a
specific reviewer's inline comment** — those need the discussions API
below.

### Reply inside an existing discussion thread

This is a two-step flow. First, list discussions to find the
discussion ID you want to reply to:

```
glab api 'projects/<PROJECT_ENC>/merge_requests/<IID>/discussions'
```

The response is a JSON array; each discussion has an `id` (the
discussion ID, a long hex string) and a `notes` array. Find the
discussion whose first note matches the reviewer / text you want to
reply under.

Then post the reply:

```
glab api --method POST \
  'projects/<PROJECT_ENC>/merge_requests/<IID>/discussions/<DISCUSSION_ID>/notes' \
  -f 'body=Your reply text here'
```

The reply will appear nested under the original comment in the GitLab
UI. Plain `glab mr note` would have created a separate top-level note.

### Merge

```
glab mr merge <IID> \
  -R https://<HOST>/<PROJECT> \
  -y --remove-source-branch
```

`-y` skips the interactive confirmation prompt — required because
`exec` is non-interactive. Add `-s` for squash, `-r` for rebase,
`--auto-merge` to enable merge-when-pipeline-succeeds.

### Close / reopen

```
glab mr close  <IID> -R https://<HOST>/<PROJECT>
glab mr reopen <IID> -R https://<HOST>/<PROJECT>
```

## Safety rules

1. **Never approve, merge, close, or reopen on your own initiative.**
   Only do these when the user explicitly says "approve", "merge",
   "close", "reopen". Implicit instructions like "review this MR" or
   "what do you think?" do **not** authorize a write action.
2. **For merge specifically**, even with explicit instruction, restate
   what you're about to merge in one line ("Merging !`<IID>` in
   `<project>` with `--remove-source-branch -y`") and proceed. If the
   MR has any unresolved discussions, failing pipelines, or the user
   hasn't referenced it earlier in the conversation, ask once before
   merging.
3. **For comments**, draft the comment text first, get user approval,
   then post. When the user gives you literal text in quotes, post that
   text verbatim — don't rewrite it.
4. **Stop on auth failure.** If `glab` returns `401`, `403`, "Token
   revoked", or "could not authenticate", surface the raw error and
   stop. Don't retry, don't fall back to a different host, don't try
   to refresh.
5. **One write per `exec` call.** Don't chain `&&` write commands; run
   one, read the output, then run the next. Makes failures attributable.

## Worked example

> **User**: Approve `https://<HOST>/<group>/<project>/-/merge_requests/123`
> — pipeline's green.

> **Assistant**: Approving !123 in `<group>/<project>` now.
>
> *[calls `exec` with
> `command: "glab mr approve 123 -R https://<HOST>/<group>/<project>"`]*
>
> Approved.

## Don'ts

- **Don't** call `approve` / `merge` / `close` / `reopen` without
  explicit user instruction.
- **Don't** use `glab mr note` for a threaded reply — that posts a
  top-level note. Use the discussions API.
- **Don't** invent discussion IDs; always list them first via
  `glab api .../discussions`.
- **Don't** run `glab auth login` or open a browser — auth is the
  user's responsibility, handled outside this skill.
- **Don't** retry on `401` / `403` — surface the raw error and stop.
- **Don't** use this skill for read operations — prefer the
  `gitlab_get_*` MCP tools when available.
