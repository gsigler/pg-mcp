#!/usr/bin/env bash
# Sync app version across package.json, package-lock.json, Cargo.toml, Cargo.lock, tauri.conf.json.
set -euo pipefail

NEW="${1:?usage: bump-version.sh <version>}"
export NEW_VERSION="${NEW}"

node -e '
const fs = require("fs");
const newVersion = process.env.NEW_VERSION;

const pkgPath = "package.json";
const pkg = JSON.parse(fs.readFileSync(pkgPath, "utf8"));
pkg.version = newVersion;
fs.writeFileSync(pkgPath, JSON.stringify(pkg, null, 2) + "\n");

const lockPath = "package-lock.json";
const lock = JSON.parse(fs.readFileSync(lockPath, "utf8"));
lock.version = newVersion;
if (lock.packages && lock.packages[""]) {
  lock.packages[""].version = newVersion;
}
fs.writeFileSync(lockPath, JSON.stringify(lock, null, 2) + "\n");

const tauriConfPath = "src-tauri/tauri.conf.json";
const conf = JSON.parse(fs.readFileSync(tauriConfPath, "utf8"));
conf.version = newVersion;
fs.writeFileSync(tauriConfPath, JSON.stringify(conf, null, 2) + "\n");
'

cargo_toml="src-tauri/Cargo.toml"
if grep -q '^version = ' "$cargo_toml"; then
  sed -i "s/^version = \".*\"/version = \"${NEW}\"/" "$cargo_toml"
else
  echo "missing version in $cargo_toml" >&2
  exit 1
fi

cargo_lock="src-tauri/Cargo.lock"
sed -i "/^name = \"pg-mcp\"$/,/^version = / s/^version = \".*\"/version = \"${NEW}\"/" "$cargo_lock"

echo "Bumped all app metadata to ${NEW}"
