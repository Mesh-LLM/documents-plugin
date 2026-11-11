#!/bin/bash
# Build an installable archive: dist/pdf-<version>-<target>.tar.gz
#   mesh-llm plugins install --archive dist/pdf-....tar.gz --name pdf
set -euo pipefail
version=${1:-$(sed -n 's/^version = "\(.*\)"/\1/p' "$(dirname "$0")/../plugin.toml")}
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
cargo build --locked --release ${CARGO_OFFLINE:+--offline}
target_dir=${CARGO_TARGET_DIR:-target}
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
pkg="$stage/pdf"
mkdir -p "$pkg" dist
cp README.md "$pkg/"
# The host expects the executable to carry the plugin name.
cp "$target_dir/release/mesh-pdf-plugin" "$pkg/pdf"
sed "s/^version = .*/version = \"${version#v}\"/" plugin.toml > "$pkg/plugin.toml"
"$pkg/pdf" --packaged-manifest > "$pkg/plugin-manifest.json"
name="pdf-$version-$(rustc -vV | sed -n 's/^host: //p').tar.gz"
COPYFILE_DISABLE=1 tar -czf "dist/$name" -C "$stage" pdf
echo "dist/$name"
