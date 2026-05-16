# Memory in Zunel

Zunel's memory is built around one idea: memory should stay useful without
becoming noisy.

It does not keep everything in one giant file. Instead, it separates live
conversation state, summarized history, and durable knowledge so the agent can
stay fast while still learning over time across both `zunel agent` and
`zunel gateway` when they share a workspace.

## The Design

Zunel stores memory in layers:

- `session.messages` for the current live conversation
- `memory/history.jsonl` for compressed historical summaries
- `SOUL.md`, `USER.md`, and `memory/MEMORY.md` for durable knowledge
- a local git-backed history under `memory/.git/` to track changes to those durable files

## The Flow

Memory moves in two stages.

### Stage 1: Consolidator

When a session gets large enough to pressure the context window, Zunel
summarizes the oldest safe slice of the conversation and appends it to
`memory/history.jsonl`.

Each entry is a JSON object:

```json
{"cursor": 42, "timestamp": "2026-04-03 00:02", "content": "- User prefers dark mode\n- Decided to use PostgreSQL"}
```

This file is append-only and cursor-based. It is the raw material from which
durable memory is shaped.

### Stage 2: Dream

`Dream` is the slower long-term memory pass. It runs on the configured schedule
when the gateway is active, and you can also trigger it manually from chat.

Dream reads:

- new entries from `memory/history.jsonl`
- `SOUL.md`
- `USER.md`
- `memory/MEMORY.md`

Then it:

1. compares what is new against what is already known
2. edits the long-term files with the smallest honest change that keeps memory coherent

## Default File Layout

```text
~/.zunel/workspace/
├── SOUL.md
├── USER.md
└── memory/
    ├── MEMORY.md
    ├── history.jsonl
    ├── .cursor
    ├── .dream_cursor
    └── .git/
```

These files play different roles:

- `SOUL.md` stores how Zunel should sound and behave
- `USER.md` stores stable information about the user
- `MEMORY.md` stores durable project facts and decisions
- `history.jsonl` stores summarized history on the way there

## Why `history.jsonl`

`history.jsonl` is optimized for incremental machine processing:

- stable cursors
- easy batching
- safer parsing
- clean separation between raw summaries and curated memory

You can still inspect it with normal tools:

```bash
rg -i "keyword" ~/.zunel/workspace/memory/history.jsonl

jq -r 'select(.content | ascii_downcase | contains("keyword")) | .content' \
  ~/.zunel/workspace/memory/history.jsonl
```

## Commands

You can inspect and control memory from chat:

| Command | What it does |
|---------|--------------|
| `/dream` | Run Dream immediately |
| `/dream-log` | Show the latest Dream memory change |
| `/dream-log <sha>` | Show a specific Dream change |
| `/dream-restore` | List recent Dream memory versions |
| `/dream-restore <sha>` | Restore memory to the state before a specific change |

## Versioned Memory

After Dream changes the long-term memory files, Zunel records those edits in a
local git-backed store. That gives you a history you can inspect, compare, and
restore.

## Configuration

Dream lives under `agents.defaults.dream`:

```json
{
  "agents": {
    "defaults": {
      "dream": {
        "intervalH": 2,
        "modelOverride": null,
        "maxBatchSize": 20,
        "maxIterations": 15
      }
    }
  }
}
```

| Field | Meaning |
|-------|---------|
| `intervalH` | How often Dream runs, in hours |
| `modelOverride` | Optional Dream-specific model override |
| `maxBatchSize` | How many history entries Dream processes per run |
| `maxIterations` | Tool budget for Dream's editing phase |

In practice:

- `modelOverride: null` means Dream uses the same model as the main agent
- larger `maxBatchSize` values catch up faster
- lower `maxBatchSize` values keep each Dream pass lighter
- `maxIterations` is a safety budget for the edit loop

## In Practice

The result is simple:

- conversations stay fast
- durable facts get cleaner over time
- memory changes stay inspectable and reversible
