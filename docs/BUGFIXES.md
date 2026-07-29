# Bug Log

A record of bugs hit during development, each with its symptom, root cause, and
fix. Newest first. Useful as a reference for similar issues.

---

## Data accuracy (call counting)

### 1. Skills invoked via slash command were not counted

- **Symptom**: Calling a skill with `/find-skills` never showed up in the panel;
  the count stayed on an unrelated skill (`summary-recorder`).
- **Cause**: A slash-command invocation is logged as a **`user` message** with a
  `<command-name>/find-skills</command-name>` tag — **not** as a `Skill`
  `tool_use` block. The parser only looked at `tool_use`, so slash-command
  skills were invisible. (`summary-recorder` was a real, older `Skill` call that
  happened to fall inside the visible time window.)
- **Fix**: Added `parse_user_command` in `store.rs` to extract the skill name
  from the `<command-name>` tag. These command events carry no model/tokens, so
  `Agg.add` in `parser.rs` guards request/model accounting with
  `!model.is_empty()` to avoid inflating request counts or creating an
  empty-named model. Bumped `STORE_VERSION` to force a one-time full rescan.

### 2. Tool calls dropped when one message spanned multiple log lines

- **Symptom**: A model-invoked skill (`skill-creator`) produced a real `Skill`
  `tool_use`, yet still wasn't counted.
- **Cause**: A single assistant message (same `message.id`) is split across
  several JSONL lines — e.g. `thinking` on one line, `tool_use` on the next,
  **both repeating the same `usage`**. The store deduped **by `message.id` and
  dropped whole duplicate lines**, so the line carrying the `tool_use` was
  discarded after the `thinking` line was ingested first. This also silently
  dropped MCP calls in the same shape.
- **Fix**: Replaced the drop-on-duplicate logic in `store.rs` with a
  merge: keep an `id → index` map, and when a line repeats a known id, **merge
  its `mcp`/`skills` into the existing event without re-counting tokens** (usage
  is identical across the split lines, so it's still counted once). Bumped
  `STORE_VERSION` for a full rescan.

### 3. Delta percentage was ~100× too small (and hidden)

- **Symptom**: The Day view showed no change percentage next to Total tokens.
- **Cause**: `pct_delta` computed `((cur-prev)/prev*100).round()/100`, which
  cancels the `×100` and returns a **fraction** (e.g. `0.2`) instead of a
  **percentage** (`20`). The UI rounds the value to an integer for display, so
  `0.2 → 0%`, and the "hide when 0" rule then hid it entirely.
- **Fix**: Changed `pct_delta` to `((cur-prev)/prev*10000).round()/100`, which
  returns a real percentage with 2 decimals (e.g. `20.47`).

---

## Data accuracy (cost & token totals)

### 4. Cache tokens priced at $0, undercounting Claude cost ~4×

- **Symptom**: Claude's monthly cost read several-fold too low (~4× on the logs
  it was found with), while Codex looked correct. The error came and went
  depending on which model was in use.
- **Cause**: models.dev lists the same model under many providers (first-party
  vendor plus resellers / gateways / clouds). `ingest_modelsdev` is
  first-writer-wins over providers iterated in key order — and `serde_json`
  has no `preserve_order`, so that order is **alphabetical**. The previous sort
  only pushed bare ids ahead of `vendor/model` ones, so among bare entries the
  alphabetically first provider won. For `claude-fable-5` that is `abacus`,
  whose entry is not a cheaper price but an **incomplete record**: identical
  `input`/`output` to Anthropic's, with `cache_read`/`cache_write` simply
  absent, so both defaulted to `0`. Claude Code usage is ~95%+ cache-read
  tokens, so this charged for only a small fraction of the volume.
  `claude-sonnet-5` was worse — the shadowing entry also carried a 50% higher
  input price.
- **Fix**: Added `is_first_party()` and ordered entries best-first in
  `pricing.rs`: the model's first-party vendor, then entries that actually
  carry cache pricing, then bare ids over `vendor/model` duplicates. Ported
  from upstream `3a30273` / `0987a4c`.

### 5. Forked Codex threads double-counted their parent's history

- **Symptom**: Codex token totals ran several-fold high — most of the ingested
  Codex volume was duplicate — the model breakdown didn't sum to Total tokens,
  some events were attributed to the wrong session and project, and the weekly
  quota bar sat pinned at 100% long after the window had rolled over.
- **Cause**: Codex forks a subagent/resumed thread by replaying the parent
  thread's **entire history** — `token_count`, `task_started`/`task_complete`,
  tool calls, even the parent's `session_meta` — into the head of the child's
  rollout file, all restamped with the fork instant. Claude events dedupe by
  `message.id`, but **Codex events carry no id**, so nothing caught it and
  every replayed turn was counted a second time. The replayed `session_meta`
  also clobbered the child's own session id and cwd. Because `state.model` is
  only set by `turn_context`, the replayed events also had an empty model, so
  they landed in Total tokens but were excluded from requests, the model
  breakdown, and cost.
- **First attempt (v9, incomplete)**: `FileState` gained `replaying`/`meta_seen`
  and a forked file skipped every line until its own first `turn_context`. That
  assumed the replay always *precedes* the fork's first turn. It does in one
  layout; in the more common one the fork's `turn_context` sits at line ~7 and
  the parent's history is replayed **after** it, so the window closed
  immediately and the replay was ingested anyway. The window check caught a
  small minority of replayed events and missed an order of magnitude more.
- **Why the first verification missed it**: the parser was reconciled against a
  "ground truth" that summed every `token_count` in the raw logs. Both sides
  shared the same duplicate-counting flaw, so agreeing to 0.01% proved only that
  they were consistent, not that either was right. A reconciliation has to be
  derived independently of the code under test.
- **Fix (v10)**: dedup by content instead of by position. Codex `token_count`
  lines carry no id, but a replay reproduces the parent's turns verbatim — same
  turn ids, same order — so `(turn_id, position-within-turn)` identifies a
  replayed event as one already ingested, exactly the way `message.id` does for
  Claude. The check runs *before* any side effect, so a replay can no longer
  double-count tokens, duplicate turn telemetry, or let the parent's stale
  `rate_limits` win the newest-snapshot race. Files are now walked in path order
  (chronological for Codex's `YYYY/MM/DD/rollout-<ISO>` layout) so the original,
  not the restamped replay, is the first writer. `STORE_VERSION` 10.
- **Known limitation**: dedup needs a turn id. Events parsed before any
  `turn_context` is seen get no id and are still counted as-is; that is the
  conservative direction (keep, don't drop), and such events are rare.

---

## Release & distribution (CI)

### 6. Release CI failed: empty Apple signing env var

- **Symptom**: The `v0.1.1` build failed at the bundle step with
  `security: SecKeychainItemImport: ... parameters ... not valid` /
  `failed to import keychain certificate`.
- **Cause**: The workflow passed `APPLE_CERTIFICATE: ${{ secrets.APPLE_CERTIFICATE }}`,
  but the secret didn't exist, so it became an **empty string**. Tauri's bundler
  treats the env var's *presence* as "a certificate was provided" and tries to
  `security import` empty data, which fails. (Local builds were fine because the
  var was *unset*, not empty.)
- **Fix**: Commented out the Apple signing/notarization env in `release.yml`
  until the real secrets exist. The build now does ad-hoc signing, like local.

### 7. GitHub Release had no .dmg / .app — it was a draft

- **Symptom**: "The release has no artifacts."
- **Cause**: `releaseDraft: true` — the build *did* succeed and attach the
  `.dmg` + `.app.tar.gz`, but a **draft release is invisible in the public
  Releases list** and its asset URLs return 404, so Homebrew can't download it.
  (The artifacts were also not in the Actions "Artifacts" tab, because
  `tauri-action` uploads to Releases.)
- **Fix**: Set `releaseDraft: false` so each tag publishes immediately and the
  asset URL is live for the Homebrew step and `brew install`.

### 8. Homebrew Cask step would hash a 404 page

- **Symptom**: Latent — the cask `sha256` could be computed from an error page.
- **Cause**: The cask step fetched the asset with `curl -sL` (no `-f`), so a 404
  returned `0` and the GitHub error HTML got hashed into a bogus checksum,
  breaking `brew install` with a `sha256 mismatch`.
- **Fix**: Use `curl -fsSL` so a missing asset fails and retries; fail loudly
  (`exit 1`) if the asset never appears.

### 9. DMG name didn't match the tag

- **Symptom**: A `v0.1.1` tag would build `Tokenscope_0.1.0_universal.dmg`,
  which the cask step (computing the name from the tag) couldn't download → 404.
- **Cause**: Tauri names the artifact from the version in `tauri.conf.json`,
  which was still `0.1.0` while the tag was `v0.1.1`.
- **Fix**: Bump the version in `package.json`, `tauri.conf.json`, and
  `Cargo.toml` (+ `Cargo.lock`) so the built DMG name matches the tag the cask
  step expects.

---

## App behavior & packaging

### 10. Two menu-bar icons after reinstall

- **Symptom**: Reinstalling/relaunching left two Tokenscope icons in the menu
  bar.
- **Cause**: No single-instance guard — a second launch started a second
  process with its own tray icon.
- **Fix**: Added `tauri-plugin-single-instance` (registered first) so a second
  launch hands off to the running instance (showing the popover) and exits.

### 11. Unsigned app blocked by Gatekeeper on first open

- **Symptom**: "Apple cannot verify Tokenscope.app is free of malware."
- **Cause**: The build is unsigned/unnotarized, and Homebrew adds a quarantine
  attribute to installed apps.
- **Fix**: Added a `postflight` to the cask that runs `xattr -cr` on the
  installed app, so `brew install` opens cleanly. (The `.dmg` path still needs a
  manual right-click → Open or `xattr -cr`; a full fix needs Developer ID
  signing + notarization.)

### 12. App icon had opaque white corners

- **Symptom**: The rounded app icon showed white square corners in Launchpad.
- **Cause**: The icon PNGs had a white (opaque) background in the corners
  instead of transparent alpha.
- **Fix**: Regenerated the icon from a clean transparent source
  (`scripts/gen_icon.py`, 4× supersampled), then ran `pnpm tauri icon` to
  produce every size + `icon.icns` / `icon.ico` with transparent corners.

---

## UI / charts

### 13. Bar-chart tooltip overlapped the legend above it

- **Symptom**: Hovering a token bar showed its tooltip floating up over the
  Total-tokens "Input … cached" legend, even for short bars.
- **Cause**: To make short bars easy to hover, the hit area was stretched to the
  full column height (`alignSelf: stretch`). The tooltip then anchored to the
  column's `top` — i.e. the top of the chart, right under the legend — so every
  bar's tooltip appeared at the same high spot.
- **Fix** (`charts.tsx` + `tokenscope-panel.html`): anchor the tooltip to the
  *visible bar top* (`r.bottom − barPx`, baseline minus bar height) instead of
  the column top, so short bars get a low tooltip clear of the legend.

### 14. Mockup tooltip drifted to the panel centre (backdrop-filter)

- **Symptom**: In `tokenscope-panel.html` only, the heatmap/bar tooltips
  appeared near the middle of the panel instead of next to the hovered cell/bar.
- **Cause**: The design board's card uses `backdrop-filter: blur(...)`. Like
  `transform`/`filter`, `backdrop-filter` establishes a **containing block for
  `position: fixed`** descendants — so the fixed tooltip anchored to the card,
  not the viewport, and its viewport coordinates landed mid-panel. The real app
  was unaffected (its card is solid, no backdrop-filter, and it needs `fixed` to
  escape the scrolling card).
- **Fix** (`tokenscope-panel.html` only): position the tooltips `absolute`
  relative to each chart's own wrapper (coords offset from the wrapper rect).
  The mockup never scrolls, so it doesn't need `fixed`.

### 15. Total-tokens bar showed slivers when usage was zero

- **Symptom**: With no usage in the period (Total = 0.00M), the input/output
  split bar still showed a small coloured sliver instead of being empty.
- **Cause**: Each segment had `minWidth: 4`, so even a `flexGrow` of `1e-6`
  rendered a 4px block — two slivers when everything was zero.
- **Fix** (`App.tsx` + `tokenscope-panel.html`): give the bar a track background
  and only render the coloured segments when `totalTokens > 0`; otherwise the
  bar is just the empty track.

---

## Theme

### 16. "System" theme mode didn't follow the macOS appearance

- **Symptom**: On macOS, the "System" theme option didn't track the OS dark/light
  mode — neither when toggling system appearance with the popover open, nor after
  quitting and relaunching the app (it stayed on the launch-time appearance).
  Windows was unaffected.
- **Cause**: The frontend derived the system appearance entirely from
  `window.matchMedia("(prefers-color-scheme: dark)")` (`App.tsx`). But Tokenscope
  is an `Accessory` (menu-bar) app whose popover is a **non-activating `NSPanel`**
  that is `order_out`'d (hidden) most of the time. In that configuration
  WKWebView's `prefers-color-scheme` is unreliable: it doesn't reliably fire the
  `change` event on a system theme switch while the webview is hidden, and at
  launch an Accessory app's `NSApp.effectiveAppearance` (what WKWebView reports)
  may not be synced to the current system value — so even a fresh restart reads
  the wrong appearance.
- **Fix**: Read the OS dark-mode setting natively in Rust and push it to the
  frontend via a Tauri event, bypassing the webview. `system_is_dark()` reads
  `NSUserDefaults`'s `AppleInterfaceStyle` (the user's **global** system
  preference, independent of app focus). `watch_system_theme()` listens on
  `NSDistributedNotificationCenter` for `AppleInterfaceThemeChangedNotification`
  — delivered to every registered app regardless of activation policy or
  frontmost status — and `emit("system-theme", dark)`. `setup()` also emits once
  at startup to correct any stale webview value. The frontend's
  `listen("system-theme")` updates `systemDark`; the existing `matchMedia`
  listener stays as the source of truth on Windows / browser preview. macOS-only
  (`#[cfg(target_os = "macos")]`), no new dependencies — uses the `objc`/`cocoa`/
  `block` re-exports already imported in `lib.rs`. (`src-tauri/src/lib.rs`,
  `src/App.tsx`)

### 17. Selected period pill flashed white→transparent on a light→dark switch

- **Symptom**: After the fix above, switching the system theme from light to dark
  while the popover was hidden, then opening it, showed a brief "white →
  transparent" fade on the currently-selected period pill (Day/Week/Month) for a
  moment — most visible element of an otherwise-instant flip.
- **Cause**: The `Segmented` selected pill carries
  `transition: "color .15s, background .15s"` (`charts.tsx`), wanted for smooth
  period-switching. On a *whole-theme* flip this turns every color change into a
  cross-fade; the white selected background fading into the dark one was the most
  jarring. Because the panel is hidden when the theme change lands, the first
  painted frame on open is still the old light theme, then the new theme is
  applied and the transition animates the change visibly.
- **Fix**: Suppress per-property transitions across a theme flip so the panel
  repaints in the new theme in one step. Added a global `.ts-no-transition` rule
  (`main.tsx`) and an effect (`App.tsx`) that adds it to `<html>` when `dark`
  changes and removes it after two `requestAnimationFrame`s. Because rAF callbacks
  don't run while the window is hidden, the class stays on until the popover is
  shown — so the first visible frame is already the new theme with no transition,
  then transitions are restored for normal interactions (e.g. clicking
  Day/Week/Month still animates). Skipped on the very first render.
  (`src/main.tsx`, `src/App.tsx`)

### 18. Total-tokens split bar didn't fill — gray track showed through

- **Symptom**: The split bar under Total tokens was only partly filled, leaving
  gray track visible on the right. It was most obvious with a lopsided split.
- **Cause**: WebKit incorrectly sized segments using `flexGrow` with
  `flexBasis: 0` instead of treating the values as a ratio.
- **Fix** (`src/App.tsx`): use explicit percentage widths for both the per-agent
  bar and the single-agent cached/new bar. The latter now matches its legend:
  dark is cached tokens and light is uncached input plus output.

---

## Notes

- "Month" was also changed from a rolling 30-day window to the **current
  calendar month vs the previous calendar month** — a behavior change requested
  during testing, not a bug.
- "Week" was likewise changed from a rolling last-7-days window to the **current
  calendar week (Monday–Sunday) vs the previous calendar week**, so the delta
  compares this week against last week.
- Delta colors were swapped so usage/cost **up = red** (bad), **down = green**
  (good).
