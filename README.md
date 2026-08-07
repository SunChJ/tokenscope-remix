# Tokenscope

**English** · [中文](README-zh.md)

A menu-bar / system-tray app for macOS and Windows that gives you **one dashboard for all your local AI coding agents** — token usage, estimated cost, per-model / MCP / Skill breakdowns, and live subscription quota.

Built with **Tauri 2 + React + TypeScript**, backed by a Rust data layer. Works with **Pi, Claude Code, and Codex CLI**.

![Tokenscope panel (dark / light)](docs/screenshot.png)

## Highlights

- **One dashboard, every agent** — filter chips (All / Claude / Codex / Pi) switch between aggregated and per-agent views, each with its own accent theme
- **Token & cost analytics** — day / week / month plus custom date ranges; breakdowns by model, MCP calls, and Skills (yours only — built-ins are filtered out)
- **Live subscription quota** — Claude (5-hour + weekly) and Codex (weekly + Spark) refreshed every 5 minutes, shown Codex-status style (`29% left (resets 05:01 on 9 Aug)`), with a burn-rate projection
- **Menu-bar glanceability** — today's tokens plus a compact per-provider quota summary (`52.8M · Cl79% · Cx29%`) or a detailed list
- **Project settlement** — usage grouped by Git repo / working directory, exported as CSV without raw paths or conversation content
- **Reliability & context telemetry** — aborted-turn waste, tool errors, context-window pressure, compaction and reasoning share
- **Local-first and read-only** — parses session logs in place; quota comes from the bundled HappyUsage CLI (no credentials stored, no local log-based quota)

## Install

```bash
# macOS (recommended)
brew install --cask sunchj/tokenscope/tokenscope

# Windows: grab the setup .exe from Releases
```

Downloads (`.dmg` / `.exe`) and update details: see [Releases](https://github.com/SunChJ/tokenscope-remix/releases).

## Quick start

1. Launch — an icon with today's token count appears in the menu bar / tray
2. **Left-click** toggles the dashboard; **right-click** for the menu (Provider Limits, Menu Bar Display, Dashboard Shortcut, language, …)
3. **Menu Bar Display** controls the quota summary next to the token count: Off / Compact / Detailed

## Develop

```bash
pnpm install
pnpm tauri dev         # desktop app (requires the Rust toolchain)
pnpm tauri build       # .app / .dmg (macOS), .exe (Windows)
```

## Docs

- Data sources & accounting model: [docs/DESIGN-usage-api.md](docs/DESIGN-usage-api.md), [docs/DESIGN-pi-adapter.md](docs/DESIGN-pi-adapter.md), [docs/DESIGN-multi-agent.md](docs/DESIGN-multi-agent.md)
- Known bugs and fixes: [docs/BUGFIXES.md](docs/BUGFIXES.md)

## Acknowledgements

A remix of [tokenscope](https://github.com/HduSy/tokenscope) by [@HduSy](https://github.com/HduSy) — the clean Rust ingest/aggregation architecture and panel design are his work; this repo stays under the MIT license. Provider quota uses [HappyUsage](https://github.com/SunChJ/happyusage) (`hu`). Issues and feature requests go to this repository.
