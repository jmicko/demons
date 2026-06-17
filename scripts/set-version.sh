#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<'USAGE'
usage: scripts/set-version.sh <version>

Updates the crate version in Cargo.toml and the root demons package entry in
Cargo.lock. The version must be SemVer-like, for example 0.2.0 or 1.0.0-beta.1.
USAGE
}

if [[ $# -ne 1 ]]; then
  usage
  exit 2
fi

version=$1
if [[ ! $version =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$ ]]; then
  echo "error: invalid version: $version" >&2
  usage
  exit 2
fi

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

export DEMONS_VERSION="$version"
perl -0pi -e 's/(\[package\].*?\nversion = ")[^"]+(")/$1$ENV{DEMONS_VERSION}$2/s' Cargo.toml
perl -0pi -e 's/(\[\[package\]\]\nname = "demons"\nversion = ")[^"]+(")/$1$ENV{DEMONS_VERSION}$2/' Cargo.lock

cargo check --locked

echo "Updated demons to version $version."
echo "Next: run make release-check, review the diff, then commit and tag v$version."
