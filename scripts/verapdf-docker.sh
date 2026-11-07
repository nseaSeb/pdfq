#!/usr/bin/env bash
# `verapdf` through its official container, for machines without a local install
# (CI). The PDF to validate must be the last argument.
set -euo pipefail
image=ghcr.io/verapdf/cli:v1.31.217@sha256:635c849e663fcae5477e30ef2447e4740db2e197532e0f337d4b38f0c03461a5
file=${*: -1}
dir=$(cd "$(dirname "$file")" && pwd)
exec docker run --rm -v "$dir:$dir:ro" "$image" "${@:1:$#-1}" "$dir/$(basename "$file")"
