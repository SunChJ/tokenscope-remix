# Tokenscope Remix

## Repo rules

- **Never open pull requests against `HduSy/tokenscope`.** That repo is stale
  (last release v1.0.5) and not part of the workflow; `gh pr create` defaults to
  it when an `upstream` remote is present, so pass the repo explicitly.
- Always target this repo's own `main` branch:
  `gh pr create --repo SunChJ/tokenscope-remix --base main --head <branch>`.
- Merging to `main` with a version bump in `package.json`,
  `src-tauri/Cargo.toml`, or `src-tauri/tauri.conf.json` triggers the release
  workflow (build + publish). Bump with `node scripts/version.mjs <X.Y.Z>` —
  it keeps package.json, tauri.conf.json, Cargo.toml, and Cargo.lock in sync.
