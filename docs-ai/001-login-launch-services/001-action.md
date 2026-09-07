# 001 — LaunchServices Login Startup: Outcome

## Timeline

| Date | Change | Ref |
| --- | --- | --- |
| 2026-09-07 | Confirmed direct LaunchAgent failure and successful LaunchServices bootstrap on macOS 26.6. | Local diagnostics, not committed |
| 2026-09-07 | Added macOS registration management, regression tests, and version 1.6.8. | #38 |
| 2026-09-07 | Fixed clean-runner resource preparation and published the existing version tag. | #39 |
| 2026-09-07 | Upgraded through Homebrew; verified installed-app migration and LaunchAgent startup. | [Release verification](002-release-verification.md) |

## Implemented behavior

- `src-tauri/src/login_startup.rs` owns the existing macOS plist, validates its
  content, and writes `/usr/bin/open -g <installed bundle>` atomically.
- Only installed release builds manage login startup. Debug and uninstalled
  builds leave production registration and preferences untouched.
- `src-tauri/src/lib.rs` routes native tray actions through the platform backend,
  logs failures, and persists only verified changes. Saved opt-out is preserved.
- The previous Tauri plugin remains the non-macOS backend. Unused webview login
  permissions were removed from `src-tauri/capabilities/default.json`.
- `plist` and `tempfile` are now explicit macOS dependencies; both were already
  present transitively in the lockfile.
- Release CI runs Rust regression tests before packaging both architectures.

## Validation

- Red: all five new login-startup tests failed against stub implementations.
- Green: Rust library suite passed 55 tests, with one pre-existing private-log
  audit intentionally ignored. The same suite passed after the 1.6.8 bump.
- `pnpm build`: passed TypeScript checking and Vite production build.
- `node scripts/version.mjs`: all four version sources agree on 1.6.8.
- `git diff --check`: passed.
- The locally modified 1.6.7 LaunchAgent bootstrapped through `/usr/bin/open`,
  returned zero, and launched the installed app. The published 1.6.8 app later
  repaired a deliberately stale registration and passed the same bootstrap check;
  see [release verification](002-release-verification.md).

## Boundaries and deviations

The local workaround was tested before product implementation, as recorded in
the plan. No global security policy was modified. No full logout/login or
non-macOS runtime test was performed. Existing `block` dependency emits a Rust
future-incompatibility warning. Quota-helper signing is not addressed here.

## Release state

Version 1.6.8 is public, both platform jobs and the cask update succeeded, and the
Homebrew-installed Apple Silicon app passed migration and startup checks.
Updater metadata was checked against both signature assets; this was not an
independent cryptographic verification. Full logout/login and Intel runtime
validation remain unperformed.
