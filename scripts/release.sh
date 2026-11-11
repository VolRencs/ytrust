#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

version="${1:-}"
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "usage: scripts/release.sh MAJOR.MINOR.PATCH" >&2
  exit 2
fi

if [[ -n "$(git status --porcelain)" ]]; then
  echo "working tree is not clean" >&2
  exit 1
fi

branch="$(git branch --show-current)"
if [[ "$branch" != "main" ]]; then
  echo "release must be created from main (current: $branch)" >&2
  exit 1
fi

sed -i "s/^version = \".*\"/version = \"$version\"/" Cargo.toml
sed -i "s/^version = \".*\"/version = \"$version\"/" node/Cargo.toml
perl -0pi -e 's/"version": "\d+\.\d+\.\d+"/"version": "'"$version"'"/' node/package.json
cp LICENSE node/LICENSE
cargo check >/dev/null
cargo fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked

git add Cargo.toml Cargo.lock node/Cargo.toml node/package.json node/pnpm-lock.yaml node/LICENSE
git commit -m "Release v$version"
git tag "v$version"
git push origin main
git push origin "v$version"

echo "Released v$version"
