# Pi Session Adapter

## Decision

Pi is a first-class usage source (`agent = "pi"`). It is not merged into Codex when Pi happens to use the `openai-codex` provider: the dashboard's agent dimension represents the harness, while provider/model identity controls request metadata and pricing.

## Source discovery

The adapter always scans the default root under `PI_CODING_AGENT_DIR` (normally `~/.pi/agent/sessions`) and additionally scans an absolute session override from:

1. `PI_CODING_AGENT_SESSION_DIR`
2. `settings.json` → `sessionDir`

Pi resolves relative session directories against each CLI process's cwd. A background desktop app has no single equivalent cwd, so only absolute overrides can be discovered globally.

## Normalization

| Pi session data | Normalized data |
|---|---|
| Session header `id` / `cwd` | Session and project identity |
| Assistant `provider` / `model` | Active provider/model metadata |
| `usage.input` | Uncached input |
| `usage.cacheWrite` | Cache creation |
| `usage.cacheRead` | Cache read |
| `usage.output` | Output (already includes `reasoning`) |
| `usage.reasoning` | Reasoning telemetry only; never added to token total again |
| `usage.cost.total` | Preferred persisted request cost |
| Assistant `stopReason` | Completed/aborted turn outcome |
| Tool result `isError` | Tool-error telemetry |
| `compaction` entry | Compaction count; optional summary usage is also billed |

Model context windows are resolved from Pi's `models-store.json` and `models.json`. Turn duration and first-response latency are derived from persisted user/assistant timestamps.

## Tree and copy semantics

All physical assistant entries in one tree file represent real upstream requests, including abandoned branches, and therefore count as usage.

`/fork` and `/clone` can copy an active path into a new file. Pi preserves each entry's stable id, so the store persists a global `entry id → original source` manifest and accepts only the first occurrence. Files are ordered by their timestamp-prefixed filename so the original normally wins. This deduplication runs before token, tool, and reliability side effects.

## Tools and Skills

- Direct `mcp__<server>__<tool>` calls are treated as user-extension MCP calls; Pi has no built-in MCP registry.
- A `read` tool call targeting `.../<skill>/SKILL.md` records one invocation per skill per user turn.
- `/skill:<name>` user commands are recognized when persisted directly.
- Skill whitelists include Pi's global/shared/project locations and explicit non-glob settings paths.

## Non-goals

- Pi sessions do not persist Codex rate-limit snapshots, so the Pi scope does not synthesize a quota card.
- Relative custom `sessionDir` values cannot be globally discovered outside their originating project cwd.
- Generic extension tools are not classified as MCP unless their persisted name uses the `mcp__` convention.
