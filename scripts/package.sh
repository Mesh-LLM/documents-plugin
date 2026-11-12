#!/bin/bash
# Build an installable archive for this machine's target:
#   dist/documents-<version>-<target>.tar.gz (.zip on Windows), plus the stable alias
#   dist/documents-<target>.<ext> that `mesh-llm plugins install` also accepts.
#   mesh-llm plugins install --archive dist/documents-....tar.gz --name documents
# Set DOCUMENTS_SKIP_BUILD=1 to package an existing release build.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
version=${1:-$(sed -n 's/^version = "\(.*\)"/\1/p' plugin.toml)}
version=${version#v}
target_dir=${CARGO_TARGET_DIR:-target}
target=$(rustc -vV | sed -n 's/^host: //p')
if [[ -z ${DOCUMENTS_SKIP_BUILD:-} ]]; then
  cargo build --locked --release ${CARGO_OFFLINE:+--offline}
fi

exe=""
case "$target" in
  *-windows-*) exe=".exe"; lib_ext=dll; ext=zip ;;
  *-apple-darwin) lib_ext=dylib; ext=tar.gz ;;
  *) lib_ext=so; ext=tar.gz ;;
esac

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
pkg="$stage/documents"
mkdir -p "$pkg" dist
cp README.md "$pkg/"
# The host expects the executable to carry the plugin name.
cp "$target_dir/release/mesh-documents-plugin$exe" "$pkg/documents$exe"
# Tika, compiled to a native library (no JVM), ships next to the executable,
# with the JDK libraries it loads at run time from its own folder (libawt on Linux).
lib=$(ls -t "$target_dir"/release/build/extractous-*/out/libs/libtika_native.$lib_ext | head -1)
cp "$(dirname "$lib")"/*.$lib_ext "$pkg/"
case "$target" in
  *-apple-darwin)
    # The build links the library by its absolute build path: load it from beside the executable.
    old=$(otool -D "$lib" | tail -1)
    install_name_tool -id @rpath/libtika_native.dylib "$pkg/libtika_native.dylib"
    install_name_tool -change "$old" @rpath/libtika_native.dylib -add_rpath @loader_path "$pkg/documents"
    codesign -f -s - "$pkg/libtika_native.dylib" "$pkg/documents"
    ;;
  *-linux-*)
    # Find the library beside the executable, not in the build tree.
    patchelf --set-rpath '$ORIGIN' "$pkg/documents"
    ;;
  # Windows loads DLLs from the executable's directory first.
esac
sed "s/^version = .*/version = \"$version\"/" plugin.toml > "$pkg/plugin.toml"
"$pkg/documents$exe" --packaged-manifest > "$pkg/plugin-manifest.json"

versioned="dist/documents-$version-$target.$ext"
stable="dist/documents-$target.$ext"
rm -f "$versioned" "$stable"
if [[ $ext == zip ]]; then
  (cd "$stage" && 7z a -tzip -bso0 "$root/$versioned" documents)
else
  COPYFILE_DISABLE=1 tar -czf "$versioned" -C "$stage" documents
fi
cp "$versioned" "$stable"
echo "$versioned"
