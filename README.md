# Tokenscope

**English** · [中文](README-zh.md)

A **menu-bar / system-tray app for macOS and Windows** that gives you one dashboard for everything your local AI coding agents consumed — **token usage, estimated cost, model / MCP / Skill breakdowns, reliability telemetry, and live provider subscription quota** for Pi, Claude Code, and Codex CLI.

Stack: **Tauri 2 + React + TypeScript** (frontend) / **Rust** (data layer).

![Tokenscope panel (dark / light)](docs/screenshot.png)

## Install

### macOS: Homebrew (recommended)

```bash
brew install --cask sunchj/tokenscope/tokenscope
```

The cask's `postflight` strips the quarantine attribute automatically, so it opens on first launch without the "Apple cannot verify" prompt. After opening it once it registers as a login item and launches in the menu bar on every boot.

Upgrade: `brew update && brew upgrade --cask tokenscope`

### macOS: Download the .dmg (fallback)

1. Download the latest `Tokenscope_*_universal.dmg` from [Releases](https://github.com/SunChJ/tokenscope-remix/releases)
2. Drag it into Applications
3. Because the build is **unsigned / unnotarized**, Gatekeeper blocks first launch — right-click → **Open** → confirm, or run `xattr -cr /Applications/Tokenscope.app && open /Applications/Tokenscope.app`

> Unsigned is a current known limitation; a "double-click to open" experience requires Apple Developer ID signing + notarization.

### Windows

1. Download the latest `Tokenscope_*_x64-setup.exe` from [Releases](https://github.com/SunChJ/tokenscope-remix/releases)
2. Install (per-user, no admin). SmartScreen warns on first run — **More info → Run anyway**
3. Requirements: **Windows 10 1803+ / Windows 11** with the WebView2 runtime (preinstalled on Windows 11)

### Updating

The app checks for new releases on launch and hourly; the banner sits under **Tokenscope** in the top-left corner, or use **Check for Updates…** in the tray menu. Homebrew users keep using `brew upgrade --cask tokenscope`.

## Features

- **Token dashboard**: today / week / month plus an inclusive custom date-range filter. With more than one agent detected, filter chips (All / Claude / Codex / Pi) appear — the All view stacks usage **by agent**, and filtering to one agent re-tints the panel with its accent (Claude coral / Codex teal / Pi violet)
- **Breakdowns**: tokens and cost **by model**, **by MCP call**, **by Skill call** (only servers/skills you installed yourself — built-ins are filtered out)
- **Project settlement**: usage grouped by local Git repo / working directory, exported as CSV without raw paths or conversation content
- **Reliability telemetry**: completed/aborted turns, tool errors/denials, tokens & cost wasted on aborted work
- **Context health**: median/peak context-window pressure, ≥80% warnings, compaction count, reasoning-token share
- **Provider limits**: live subscription quota for **Claude** (5-hour + weekly) and **Codex** (weekly + Spark), shown Codex-status style — `29% left (resets 05:01 on 9 Aug)` — with a burn-rate projection when enough trend points exist
- **Menu Bar Display**: show the tightest window per provider right in the menu bar (`52.8M · Cl79% · Cx29%`), a detailed list, or nothing at all
- Cost donut, year-long activity heatmap

## Provider quota

Subscription limits are collected through the **HappyUsage `hu` CLI**, which owns credential discovery, OAuth refresh, and the provider API calls (Claude via the Anthropic OAuth usage API, Codex via the ChatGPT usage API).

- The `hu` binary **ships inside the app bundle** (`Resources/bin/hu`) — no system install needed; it is rebuilt on every release via `src-tauri/bin/build-hu.sh`
- Runtime lookup: bundled `hu` → system installs → a one-per-24h auto-install (Homebrew tap → install script → `go install`) → native Codex usage call (Pi / Codex CLI OAuth) → cached snapshot
- Quota refreshes every 5 minutes on a background thread; the last successful snapshot is cached (`provider_limits.json`) and survives offline / API failures
- Claude falls back to `~/.claude.json` `cachedUsageUtilization` (updated by Claude Code when it runs); Codex trend merges session-log rate-limit history
- Tokenscope **never caches credentials** — only quota metadata

Menu-bar label: **Off** (`52.8M`) / **Compact** (`52.8M · Cl79% · Cx29%`, tightest window per provider) / **Detailed** (`52.8M · Cl 5h0/W79 · Cx W29/S31`). The right-click tray menu also has a **Provider Limits** submenu with every window's remaining percentage.

## Data sources (local-first; session logs are read-only)

| Purpose | Path |
|---------|------|
| Claude session logs (tokens / model / tool calls) | `~/.claude/projects/**/*.jsonl` |
| Codex session logs (tokens + offline quota fallback) | `~/.codex/sessions/**/*.jsonl` (honors `CODEX_HOME`) |
| Pi session logs (tokens / model / tools / telemetry) | `~/.pi/agent/sessions/**/*.jsonl` (honors `PI_CODING_AGENT_DIR`, `PI_CODING_AGENT_SESSION_DIR`, and absolute `settings.sessionDir`) |
| Provider quota (Claude + Codex) | Bundled HappyUsage `hu` (`Resources/bin/hu`); fallbacks: `~/.claude.json` `cachedUsageUtilization`, native usage API with Pi / Codex CLI OAuth |
| Claude user MCP whitelist | `~/.claude.json` → `mcpServers` + `projects[*].mcpServers` |
| Codex user MCP whitelist | Global and trusted-project `config.toml` → `[mcp_servers.*]` (`$CODEX_HOME/config.toml`, project `.codex/config.toml`) |
| User Skill whitelist | Claude: `~/.claude/skills/`; Codex: `$CODEX_HOME/skills/`, `~/.agents/skills/`, project `.agents/skills/`; Pi: global/shared/project Pi skill locations plus explicit settings paths |
| Model prices | **Primary**: [models.dev](https://models.dev/api.json) (bare model names, matching CLI logs) → **Fallback**: [LiteLLM](https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json) → built-in snapshot. Cached in `~/Library/Caches/tokenscope/`, refreshed every 24h |

## Key processing

- **Claude**: deduplicated by `message.id` (streaming/retries repeat usage); one message spanning multiple lines merges its tool calls and counts tokens once
- **Codex**: a dedicated adapter cross-checks exact per-response `last_token_usage` against the accumulated `total_token_usage` snapshot, so quota-only repeats and fork replays are not counted as new work; stable `(turn, total snapshot)` ids deduplicate persisted replays, and `cached_input_tokens` / `cache_write_input_tokens` are split out of the inclusive `input_tokens` for a four-category accounting comparable with Claude
- **Pi**: each persisted assistant response contributes its exact four-way `usage`; stable tree-entry ids deduplicate history copied by `/fork` or `/clone`; persisted request cost, reasoning, stop reason, tool errors, compactions, and model context windows feed the matching telemetry
- **Token split**: `input` (uncached) / `cache` (creation + read) / `output`; the UI folds cache into "In" by default with a separate "cached %"
- **Pricing**: exact id → normalized id (strip vendor prefix, `.`↔`p`, e.g. `glm-5.1`⇄`glm-5p1`); models.dev's official bare-name price wins. Pi's persisted request cost is preferred when available. Models without any price still count tokens but are labelled "no price"
- **Tool classification**: MCP calls are attributed from direct `mcp__<server>__*` names, Codex tool-search mappings, or Codex App custom calls, then checked against the owning agent's config; Claude Skill calls plus Codex/Pi `SKILL.md` reads are matched against that agent's user/project skill directories; everything else is ignored

> Cost is an **estimate** based on public prices; subscription users should read it as "equivalent spend value".

### Token types & cost formula

Every assistant message's `usage` reports four **mutually exclusive** token counts (they never double-count the same token):

| Stage | `usage` field | What it is | Price (relative to input) |
|-------|---------------|------------|---------------------------|
| **Input** (uncached) | `input_tokens` | New prompt tokens sent this turn | 1× |
| **Cache write** | `cache_creation_input_tokens` | Context written into the prompt cache | ~1.25× |
| **Cache read** (hit) | `cache_read_input_tokens` | Context replayed from the cache | ~0.1× |
| **Output** | `output_tokens` | Tokens the model generated | ~5× |

Pi persists the same categories as `usage.input`, `usage.cacheWrite`, `usage.cacheRead`, `usage.output`; its optional `usage.reasoning` is already a subset of output and never added again.

```
total = input + cache_creation + cache_read + output
# UI shows:  In = input + cache_creation + cache_read,  Out = output,  cached % = cache_read / total

cost = input × price.input
     + cache_creation × price.cache_creation
     + cache_read × price.cache_read        # cache hits billed at the discounted read rate
     + output × price.output
```

A cache hit is **not** billed as normal input — it uses the dedicated cheaper `cache_read` rate, which is why heavily-cached usage shows a huge token count but a modest cost.

## Develop

```bash
pnpm install
pnpm tauri dev         # launch the desktop app (requires the Rust toolchain)
```

Frontend-only preview (using the real-data snapshot `public/dev-dashboard.json`):

```bash
pnpm dev               # http://localhost:1420
# refresh the snapshot:
cd src-tauri && cargo run --example dump > ../public/dev-dashboard.json
```

## Build & release

```bash
pnpm tauri build       # macOS: .app / .dmg; Windows: NSIS .exe → src-tauri/target/release/bundle/
```

The build downloads and bundles the `hu` binary (`src-tauri/bin/build-hu.sh`). Keep version files in sync, then push:

```bash
pnpm version:set 1.5.1
pnpm version:check
git commit -am "release: v1.5.1"
git push origin main
```

A version change pushed to `main` is validated by GitHub Actions, tagged `vX.Y.Z`, built and published for macOS and Windows with signed updater artifacts and `latest.json` (requires `TAURI_SIGNING_PRIVATE_KEY` / `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` secrets).

## Structure

```
src/                  React frontend
  data.ts             types + Tauri bridge + theme + formatting
  charts.tsx          chart primitives (bars / donut / sparkline / heatmap / segmented control)
  App.tsx             main panel
src-tauri/src/
  store.rs            incremental multi-source JSONL ingest (Claude + Codex + Pi adapters)
  parser.rs           aggregation into scopes (All / per-agent; presets + custom ranges + heatmap)
  pricing.rs          models.dev / LiteLLM price loading and costing
  quota_api.rs        provider quota via bundled HappyUsage `hu` + native/auto-install fallbacks
  config.rs           user MCP / Skill whitelists (Claude + Codex + Pi)
  model.rs            data structures returned to the frontend
  lib.rs              Tauri commands + menu-bar tray
src-tauri/bin/        build-hu.sh — downloads the hu binary bundled into releases
```

## Bug log

Notable bugs found during development — symptom, root cause, and fix — are collected in [docs/BUGFIXES.md](docs/BUGFIXES.md).

## Acknowledgements

This project started as a remix of [tokenscope](https://github.com/HduSy/tokenscope) by [@HduSy](https://github.com/HduSy) — a beautifully built macOS menu-bar dashboard for Claude CLI usage. The clean Rust ingest/aggregation architecture and the panel design this project inherits are his work, and this repo remains under the original MIT license. Huge thanks! 🙏

Tokenscope Remix extends it into a multi-agent dashboard (see [docs/DESIGN-multi-agent.md](docs/DESIGN-multi-agent.md)) and is maintained independently: please file issues and feature requests here, not upstream. Provider quota uses [HappyUsage](https://github.com/SunChJ/happyusage) (`hu`) for credential handling and provider API calls.
