#!/bin/sh
# Builds the extension for this machine into bin/ and runs tests/real.py headlessly against a rhun
# binary, with a real kernel: a notebook opened, cells run (a stream and a result, a matplotlib
# figure, an exception, an interrupt, an updated display, HTML and JSON), completion from the
# kernel, a save nbformat validates, a restart, and a file nbformat wrote saved back byte for byte.
# usage: tests/run.sh [RHUN]. JUPYTER_TEST_PROJECT names a project whose .venv has ipykernel,
# matplotlib and nbformat; without it one is made in a temporary folder (pip needs the network).
set -e
cd "$(dirname "$0")/.."
RHUN=${1:-$(command -v rhun || echo "$HOME/.local/bin/rhun")}
name=$(sed -n 's/^name *= *//p' extension.ini | head -1)
case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) target=linux-x86_64 ;;
    Darwin-arm64) target=macos-arm64 ;;
    *) echo "no target for $(uname -s)-$(uname -m)"; exit 77 ;;
esac
cargo build --release --quiet
mkdir -p bin
cp "target/release/$name" "bin/$name-$target"
if [ -z "$JUPYTER_TEST_PROJECT" ]; then
    work=$(mktemp -d)
    trap 'rm -rf "$work"' EXIT
    JUPYTER_TEST_PROJECT=$work/proj
    mkdir -p "$JUPYTER_TEST_PROJECT"
    python3 -m venv "$JUPYTER_TEST_PROJECT/.venv"
    "$JUPYTER_TEST_PROJECT/.venv/bin/python" -m pip install --quiet --disable-pip-version-check ipykernel matplotlib nbformat
fi
RHUN=$RHUN JUPYTER_TEST_PROJECT=$JUPYTER_TEST_PROJECT python3 tests/real.py
