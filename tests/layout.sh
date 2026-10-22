#!/bin/sh
# Copies the files the extension ships into DEST, the folder rhun installs (protocol section 1):
# everything at the top of this repository except the development and build files listed below.
# That is extension.ini, README.md, LICENSE, the icon, and whatever the manifest names or rhun
# reads from the folder: themes/, syntax/, icons/ ([icons]), icon-themes/, debugger templates
# and adapters, a runtime's script, anything else you add. The program is not copied: the caller
# builds it into DEST/bin. Names starting with a dot are never copied (rhun skips them, and
# .rhun-install is rhun's own). A link fails: rhun refuses an archive that holds one.
# usage: sh tests/layout.sh DEST (tests/run.sh, tests/check.sh and the release workflow use it)
set -e
dest=$1
[ -n "$dest" ] || { echo "usage: tests/layout.sh DEST" >&2; exit 2; }
mkdir -p "$dest"
dest=$(cd "$dest" && pwd)
cd "$(dirname "$0")/.."
for f in *; do
    case $f in
        # development and build: the tests, notes for agents, the program (built fresh), archives
        tests | AGENTS.md | CLAUDE.md | bin | dist | out | target) continue ;;
        # the Rust source and the crate's files; add your own source folders here
        src | build.rs | Cargo.toml | Cargo.lock | rust-toolchain | rust-toolchain.toml) continue ;;
        rustfmt.toml | clippy.toml | deny.toml) continue ;;
    esac
    cp -R "$f" "$dest/"
done
links=$(find "$dest" -type l)
if [ -n "$links" ]; then
    echo "rhun refuses an extension archive with links; copy the files instead:" >&2
    echo "$links" >&2
    exit 1
fi
