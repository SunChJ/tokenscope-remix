#!/usr/bin/env node

import fs from "node:fs";

const requested = process.argv[2];
const semver = /^\d+\.\d+\.\d+$/;

function readJson(path) {
  return JSON.parse(fs.readFileSync(path, "utf8"));
}

function replaceVersion(path, pattern, version) {
  const source = fs.readFileSync(path, "utf8");
  if (!pattern.test(source)) throw new Error(`version field not found in ${path}`);
  fs.writeFileSync(path, source.replace(pattern, (_match, before, after) => `${before}${version}${after}`));
}

if (requested) {
  const version = requested.replace(/^v/, "");
  if (!semver.test(version)) throw new Error("version must use X.Y.Z");

  replaceVersion("package.json", /^(\s*"version":\s*")[^"]+(".*)$/m, version);
  replaceVersion("src-tauri/tauri.conf.json", /^(\s*"version":\s*")[^"]+(".*)$/m, version);
  replaceVersion("src-tauri/Cargo.toml", /^(version = ")[^"]+(".*)$/m, version);
  replaceVersion(
    "src-tauri/Cargo.lock",
    /(\[\[package\]\]\nname = "tokenscope"\nversion = ")[^"]+(".*)/,
    version,
  );
}

const versions = {
  package: readJson("package.json").version,
  tauri: readJson("src-tauri/tauri.conf.json").version,
  cargo: fs.readFileSync("src-tauri/Cargo.toml", "utf8").match(/^version = "([^"]+)"/m)?.[1],
  lock: fs.readFileSync("src-tauri/Cargo.lock", "utf8")
    .match(/\[\[package\]\]\nname = "tokenscope"\nversion = "([^"]+)"/)?.[1],
};
const unique = new Set(Object.values(versions));
if (unique.size !== 1 || unique.has(undefined)) {
  throw new Error(`version mismatch: ${JSON.stringify(versions)}`);
}
if (!semver.test(versions.package)) throw new Error("version must use X.Y.Z");

process.stdout.write(versions.package);
