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

Pending the corrected workflow, public assets, cask update, and installed-app
migration/bootstrap verification. Full logout/login testing remains out of scope.
