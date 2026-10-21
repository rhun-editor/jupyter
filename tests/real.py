#!/usr/bin/env python3
"""The jupyter extension in rhun with a real kernel: rhun runs headless with its control socket,
the extension from this repository, and a project whose .venv has ipykernel, matplotlib and
nbformat. Opens a notebook, runs cells (a stream and a result, a matplotlib figure, an exception,
an interrupt, an updated display, HTML and JSON displays) in a kernel rhun starts as the user
after asking (nb-exec), completion in a cell from the kernel,
saves, validates the file with nbformat, restarts the kernel;
and opens a file nbformat wrote, saving it back byte for byte.

    RHUN=/path/to/rhun JUPYTER_TEST_PROJECT=/path/to/project python3 tests/real.py

The project must have .venv with ipykernel, matplotlib and nbformat (python -m pip install ...).
"""
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
RHUN = os.environ.get('RHUN')
PROJECT = os.environ.get('JUPYTER_TEST_PROJECT')
CONFIG = ('[ui]\nsidebar = false\nagents_panel = false\n[editor]\ncursor_blink = false\n'
          '[files]\nrestore_session = false\nrestore_project = false\n'
          '[updates]\ncheck = false\n[git]\nenabled = false\n')
TARGET = {'darwin': 'macos-arm64', 'linux': 'linux-x86_64', 'win32': 'windows-x64'}[sys.platform]


def code(src):
    return {'cell_type': 'code', 'execution_count': None, 'id': f'c{abs(hash(src)) % 10**8}',
            'metadata': {}, 'outputs': [], 'source': src}


NOTEBOOK = {
    'cells': [
        {'cell_type': 'markdown', 'id': 'm1', 'metadata': {}, 'source': '# Demo\nA *real* kernel.'},
        code('print("hello")\n6 * 7'),
        code('import matplotlib.pyplot as plt\nplt.plot([1, 3, 2])\nplt.show()'),
        code('1 / 0'),
        code('import time\nprint("sleeping", flush=True)\ntime.sleep(60)'),
        code('from IPython.display import display\nh = display("one", display_id=True)\nh.update("two")'),
        code('import os\nos.pa'),
        code('from IPython.display import HTML, JSON, display\n'
             'display(HTML("<p>an <b>HTML</b> output</p><table><tr><th>k</th></tr><tr><td>1</td></tr></table>"))\n'
             'display(JSON({"k": [1, 2]}))'),
    ],
    'metadata': {'kernelspec': {'display_name': 'Python 3 (ipykernel)', 'language': 'python', 'name': 'python3'}},
    'nbformat': 4, 'nbformat_minor': 5,
}


def field(text, cell, name):
    for line in text.splitlines():
        if line.startswith(f'cell={cell} '):
            for f in line.split(' '):
                if f.startswith(name + '='):
                    return f[len(name) + 1:]
    return None


@unittest.skipUnless(RHUN and PROJECT, 'set RHUN and JUPYTER_TEST_PROJECT')
class RealKernel(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='rhun-jp-', dir='/tmp')
        self.work = Path(self.tmp.name).resolve()
        self.proj = Path(PROJECT).resolve()
        self.sub = Path(tempfile.mkdtemp(prefix='real-', dir=self.proj))
        self.python = self.proj / '.venv/bin/python'
        exts = self.work / 'exts/rhun-editor/jupyter'
        (exts / 'bin').mkdir(parents=True)
        shutil.copy(ROOT / 'extension.ini', exts)
        shutil.copy(ROOT / f'bin/jupyter-{TARGET}', exts / 'bin')
        config = self.work / 'config/rhun/config'
        config.parent.mkdir(parents=True)
        config.write_text(CONFIG)
        self.state = self.work / 'state'
        env = dict(os.environ, HOME=str(self.work), XDG_CONFIG_HOME=str(self.work / 'config'),
                   XDG_STATE_HOME=str(self.state), XDG_DATA_HOME=str(self.work / 'data'))
        control = self.work / 'c'
        self.process = subprocess.Popen([RHUN, str(self.proj), '--headless', '1000x700', '--scale', '1',
                                         '--extensions', str(self.work / 'exts'), '--control', str(control)],
                                        env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        self.client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.client.settimeout(30)
        deadline = time.monotonic() + 10
        while True:
            try:
                self.client.connect(str(control))
                break
            except (FileNotFoundError, ConnectionRefusedError):
                if self.process.poll() is not None or time.monotonic() > deadline:
                    self.fail('rhun did not start')
                time.sleep(0.01)
        self.reader = self.client.makefile('r', encoding='utf-8')

    def tearDown(self):
        try:
            self.command('quit')
        except Exception:
            pass
        try:
            self.process.wait(timeout=15)
        except subprocess.TimeoutExpired:
            self.process.terminate()
            self.process.wait(timeout=10)
        self.process.stderr.close()
        self.reader.close()
        self.client.close()
        # no kernel outlives rhun and the extension
        time.sleep(1)
        left = subprocess.run(['pgrep', '-f', str(self.state)], capture_output=True, text=True).stdout.split()
        for pid in left:
            os.kill(int(pid), 9)
        shutil.rmtree(self.sub, ignore_errors=True)
        self.tmp.cleanup()
        self.assertEqual(left, [], 'kernel processes left running')

    def command(self, line):
        self.client.sendall((line + '\n').encode('utf-8'))
        output = []
        while True:
            reply = self.reader.readline()
            if reply == 'ok\n':
                return ''.join(output)
            self.assertNotIn(reply, ('', 'error\n'), line)
            output.append(reply)

    def wait_for(self, check, timeout=60):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            out = self.command('print-notebook')
            if check(out):
                return out
            self.command('wait 100')
        self.fail(f'timed out; last: {out}')

    def approve(self):
        """The kernel runs as the user: rhun asks once per extension and project."""
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            out = self.command('print-dialog')
            if 'dialog=Let Jupyter run programs' in out:
                self.assertIn('.json', out)  # the command, its middle cut short
                self.command('dialog yes')
                return
            self.command('wait 100')
        self.fail('no question before the kernel started')

    def state_of(self, cell, want, timeout=60):
        return self.wait_for(lambda o: field(o, cell, 'state') == want, timeout)

    def test_run_save_restart(self):
        nb = self.sub / 'demo.ipynb'
        nb.write_text(json.dumps(NOTEBOOK, indent=1))
        self.command('open ' + str(nb))
        out = self.wait_for(lambda o: 'kernel=Python 3 (ipykernel)' in o, 15)
        self.assertEqual(field(out, 0, 'kind'), 'markup')
        self.assertEqual(field(out, 1, 'lang'), 'Python')

        self.command('nb-run 1')
        self.approve()
        out = self.state_of(1, 'done')
        self.assertEqual(field(out, 1, 'count'), '1')
        self.assertIn('kernel=Python 3.', out)
        self.assertIn('(.venv)', out)
        o = self.command('print-nb-output 1 0')
        self.assertIn('kind=stdout', o)
        self.assertIn('hello', o)
        o = self.command('print-nb-output 1 1')
        self.assertIn('kind=result', o)
        self.assertIn('42', o)

        self.command('nb-run 2')
        self.state_of(2, 'done', 90)
        # (a first run may say on stderr that matplotlib builds its font cache)
        o = self.command('print-nb-output 2 0') + self.command('print-nb-output 2 1')
        self.assertIn('kind=display id=- items=png', o)

        self.command('nb-run 3')
        self.state_of(3, 'failed')
        o = self.command('print-nb-output 3 0')
        self.assertIn('kind=error', o)
        self.assertIn('ZeroDivisionError', o)
        self.assertIn('\\e[', o)  # the traceback's colors

        self.command('nb-run 4')
        self.wait_for(lambda o: field(o, 4, 'state') == 'running' and field(o, 4, 'outputs') == '1')
        self.command('nb-select 4')
        self.command('cmd notebook.interrupt')
        self.state_of(4, 'failed', 20)
        o = self.command('print-nb-output 4 1')
        self.assertIn('KeyboardInterrupt', o)

        self.command('nb-run 5')
        self.state_of(5, 'done')
        out = self.command('print-notebook')
        self.assertEqual(field(out, 5, 'outputs'), '1')
        o = self.command('print-nb-output 5 0')
        self.assertIn("'two'", o)

        if os.environ.get('JUPYTER_TEST_SHOT'):  # a picture to look at: the figure, scrolled to
            self.command('nb-select 2')
            self.command('wait 300')
            self.command('shot ' + os.environ['JUPYTER_TEST_SHOT'])
        self.command('key ctrl+s')
        self.wait_for(lambda o: 'dirty=0' in o, 15)
        check = subprocess.run([str(self.python), '-c', (
            'import sys, nbformat\n'
            'nb = nbformat.read(sys.argv[1], 4)\n'
            'nbformat.validate(nb)\n'
            'c = nb.cells\n'
            'assert c[1].execution_count == 1 and c[1].outputs[0].text == "hello\\n", c[1]\n'
            'assert c[1].outputs[1].data["text/plain"] == "42"\n'
            'assert any("image/png" in o.get("data", {}) for o in c[2].outputs)\n'
            'assert c[3].outputs[0].ename == "ZeroDivisionError"\n'
            'assert c[5].outputs[0].data["text/plain"] == "\'two\'"\n'
            'assert nb.metadata.language_info.name == "python"\n'
            'print("valid")\n'), str(nb)], capture_output=True, text=True)
        self.assertEqual(check.stdout.strip(), 'valid', check.stderr)

        # completion in a cell comes from its kernel (the cell is a document to the extension)
        self.command('nb-run 6')
        self.state_of(6, 'failed')
        self.command('nb-select 6')
        self.command('key Return')
        self.command('key ctrl+End')
        self.command('cmd complete')
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            o = self.command('print-complete')
            if 'pardir' in o:
                break
            self.command('wait 200')
        self.assertIn('pardir', o)
        self.command('key Escape')
        self.command('key Escape')

        # HTML and JSON displays: a rich document (a Markdown table), then text to read
        self.command('nb-run 7')
        self.state_of(7, 'done')
        o = self.command('print-nb-output 7 0')
        self.assertIn('items=rich,text,ansi', o)
        self.assertIn('an HTML output', o)
        o = self.command('print-nb-output 7 1')
        self.assertIn('items=text,ansi', o)
        self.assertIn('"k": [', o)

        # a restart: a new kernel counts from 1 again
        self.command('cmd notebook.restart')
        self.command('dialog yes')
        self.command('wait 500')
        self.command('nb-run 1')
        out = self.state_of(1, 'done')
        self.assertEqual(field(out, 1, 'count'), '1')

    def test_jupyter_written_file_round_trips(self):
        nb = self.sub / 'written.ipynb'
        script = (
            'import sys, nbformat\nfrom nbformat import v4\n'
            'n = v4.new_notebook()\n'
            'n.metadata["kernelspec"] = {"name": "python3", "display_name": "Python 3", "language": "python"}\n'
            'c = v4.new_code_cell("print(1)\\nx = 2", execution_count=4)\n'
            'c.outputs = [v4.new_output("stream", name="stdout", text="1\\n"),\n'
            '  v4.new_output("execute_result", data={"text/plain": "2", "text/html": "<b>2</b>"}, execution_count=4),\n'
            '  v4.new_output("display_data", data={"image/png": "iVBORw0KGgo=", "text/plain": "<Figure>"}, metadata={"image/png": {"width": 30}}),\n'
            '  v4.new_output("error", ename="E", evalue="v", traceback=["\\x1b[31mE\\x1b[0m: v"])]\n'
            'c.metadata["tags"] = ["t"]\n'
            'n.cells = [v4.new_markdown_cell("# T\\n\\nü text"), c, v4.new_raw_cell("raw")]\n'
            'nbformat.write(n, sys.argv[1])\n')
        subprocess.run([str(self.python), '-c', script, str(nb)], check=True)
        before = nb.read_bytes()
        self.command('open ' + str(nb))
        out = self.wait_for(lambda o: 'cell=2 ' in o, 15)
        self.assertEqual(field(out, 1, 'outputs'), '4')
        self.assertEqual(field(out, 2, 'kind'), 'markup')
        o = self.command('print-nb-output 1 2')
        self.assertIn('items=png', o)
        self.command('key ctrl+s')
        self.command('wait 1000')
        self.assertEqual(nb.read_bytes(), before)


if __name__ == '__main__':
    unittest.main()
