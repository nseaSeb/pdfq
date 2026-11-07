#!/usr/bin/env bash
# Downloads the @preview packages of a list into DEST/preview/<name>/<version>,
# verifying each tarball's sha256, then fails if any package imports an
# @preview package that is not in the list (no download at runtime).
# With SMOKE, also checks that the smoke document imports every package no
# other listed package depends on (so each one is compiled at build time).
# Usage: scripts/fetch-packages.sh DEST [LIST] [SMOKE]
set -euo pipefail
dest=${1:?usage: fetch-packages.sh DEST [LIST]}
list=${2:-packages/packages.txt}
smoke=${3:-}
registry=${PDFQ_PACKAGE_REGISTRY:-https://packages.typst.org/preview}

sha256() {
  if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1
}

mkdir -p "$dest/preview"
tmp=$(mktemp -d)
trap 'rm -rf -- "$tmp"' EXIT
listed=" "
while read -r name version hash; do
  case "$name" in '' | '#'*) continue ;; esac
  [[ -n "${hash:-}" ]] || { echo "missing sha256 for $name:$version" >&2; exit 1; }
  tarball="$tmp/$name-$version.tar.gz"
  curl -fsSL --retry 3 -o "$tarball" "$registry/$name-$version.tar.gz"
  actual=$(sha256 "$tarball")
  if [[ "$actual" != "$hash" ]]; then
    echo "sha256 mismatch for $name:$version: expected $hash, got $actual" >&2
    exit 1
  fi
  mkdir -p "$dest/preview/$name/$version"
  # Files must stay readable by the non-root runtime user, whatever the archive says.
  tar -xzf "$tarball" -C "$dest/preview/$name/$version" --no-same-owner --no-same-permissions
  chmod -R a+rX "$dest/preview/$name/$version"
  listed="$listed$name:$version "
  echo "fetched @preview/$name:$version"
done <"$list"

# Imports may appear in code mode (`import "@preview/x:1.0.0"`), so match the
# spec anywhere in .typ files.
missing=0
while read -r spec; do
  if [[ "$listed" != *" ${spec#@preview/} "* ]]; then
    echo "unlisted dependency: $spec" >&2
    missing=1
  fi
done < <( (grep -rhoE '@preview/[a-z0-9-]+:[0-9]+\.[0-9]+\.[0-9]+' "$dest/preview" --include='*.typ' || true) | sort -u)
if [[ -n "$smoke" ]]; then
  # No match is fine (grep exits 1): packages without dependencies.
  deps=$( (grep -rhoE '@preview/[a-z0-9-]+:[0-9]+\.[0-9]+\.[0-9]+' "$dest/preview" --include='*.typ' || true) | sort -u)
  for spec in $listed; do
    if ! grep -qF "@preview/$spec" "$smoke" && ! grep -qxF "@preview/$spec" <<<"$deps"; then
      echo "$spec is not imported by $smoke" >&2
      missing=1
    fi
  done
fi
exit "$missing"
