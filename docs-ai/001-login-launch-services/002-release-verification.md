# 001.002 — Release Verification

## CI resource preparation

PR #38 merged the application fix and prepared the `v1.6.8` tag. The first
release run (34108332795) failed before tests could execute: Tauri's build script
requires `bin/hu`, which is ignored by Git and normally downloaded by the later
Tauri packaging hook. The local checkout already contained that resource.

The workflow now runs the existing helper download script before Rust tests and
checks out the prepared tag for platform builds. This permits workflow-only
recovery from `main` while keeping the published application source pinned to
`v1.6.8`; neither the tag nor application history is rewritten.

Validation from a fresh `git archive v1.6.8` extraction, after running
`bash src-tauri/bin/build-hu.sh`: 55 Rust tests passed, one existing private-log
audit ignored. This check did not use the developer checkout's generated helper
or frontend output.

## Publication and installation

- PR #39 supplied the workflow-only fix. Recovery run
  [34108802863](https://github.com/SunChJ/tokenscope-remix/actions/runs/34108802863)
  succeeded: prepare, both architecture jobs, and publish.
- [Version 1.6.8](https://github.com/SunChJ/tokenscope-remix/releases/tag/v1.6.8)
  became public on 2026-09-07 with both DMGs, both application archives, both
  signature files, and `latest.json`.
- Updater metadata reports 1.6.8 and contains both Darwin architectures. Its
  signature strings match the corresponding downloaded signature assets; an
  independent cryptographic verification was not performed.
- The Homebrew tap updated automatically (`8a4c329`). A cask upgrade installed
  1.6.8 at `/Applications/Tokenscope.app`.
- With the app stopped and the local job unloaded, a stale, non-running plist
  was supplied. Launching the installed release repaired its arguments to
  `/usr/bin/open -g /Applications/Tokenscope.app`, restored `RunAtLoad`, and
  preserved the enabled preference.
- After quitting the app, `launchctl bootstrap` of that repaired plist returned
  success. The job exited zero and exactly one installed app process remained
  running. The helper job being inactive after `open` exits is expected.
- `codesign --verify --deep --strict` passed for the installed bundle. This is
  integrity validation, not Developer ID or Gatekeeper trust.

Full logout/login, Intel runtime testing, and quota-provider functionality were
not verified. No global security policy was changed.
