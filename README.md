# Jupyter

Jupyter notebooks (`.ipynb`) in [rhun](https://rhun.app): the extension reads and writes the
files and runs their cells in the Jupyter kernels your Python environments have. A Rust program
built on [rhun-ext](https://github.com/rhun-editor/extension-protocol/tree/main/rust/rhun-ext);
rhun draws the notebook (cells, outputs, the toolbar and the keys of its notebook tab).

- **Open** a `.ipynb` file and it opens as a notebook. Saving writes nbformat 4 as Jupyter does,
  and a file nothing changed in is written back byte for byte; cell metadata and outputs rhun
  cannot show are kept as they were.
- **Run cells** with Shift+Enter, the toolbar or a cell's `[ ]`. Outputs come as they are
  printed: streams, results, errors with their traceback, images (a matplotlib figure),
  displays that update, and HTML, Markdown, JSON and LaTeX as rich documents or readable text
  (an HTML table, such as a pandas DataFrame, as a table).
- **Interrupt** and **restart** the kernel from the toolbar or the palette.
- **Completion** in a Python cell comes from the running kernel, so it knows the
  names your earlier cells made.

The kernel is the notebook's own (its kernelspec, when installed), else the first Python with
`ipykernel` found in a `.venv`, `venv` or `env` folder next to the notebook or above it up to the
project, then `python3` or `python` on your PATH, then installed kernelspecs. The button at the
right of the notebook's toolbar picks another. Without any, run

```sh
python3 -m venv .venv
.venv/bin/python -m pip install ipykernel
```

in the project and run the cell again.

Install it from rhun's Extensions page (the extensions button in the title bar): **Install from
GitHub…**, then `rhun-editor/jupyter`. Or take `jupyter-TARGET.tar.gz` for your system from the
[latest release](https://github.com/rhun-editor/jupyter/releases/latest) and use **Install** >
**From an Archive**. It needs rhun 0.18 (extension protocol 1.1: notebooks).

## Permissions

It reads the notebooks you open, your project (for its Python environments) and your home folder
(installed kernelspecs). It runs `python3` or `python` only to see which environments have
ipykernel. Kernels run as you, outside the extension's sandbox: the first time it starts one in a
project, rhun shows the command and asks; **Forget** on the extension's page takes that back. A
kernel listens on the loopback network, where the extension talks to it (ZeroMQ over TCP); the
extension uses the network for nothing else.

## Build and test

```sh
cargo test              # .ipynb round trips, HTML outputs, kernel message parts
cargo build --release
tests/run.sh [RHUN]     # headless with a real kernel, against the rhun on your PATH
tests/check.sh          # the protocol's conformance checker (needs Go)
```

`tests/run.sh` builds the extension into `bin/` and runs `tests/real.py`: a notebook opened in a
headless rhun, its cells run in an ipykernel rhun starts after asking (a stream and a result, a
matplotlib figure, an exception, an interrupt, an updated display, HTML and JSON), completion from
the kernel, a save nbformat validates and a restart. Set `JUPYTER_TEST_PROJECT` to a project
whose `.venv` has ipykernel, matplotlib and nbformat; without it the script makes one with pip.
A tag `vX.Y.Z` equal to `version` in `extension.ini` builds Linux, macOS and Windows on their own
runners and publishes what Install from GitHub takes.
