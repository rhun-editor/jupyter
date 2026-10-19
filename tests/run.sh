#!/bin/sh
# tests/run.sh: unit tests, clippy, then (when they are set) the protocol checker and the real-kernel
# workflow in rhun. CHECK=path/to/rhun-ext-check, RHUN=path/to/rhun, JUPYTER_TEST_PROJECT=a project
# whose .venv has ipykernel, matplotlib and nbformat.
set -e
cd "$(dirname "$0")/.."
cargo test --offline
cargo clippy --offline --all-targets -- -D warnings
cargo build --release --offline
case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) target=macos-arm64 ;;
    Linux-x86_64) target=linux-x86_64 ;;
    *) target=windows-x64 ;;
esac
mkdir -p bin
cp target/release/jupyter bin/jupyter-$target
if [ -n "$CHECK" ]; then
    "$CHECK" .
fi
python3 tests/real.py
