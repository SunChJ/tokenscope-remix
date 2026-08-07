# Provider Quota via HappyUsage

## Decision

Live provider subscription quota is collected **only** through the HappyUsage
`hu` CLI. `hu` owns credential discovery, OAuth refresh, and the provider API
calls (Claude via the Anthropic OAuth usage API, Codex via the ChatGPT
`wham/usage` endpoint).

There is deliberately **no fallback to locally computed quota** (session-log
`rate_limits`, `~/.claude.json` caches, or previously persisted snapshots):
when `hu` cannot serve a provider, that provider shows an explicit
**unavailable** state. Local logs count tokens; they do not describe
subscription limits.

## Architecture

```
refresh thread (every 5 min)
  └─ quota_api::reload()
       ├─ bundled hu → `hu usage claude --json` → claude ProviderLimit
       ├─ bundled hu → `hu usage codex --json` → codex ProviderLimit
       └─ failed fetch ⇒ that provider is None (unavailable)
parser::build_dashboard()
  └─ quota_api::shared() → provider_limits (dashboard + tray)
       └─ trend: live points only, carried across refreshes in memory
```

The in-memory cache holds only the current process's last successful readings
so trend points can accumulate; it is never persisted and never treated as a
data source when a refresh fails.

## Bundling

`src-tauri/bin/build-hu.sh` downloads the `hu` release asset matching the
Tauri target OS/architecture before `tauri build`; `bundle.resources` ships it
as `Resources/bin/hu`. macOS releases are separate Apple Silicon and Intel
packages rather than a universal app, so each package contains exactly one
matching `hu`. At runtime Tokenscope invokes only that bundled binary; it never
uses or installs a system copy whose version and output schema it does not
control.

## Normalization

| Provider | Source | Windows |
|---|---|---|
| Claude | `hu` envelope `quotas[]` | `session` → 5-hour, `weekly` → 7d |
| Codex | `hu` envelope `quotas[]` | primary → Weekly, `Spark` → Spark |

- Matching is case-insensitive and tolerates renamed quota names across
  happyusage versions (`Spark` / `Spark_weekly`, `session` / `weekly`).
- Codex's `period` label is **not** trusted (`limit_window_seconds` can be
  absent, which makes `hu` fall back to a misleading `5h`); the 5h Codex
  window is retired anyway, so both Codex windows are classified as weekly.
- `used_pct` is retained in storage; presentation converts to rounded
  percentage left (`29% left (resets 05:01 on 9 Aug)`).

## Menu bar and dashboard

- **Provider Limits** submenu: one read-only row per provider
  (`Claude — 5h 0% · W 79%`), refreshed with every dashboard build.
- **Menu Bar Display**: Off / Compact / Detailed.
  - Compact: `52.8M · Cl79% · Cx29%` — tightest window per provider.
  - Detailed: `52.8M · Cl 5h0/W79 · Cx W29/S31`.
- Dashboard: a global two-column card (Claude | Codex), each window showing
  remaining capacity, the reset date, and an optional burn-rate projection.
  A provider with no `hu` snapshot shows **Unavailable**; if both are
  unavailable the whole section says so. Provider limits never attach to a
  token-usage scope (Pi / Codex CLI remain only usage sources).
