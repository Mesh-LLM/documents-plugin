#!/bin/bash
# Check a packaged archive works on its own: unpack it outside the build tree,
# then run its executable for the manifest and a native Tika extraction.
#   scripts/smoke.sh dist/documents-<version>-<target>.<ext>
set -euo pipefail
archive=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
case "$archive" in
  *.zip)
    if command -v 7z >/dev/null; then (cd "$work" && 7z x -bso0 "$archive")
    else powershell -NoProfile -Command "Expand-Archive -LiteralPath '$(cygpath -w "$archive")' -DestinationPath '$(cygpath -w "$work")'"
    fi ;;
  *) tar -xzf "$archive" -C "$work" ;;
esac
exe="$work/documents/documents"
[[ -f $exe.exe ]] && exe="$exe.exe"
for file in plugin.toml plugin-manifest.json README.md; do
  [[ -s $work/documents/$file ]] || { echo "missing $file" >&2; exit 1; }
done
ls -l "$work/documents"
# No cargo-provided library paths: the archive has to find Tika by itself.
env -u LD_LIBRARY_PATH -u DYLD_LIBRARY_PATH -u DYLD_FALLBACK_LIBRARY_PATH \
  "$exe" --packaged-manifest > "$work/manifest.json"
grep -q '"documents"' "$work/manifest.json"
env -u LD_LIBRARY_PATH -u DYLD_LIBRARY_PATH -u DYLD_FALLBACK_LIBRARY_PATH \
  "$exe" --extract-worker --deadline-secs 120 < "$root/tests/fixtures/smoke.pdf" > "$work/out.xhtml"
grep -q "Hello from the pdf-plugin smoke test" "$work/out.xhtml" \
  || { echo "extraction failed:" >&2; head -c 2000 "$work/out.xhtml" >&2; exit 1; }
echo "smoke ok: $(basename "$archive")"
