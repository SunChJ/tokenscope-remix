# 001 — LaunchServices Login Startup: Plan

- Status: Implemented (release verification pending)
- Anchor date: 2026-09-07
- Release: 1.6.8

## Background

On macOS 26.6, the installed 1.6.7 app was rejected when a legacy LaunchAgent
executed its ad-hoc-signed binary directly. The same installed bundle launched
successfully through LaunchServices. Changing the local agent to
`/usr/bin/open -g /Applications/Tokenscope.app` and bootstrapping it returned exit
code zero and started the app. This local diagnostic validation preceded this
implementation plan; no product code had been changed.

The current autostart plugin checks only whether its plist exists. It does not
repair stale paths or distinguish a development executable from an installed
application. The tray also swallows registration errors.

## Goals

- Keep the existing launch-agent label and saved preference.
- Register `/usr/bin/open -g <installed bundle>` on macOS.
- Repair existing direct-executable or malformed registrations when enabled.
- Honor opt-out, including removal of legacy registrations.
- Do not change production login settings from debug or uninstalled builds.
- Report registration failures and avoid persisting an unfulfilled toggle.
- Publish a patch release and verify the Homebrew-installed artifact.

## Approach

Add a small macOS-specific registration module. Serialize the plist with the
existing dependency ecosystem and atomically replace it; test with temporary
directories, never the real login settings. Keep the existing plugin for other
platforms. Native tray actions remain the only login-setting interface.

Only accept the standard system or per-user Applications installation location.
Do not shell-interpolate paths. Compare the actual registration with the expected
configuration instead of treating file existence as success. File changes take
effect at the next login; avoid unloading a legacy job from inside its running
process, which could terminate the app during migration.

## Non-goals and alternatives

- No Developer ID signing, notarization, or global security-policy changes.
- No broad quarantine removal added to application code.
- No custom launch daemon or background helper.
- Do not fork the Tauri plugin just to override an executable path.
- Defer signing issues in the bundled quota helper; this patch covers app login
  startup, not a guarantee that every quota provider works.

## Validation

Run regression tests for plist arguments/escaping, legacy migration, opt-out,
malformed entries, write failures, idempotence, and development isolation. Run
Rust tests and frontend build, check version consistency, then verify CI assets,
the Homebrew cask, installed version, and a LaunchAgent bootstrap. A bootstrap is
not a full logout/login test and does not establish Gatekeeper trust.

## Amendments

- Updated 2026-09-07: CI must prepare the bundled helper before running Rust
  tests; recovery builds use the existing version tag. See
  [002-release-verification.md](002-release-verification.md).
