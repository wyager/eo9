#!/usr/bin/env sh
# Validate every eo9 WIT package with wasm-tools:
#   1. parse + print the WIT text            (wasm-tools component wit <pkg>)
#   2. encode to the binary WIT package      (wasm-tools component wit <pkg> --wasm)
#   3. validate the binary encoding          (requires the cm-async feature for future/stream)
#   4. round-trip the binary back to WIT text
#
# Usage: wit/check.sh
set -eu
cd "$(dirname "$0")"

packages="io rt text time entropy perf exec disk fs net pci sandbox"

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT INT TERM

for pkg in $packages; do
    if [ ! -d "$pkg" ]; then
        printf 'eo9:%s ... missing (skipped)\n' "$pkg"
        continue
    fi
    printf 'eo9:%s ... ' "$pkg"
    wasm-tools component wit "$pkg" > "$tmpdir/$pkg.wit"
    wasm-tools component wit "$pkg" --wasm --output "$tmpdir/$pkg.wasm"
    wasm-tools validate --features cm-async "$tmpdir/$pkg.wasm"
    wasm-tools component wit "$tmpdir/$pkg.wasm" > "$tmpdir/$pkg.roundtrip.wit"
    printf 'ok\n'
done

echo "all wit packages validate"
