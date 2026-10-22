//! rhun's jupyter extension: opens and saves .ipynb notebooks and runs their cells in Jupyter
//! kernels (ipykernel in the project's Python environment, or an installed kernelspec).

mod html;
mod ipynb;
mod kernel;
mod zmtp;

use kernel::{Candidate, Kernel, Launcher, LocalLauncher, Op, Proc, Sink};
use rhun_ext::notebook::{
    CellId, Command, ExecId, ExecSignal, KernelState, NbId, NbOpen, NbRun, NbSave, Notebook,
    Output, OutputOp, RunId, RunState,
};
use rhun_ext::{
    CompleteRequest, CompletionItem, CompletionList, Doc, Ext, Feature, Handler, Kind, Pos, Range,
};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

struct ExtSink(Ext);

impl Sink for ExtSink {
    fn state(&self, nb: NbId, cell: CellId, run: RunId, state: RunState, count: Option<u64>) {
        let _ = self.0.nb_state(nb, cell, run, state, count);
    }
    fn output(&self, nb: NbId, cell: CellId, run: RunId, op: Op, out: Option<&Output>) {
        let op = match &op {
            Op::Append => OutputOp::Append,
            Op::ReplaceId(id) => OutputOp::ReplaceId(id),
            Op::Clear => OutputOp::Clear,
            Op::ClearWait => OutputOp::ClearWait,
        };
        let _ = self.0.nb_output(nb, cell, run, op, out);
    }
    fn stream(&self, nb: NbId, cell: CellId, run: RunId, stderr: bool, text: &str) {
        let _ = self.0.nb_stream(nb, cell, run, stderr, text.as_bytes());
    }
    fn label(&self, nb: NbId, state: KernelState, label: &str) {
        let _ = self.0.nb_kernel_label(nb, state, label);
    }
}

/// Kernels run as the user, started by rhun (nb-exec): DECISIONS 1. A rhun without nb-exec
/// (`unknown`) gets kernels the extension starts itself, inside its sandbox.
struct ExecLauncher {
    ext: Ext,
    state: PathBuf,
    /// EXEC -> exit status (None: rhun ended it), filled by nb-exec-exit
    exits: Arc<Mutex<HashMap<ExecId, Option<i32>>>>,
}

struct ExecProc {
    ext: Ext,
    exec: ExecId,
    log: PathBuf,
    exits: Arc<Mutex<HashMap<ExecId, Option<i32>>>>,
}

impl Launcher for ExecLauncher {
    fn launch(
        &self,
        nb: NbId,
        argv: &[String],
        cwd: &Path,
        env: &[(String, String)],
        log: &Path,
    ) -> Result<Box<dyn Proc>, String> {
        let cmd = Command {
            program: argv[0].clone(),
            dir: cwd.display().to_string(),
            args: argv[1..].to_vec(),
            env: env.iter().map(|(k, v)| format!("{k}={v}")).collect(),
        };
        match self.ext.nb_exec(nb, &cmd) {
            Ok(exec) => Ok(Box::new(ExecProc {
                ext: self.ext.clone(),
                exec,
                log: self.state.join(format!("kernel-{exec}.log")),
                exits: self.exits.clone(),
            })),
            // an older rhun, or a remote window (the extension runs on the host there)
            Err(e) if e.code == "unknown" || e.text.contains("remote window") => {
                LocalLauncher.launch(nb, argv, cwd, env, log)
            }
            Err(e) if e.code == "needs" => Err(format!(
                "this extension may not start {} as a kernel ([needs] kernels in its manifest)",
                argv[0]
            )),
            Err(e) => Err(e.text),
        }
    }
}

impl Proc for ExecProc {
    fn exited(&self) -> bool {
        self.exits.lock().unwrap().contains_key(&self.exec)
    }
    fn interrupt(&self) -> bool {
        self.ext
            .nb_exec_signal(self.exec, ExecSignal::Interrupt)
            .is_ok()
    }
    fn kill(&self) {
        let _ = self.ext.nb_exec_signal(self.exec, ExecSignal::Kill);
    }
    fn log(&self) -> PathBuf {
        self.log.clone()
    }
}

enum Ks {
    None,
    Starting,
    Ready(Arc<Kernel>),
}

/// A notebook rhun has open.
struct Nb {
    path: PathBuf,
    /// metadata.kernelspec's name and display_name when the file has them
    spec_name: Option<String>,
    spec_label: Option<String>,
    choice: Option<Candidate>,
    /// the user picked the kernel: its kernelspec goes in the file
    picked: bool,
    kernel: Ks,
    queue: Vec<NbRun>,
    /// language_info of the last kernel that ran, for the file
    lang_info: Option<Value>,
}

impl Nb {
    fn new(path: PathBuf) -> Nb {
        Nb {
            path,
            spec_name: None,
            spec_label: None,
            choice: None,
            picked: false,
            kernel: Ks::None,
            queue: Vec::new(),
            lang_info: None,
        }
    }
}

type Nbs = Arc<Mutex<HashMap<NbId, Nb>>>;

#[derive(Clone)]
struct Ctx {
    nbs: Nbs,
    sink: Arc<dyn Sink>,
    launcher: Arc<dyn Launcher>,
    project: Option<PathBuf>,
    state: PathBuf,
}

struct App {
    nbs: Nbs,
    /// the notebook each cell document it heard of belongs to (feature notebooks, `0 cell`)
    cells: HashMap<Doc, NbId>,
    project: Option<PathBuf>,
    state: PathBuf,
    sink: Option<Arc<dyn Sink>>,
    launcher: Option<Arc<dyn Launcher>>,
    exits: Arc<Mutex<HashMap<ExecId, Option<i32>>>>,
}

const NO_KERNEL: &str = "No Python with ipykernel was found. Make an environment in the project \
and install ipykernel in it:\n    python3 -m venv .venv\n    .venv/bin/python -m pip install ipykernel\n\
then run the cell again, or pick a kernel with the kernel button.";

impl Ctx {
    fn dir_of(&self, nb: NbId) -> PathBuf {
        let nbs = self.nbs.lock().unwrap();
        nbs.get(&nb)
            .and_then(|n| n.path.parent().map(Path::to_path_buf))
            .filter(|p| !p.as_os_str().is_empty())
            .or_else(|| self.project.clone())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    fn fail_runs(&self, nb: NbId, runs: &[NbRun], why: &str) {
        for r in runs {
            self.sink.output(
                nb,
                r.cell,
                r.run,
                Op::Append,
                Some(&kernel::message_output(why)),
            );
            self.sink.state(nb, r.cell, r.run, RunState::Failed, None);
        }
    }

    /// Starts the notebook's kernel (its choice, else the default one) and runs what queued
    /// meanwhile. Called on a thread of its own with the notebook marked Starting.
    fn start(&self, nb: NbId) {
        let dir = self.dir_of(nb);
        let (choice, spec_name) = {
            let nbs = self.nbs.lock().unwrap();
            let Some(n) = nbs.get(&nb) else { return };
            (n.choice.clone(), n.spec_name.clone())
        };
        let choice = choice.or_else(|| {
            kernel::default_choice(
                &kernel::discover(&dir, self.project.as_deref()),
                spec_name.as_deref(),
            )
        });
        let Some(cand) = choice else {
            let runs = self.stop_starting(nb);
            self.sink.label(nb, KernelState::Dead, "No kernel");
            self.fail_runs(nb, &runs, NO_KERNEL);
            return;
        };
        self.sink.label(nb, KernelState::Starting, &cand.label);
        match Kernel::start(
            nb,
            &cand,
            &dir,
            &self.state,
            self.sink.clone(),
            self.launcher.as_ref(),
        ) {
            Ok(k) => {
                let mut nbs = self.nbs.lock().unwrap();
                let Some(n) = nbs.get_mut(&nb) else {
                    drop(nbs);
                    k.shutdown(false);
                    return;
                };
                n.choice = Some(cand.clone());
                n.lang_info = k.language_info();
                let queue = std::mem::take(&mut n.queue);
                n.kernel = Ks::Ready(k.clone());
                let mut failed = Vec::new();
                for r in queue {
                    if let Err(e) = k.execute(r.cell, r.run, &String::from_utf8_lossy(&r.source)) {
                        failed.push((r, e));
                    }
                }
                drop(nbs);
                if !k.busy() {
                    self.sink.label(nb, KernelState::Idle, &cand.label);
                }
                for (r, e) in failed {
                    self.fail_runs(
                        nb,
                        &[r],
                        &format!("The cell could not be sent to the kernel: {e}"),
                    );
                }
            }
            Err(e) => {
                let runs = self.stop_starting(nb);
                self.sink
                    .label(nb, KernelState::Dead, &format!("{} (failed)", cand.label));
                self.fail_runs(
                    nb,
                    &runs,
                    &format!("The kernel {} did not start: {e}", cand.label),
                );
            }
        }
    }

    /// The runs waiting for a kernel that is not coming.
    fn stop_starting(&self, nb: NbId) -> Vec<NbRun> {
        let mut nbs = self.nbs.lock().unwrap();
        match nbs.get_mut(&nb) {
            Some(n) => {
                n.kernel = Ks::None;
                std::mem::take(&mut n.queue)
            }
            None => Vec::new(),
        }
    }

    /// Stops the notebook's kernel (its runs are cancelled) and starts it again, or the kernel
    /// the user picked.
    fn restart(&self, nb: NbId) {
        let old = {
            let mut nbs = self.nbs.lock().unwrap();
            let Some(n) = nbs.get_mut(&nb) else { return };
            match std::mem::replace(&mut n.kernel, Ks::Starting) {
                Ks::Ready(k) => Some(k),
                Ks::Starting => {
                    // a start already under way runs what queues
                    return;
                }
                Ks::None => None,
            }
        };
        if let Some(k) = old {
            k.shutdown(true);
            for (cell, run) in k.take_runs() {
                self.sink.state(nb, cell, run, RunState::Cancelled, None);
            }
        }
        self.start(nb);
    }
}

impl App {
    fn ctx(&mut self, ext: &Ext) -> Ctx {
        let sink = self
            .sink
            .get_or_insert_with(|| Arc::new(ExtSink(ext.clone())))
            .clone();
        let (exits, state) = (self.exits.clone(), self.state.clone());
        let launcher = self
            .launcher
            .get_or_insert_with(|| {
                Arc::new(ExecLauncher {
                    ext: ext.clone(),
                    state,
                    exits,
                })
            })
            .clone();
        Ctx {
            nbs: self.nbs.clone(),
            sink,
            launcher,
            project: self.project.clone(),
            state: self.state.clone(),
        }
    }
}

impl Handler for App {
    fn ready(&mut self, _ext: &Ext, project: Option<&str>) {
        self.project = project.map(PathBuf::from);
    }

    fn nb_open(&mut self, ext: &Ext, req: &NbOpen) -> Result<Notebook, String> {
        let nb = ipynb::read(&req.data)?;
        let meta: Value = serde_json::from_slice(&nb.meta).unwrap_or(Value::Null);
        let spec = meta.get("metadata").and_then(|m| m.get("kernelspec"));
        let mut n = Nb::new(PathBuf::from(&req.path));
        n.spec_name = spec
            .and_then(|s| s.get("name"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        n.spec_label = spec
            .and_then(|s| s.get("display_name"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let label = n.spec_label.clone().unwrap_or_else(|| "Python".to_string());
        let ctx = self.ctx(ext);
        let id = req.nb;
        let mut nbs = self.nbs.lock().unwrap();
        match nbs.get_mut(&id) {
            // opened again (a reload): the kernel stays
            Some(old) => {
                old.path = n.path;
                old.spec_name = n.spec_name;
                old.spec_label = n.spec_label;
            }
            None => {
                nbs.insert(id, n);
                thread::spawn(move || ctx.sink.label(id, KernelState::None, &label));
            }
        }
        Ok(nb)
    }

    fn nb_save(&mut self, _ext: &Ext, req: &NbSave) -> Result<Vec<u8>, String> {
        let mut nbs = self.nbs.lock().unwrap();
        let mut patch: Vec<(&str, Value)> = Vec::new();
        if let Some(n) = nbs.get_mut(&req.nb) {
            n.path = PathBuf::from(&req.path);
            if let Some(li) = &n.lang_info {
                patch.push(("language_info", li.clone()));
            }
            if let (Some(c), true) = (&n.choice, n.picked || n.spec_name.is_none()) {
                if matches!(n.kernel, Ks::Ready(_)) || n.picked {
                    patch.push((
                        "kernelspec",
                        serde_json::json!({"display_name": c.display_name, "language": c.language, "name": c.spec_name}),
                    ));
                }
            }
        }
        ipynb::write(&req.notebook, &patch)
    }

    fn nb_run(&mut self, ext: &Ext, run: &NbRun) {
        let ctx = self.ctx(ext);
        ctx.sink
            .state(run.nb, run.cell, run.run, RunState::Queued, None);
        let mut nbs = self.nbs.lock().unwrap();
        let n = nbs.entry(run.nb).or_insert_with(|| Nb::new(PathBuf::new()));
        match &n.kernel {
            Ks::Ready(k) => {
                let k = k.clone();
                if let Err(e) = k.execute(run.cell, run.run, &String::from_utf8_lossy(&run.source))
                {
                    drop(nbs);
                    ctx.fail_runs(
                        run.nb,
                        std::slice::from_ref(run),
                        &format!("The cell could not be sent to the kernel: {e}"),
                    );
                }
            }
            Ks::Starting => n.queue.push(run.clone()),
            Ks::None => {
                n.queue.push(run.clone());
                n.kernel = Ks::Starting;
                let id = run.nb;
                thread::spawn(move || ctx.start(id));
            }
        }
    }

    fn nb_interrupt(&mut self, ext: &Ext, nb: NbId) {
        let ctx = self.ctx(ext);
        let mut nbs = self.nbs.lock().unwrap();
        let Some(n) = nbs.get_mut(&nb) else { return };
        match &n.kernel {
            Ks::Ready(k) => k.interrupt(),
            Ks::Starting => {
                let runs = std::mem::take(&mut n.queue);
                drop(nbs);
                for r in runs {
                    ctx.sink.state(nb, r.cell, r.run, RunState::Cancelled, None);
                }
            }
            Ks::None => {}
        }
    }

    fn nb_restart(&mut self, ext: &Ext, nb: NbId) {
        let ctx = self.ctx(ext);
        thread::spawn(move || ctx.restart(nb));
    }

    fn nb_kernel(&mut self, ext: &Ext, nb: NbId) {
        let ctx = self.ctx(ext);
        let ext = ext.clone();
        thread::spawn(move || {
            let dir = ctx.dir_of(nb);
            let cands = kernel::discover(&dir, ctx.project.as_deref());
            if cands.is_empty() {
                let _ = ext.toast("No Python was found on PATH or in the project's environments");
                return;
            }
            let current = ctx
                .nbs
                .lock()
                .unwrap()
                .get(&nb)
                .and_then(|n| n.choice.as_ref().map(|c| c.argv.clone()));
            let items: Vec<String> = cands
                .iter()
                .map(|c| {
                    let mut s = c.label.clone();
                    if current.as_ref() == Some(&c.argv) {
                        s.push_str(" (current)");
                    }
                    if !c.problem.is_empty() {
                        s.push_str(" - no ipykernel");
                    }
                    s
                })
                .collect();
            let refs: Vec<&str> = items.iter().map(String::as_str).collect();
            let name = ctx
                .nbs
                .lock()
                .unwrap()
                .get(&nb)
                .and_then(|n| n.path.file_name().map(|f| f.to_string_lossy().into_owned()))
                .unwrap_or_default();
            let Ok(Some(i)) = ext.choose(&format!("Kernel for {name}"), &refs) else {
                return;
            };
            let Some(cand) = cands.get(i).cloned() else {
                return;
            };
            {
                let mut nbs = ctx.nbs.lock().unwrap();
                let Some(n) = nbs.get_mut(&nb) else { return };
                n.choice = Some(cand);
                n.picked = true;
            }
            ctx.restart(nb);
        });
    }

    fn nb_exec_exit(&mut self, _ext: &Ext, exec: ExecId, code: Option<i32>) {
        self.exits.lock().unwrap().insert(exec, code);
    }

    fn nb_cell(&mut self, _ext: &Ext, doc: Doc, nb: NbId, _cell: CellId, _index: usize) {
        self.cells.insert(doc, nb);
    }

    fn nb_cells(&mut self, _ext: &Ext, nb: NbId, docs: &[Doc]) {
        self.cells.retain(|_, n| *n != nb);
        for d in docs {
            self.cells.insert(*d, nb);
        }
    }

    /// Completion in a cell, from its notebook's kernel (complete_request); none elsewhere.
    fn complete(&mut self, ext: &Ext, req: &CompleteRequest) -> CompletionList {
        let Some(nb) = self.cells.get(&req.doc).copied() else {
            return CompletionList::default();
        };
        let kernel = match self.nbs.lock().unwrap().get(&nb).map(|n| &n.kernel) {
            Some(Ks::Ready(k)) => k.clone(),
            _ => return CompletionList::default(),
        };
        let Ok((text, _)) = ext.text(req.doc) else {
            return CompletionList::default();
        };
        let text = String::from_utf8_lossy(&text).into_owned();
        let Some(at) = byte_at(&text, req.at) else {
            return CompletionList::default();
        };
        let cursor = text[..at].chars().count();
        let Some(reply) = kernel.complete(&text, cursor) else {
            return CompletionList::default();
        };
        let chars = |key: &str| reply.get(key).and_then(|v| v.as_u64()).map(|n| n as usize);
        let (Some(start), Some(end)) = (chars("cursor_start"), chars("cursor_end")) else {
            return CompletionList::default();
        };
        let range = Range::new(
            pos_of(&text, char_byte(&text, start)),
            pos_of(&text, char_byte(&text, end)),
        );
        let items = reply
            .get("matches")
            .and_then(|m| m.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|m| m.as_str())
                    .take(500)
                    .map(|m| CompletionItem::new(range, Kind::Text, m))
                    .collect()
            })
            .unwrap_or_default();
        CompletionList::new(items)
    }

    fn nb_closed(&mut self, _ext: &Ext, nb: NbId) {
        let n = self.nbs.lock().unwrap().remove(&nb);
        if let Some(Nb {
            kernel: Ks::Ready(k),
            ..
        }) = n
        {
            thread::spawn(move || k.shutdown(false));
        }
    }

    fn bye(&mut self, _ext: &Ext) {
        let all: Vec<Nb> = self.nbs.lock().unwrap().drain().map(|(_, n)| n).collect();
        let hs: Vec<_> = all
            .into_iter()
            .filter_map(|n| match n.kernel {
                Ks::Ready(k) => Some(thread::spawn(move || k.shutdown(false))),
                _ => None,
            })
            .collect();
        for h in hs {
            let _ = h.join();
        }
    }
}

/// The byte offset of a position (1-based line, 1-based byte column).
fn byte_at(text: &str, at: Pos) -> Option<usize> {
    let mut off = 0;
    for (i, line) in text.split('\n').enumerate() {
        if i + 1 == at.line as usize {
            let col = at.col as usize - 1;
            return (col <= line.len()).then_some(off + col);
        }
        off += line.len() + 1;
    }
    None
}

/// The byte offset of the n-th character.
fn char_byte(text: &str, n: usize) -> usize {
    text.char_indices()
        .nth(n)
        .map(|(b, _)| b)
        .unwrap_or(text.len())
}

/// The position of a byte offset.
fn pos_of(text: &str, off: usize) -> Pos {
    let before = &text[..off.min(text.len())];
    let line = before.matches('\n').count() as u32 + 1;
    let col = (before.len() - before.rfind('\n').map(|i| i + 1).unwrap_or(0)) as u32 + 1;
    Pos::new(line, col)
}

fn main() {
    let state = std::env::var_os("RHUN_STATE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let app = App {
        nbs: Arc::new(Mutex::new(HashMap::new())),
        cells: HashMap::new(),
        project: None,
        state,
        sink: None,
        launcher: None,
        exits: Arc::new(Mutex::new(HashMap::new())),
    };
    if let Err(e) = rhun_ext::run(&[Feature::Notebooks, Feature::Complete], app) {
        eprintln!("jupyter: {e}");
    }
}
