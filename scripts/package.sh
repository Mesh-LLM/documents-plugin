#!/bin/bash
# Build an installable archive for this machine's target:
#   dist/pdf-<version>-<target>.tar.gz (.zip on Windows), plus the stable alias
#   dist/pdf-<target>.<ext> that `mesh-llm plugins install` also accepts.
#   mesh-llm plugins install --archive dist/pdf-....tar.gz --name pdf
# Set PDF_SKIP_BUILD=1 to package an existing release build.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
version=${1:-$(sed -n 's/^version = "\(.*\)"/\1/p' plugin.toml)}
version=${version#v}
target_dir=${CARGO_TARGET_DIR:-target}
target=$(rustc -vV | sed -n 's/^host: //p')
if [[ -z ${PDF_SKIP_BUILD:-} ]]; then
  cargo build --locked --release ${CARGO_OFFLINE:+--offline}
fi

exe=""
case "$target" in
  *-windows-*) exe=".exe"; lib_glob="libtika_native.dll"; ext=zip ;;
  *-apple-darwin) lib_glob="libtika_native.dylib"; ext=tar.gz ;;
  *) lib_glob="libtika_native.so"; ext=tar.gz ;;
esac

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
pkg="$stage/pdf"
mkdir -p "$pkg" dist
cp README.md "$pkg/"
# The host expects the executable to carry the plugin name.
cp "$target_dir/release/mesh-pdf-plugin$exe" "$pkg/pdf$exe"
# Tika, compiled to a native library (no JVM), ships next to the executable.
lib=$(ls -t "$target_dir"/release/build/extractous-*/out/libs/$lib_glob | head -1)
cp "$lib" "$pkg/"
case "$target" in
  *-apple-darwin)
    # The build links the library by its absolute build path: load it from beside the executable.
    old=$(otool -D "$lib" | tail -1)
    install_name_tool -id @rpath/libtika_native.dylib "$pkg/libtika_native.dylib"
    install_name_tool -change "$old" @rpath/libtika_native.dylib -add_rpath @loader_path "$pkg/pdf"
    codesign -f -s - "$pkg/libtika_native.dylib" "$pkg/pdf"
    ;;
  *-linux-*)
    # Find the library beside the executable, not in the build tree.
    patchelf --set-rpath '$ORIGIN' "$pkg/pdf"
    ;;
  # Windows loads DLLs from the executable's directory first.
esac
sed "s/^version = .*/version = \"$version\"/" plugin.toml > "$pkg/plugin.toml"
"$pkg/pdf$exe" --packaged-manifest > "$pkg/plugin-manifest.json"

versioned="dist/pdf-$version-$target.$ext"
stable="dist/pdf-$target.$ext"
rm -f "$versioned" "$stable"
if [[ $ext == zip ]]; then
  (cd "$stage" && 7z a -tzip -bso0 "$root/$versioned" pdf)
else
  COPYFILE_DISABLE=1 tar -czf "$versioned" -C "$stage" pdf
fi
cp "$versioned" "$stable"
echo "$versioned"
