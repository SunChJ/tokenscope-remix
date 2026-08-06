# Provider Quota via HappyUsage

## Decision

Live provider subscription quota is collected through the **HappyUsage `hu`
CLI** instead of a self-hosted credential + HTTP stack. `hu` already owns:

- Credential discovery (Claude Code keychain / `.credentials.json`,
  Codex CLI `auth.json`, …) and OAuth refresh,
- Provider API calls (Claude via the Anthropic OAuth usage API, Codex via the
  ChatGPT `wham/usage` endpoint),
- Per-provider error handling.

Tokenscope runs `hu usage <provider> --json` and parses the envelope. This
keeps one credential implementation instead of duplicating it in Rust.

## Architecture

```
refresh thread (every 5 min)
  └─ quota_api::reload()
       ├─ hu usage claude --json   → claude ProviderLimit
       │    └─ on failure: ~/.claude.json cachedUsageUtilization (local fallback)
       └─ hu usage codex --json    → codex ProviderLimit
       └─ persist ProviderCache → provider_limits.json (no credentials)
parser::build_dashboard()
  └─ quota_api::shared() → provider_limits (dashboard + tray)
       └─ Codex windows merge session-log quota_history for trend depth
```

`provider_limits.json` caches the last successful snapshot per provider so a
later offline refresh (or a missing `hu`) still has data. The cache holds
quota metadata only — never tokens.

## Normalization

| Provider | Source | Windows |
|---|---|---|
| Claude | `hu` envelope `quotas[]` | `session` → 5-hour, `weekly` → 7d |
| Claude (fallback) | `~/.claude.json` `cachedUsageUtilization` | `five_hour`, `seven_day` |
| Codex | `hu` envelope `quotas[]` | primary → Weekly, `Spark` → Spark |

- HappyUsage hardcodes Claude's period labels, so they are trusted.
- Codex's `period` label is **not** trusted: `limit_window_seconds` can be
  absent from the window payload (observed on live API responses), which makes
  `hu` fall back to a misleading `5h`. The 5h Codex window is retired anyway,
  so both Codex windows are classified as weekly here.
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
  Provider limits never attach to a token-usage scope (Pi / Codex CLI remain
  only usage sources).

## Fallback chain

1. `hu usage <provider> --json`
2. Last successful `provider_limits.json` snapshot
3. Claude-only: `~/.claude.json` `cachedUsageUtilization` (updated by Claude
   Code when it runs)
4. Codex trend: session-log `rate_limits` history merged into the live window
