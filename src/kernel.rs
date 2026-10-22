//! Jupyter kernels: finding the Pythons that have ipykernel (and installed kernelspecs), starting
//! one for a notebook, and turning its messages (the Jupyter messaging protocol 5.3, signed with
//! HMAC-SHA256) into runs' states, outputs and streams.

use crate::ipynb;
use crate::zmtp::Socket;
use rhun_ext::notebook::{CellId, Item, KernelState, NbId, Output, OutputKind, RunId, RunState};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::fs;
use std::hash::{BuildHasher, Hasher};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Where a kernel's messages go: rhun (through the protocol), or a test's recorder.
pub trait Sink: Send + Sync + 'static {
    fn state(&self, nb: NbId, cell: CellId, run: RunId, state: RunState, count: Option<u64>);
    fn output(&self, nb: NbId, cell: CellId, run: RunId, op: Op, out: Option<&Output>);
    fn stream(&self, nb: NbId, cell: CellId, run: RunId, stderr: bool, text: &str);
    fn label(&self, nb: NbId, state: KernelState, label: &str);
}

/// rhun_ext's OutputOp without its borrow, so it can cross threads.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Append,
    ReplaceId(String),
    Clear,
    ClearWait,
}

/// A kernel the user can pick.
#[derive(Clone, Debug)]
pub struct Candidate {
    /// What the kernel button and the pick list show.
    pub label: String,
    /// The command, with `{connection_file}` where the connection file goes.
    pub argv: Vec<String>,
    /// For the notebook's metadata.kernelspec.
    pub spec_name: String,
    pub display_name: String,
    pub language: String,
    /// Empty when it can start; else why not (ipykernel missing).
    pub problem: String,
    pub signal_interrupt: bool,
    pub env: Vec<(String, String)>,
}

pub fn random_u64() -> u64 {
    static N: AtomicU64 = AtomicU64::new(0);
    let mut h = RandomState::new().build_hasher();
    h.write_u64(N.fetch_add(1, Ordering::Relaxed));
    if let Ok(t) = SystemTime::now().duration_since(UNIX_EPOCH) {
        h.write_u128(t.as_nanos());
    }
    h.write_u32(std::process::id());
    h.finish()
}

fn random_hex(bytes: usize) -> String {
    let mut s = String::new();
    while s.len() < bytes * 2 {
        s.push_str(&format!("{:016x}", random_u64()));
    }
    s.truncate(bytes * 2);
    s
}

fn uuid() -> String {
    let h = random_hex(16);
    format!(
        "{}-{}-4{}-a{}-{}",
        &h[0..8],
        &h[8..12],
        &h[13..16],
        &h[17..20],
        &h[20..32]
    )
}

/// Now as ISO 8601 in UTC, as Jupyter's headers have it.
fn iso_now() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // civil from days (Howard Hinnant)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:06}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60,
        d.subsec_micros()
    )
}

pub fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> String {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    for p in parts {
        inner.update(p);
    }
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// ---------------------------------------------------------------- finding kernels

fn venv_python(dir: &Path) -> Option<PathBuf> {
    if !dir.join("pyvenv.cfg").is_file() && !dir.join("conda-meta").is_dir() {
        return None;
    }
    let p = if cfg!(windows) {
        dir.join("Scripts").join("python.exe")
    } else {
        dir.join("bin").join("python")
    };
    p.exists().then_some(p)
}

fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for d in std::env::split_paths(&path) {
        let p = d.join(if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.to_string()
        });
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

struct Probe {
    version: String,
    prefix: String,
    ipykernel: bool,
}

const PROBE: &str = "import sys, json, importlib.util\nprint(json.dumps({'v': '%d.%d' % sys.version_info[:2], 'p': sys.prefix, 'k': importlib.util.find_spec('ipykernel') is not None}))";

/// Runs the Python to learn its version, prefix and whether it has ipykernel (at most 10 s).
fn probe(python: &Path) -> Option<Probe> {
    let mut child = Command::new(python)
        .args(["-I", "-c", PROBE])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < Duration::from_secs(10) => {
                thread::sleep(Duration::from_millis(10))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = String::new();
    std::io::Read::read_to_string(child.stdout.as_mut()?, &mut out).ok()?;
    let v: Value = serde_json::from_str(out.trim()).ok()?;
    Some(Probe {
        version: v.get("v")?.as_str()?.to_string(),
        prefix: v.get("p")?.as_str()?.to_string(),
        ipykernel: v.get("k")?.as_bool()?,
    })
}

/// The user's real home: extensions run with HOME set to their state folder.
fn real_home() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        extern "C" {
            fn getuid() -> u32;
        }
        let uid = unsafe { getuid() };
        if let Ok(passwd) = fs::read_to_string("/etc/passwd") {
            for line in passwd.lines() {
                let f: Vec<&str> = line.split(':').collect();
                if f.len() >= 6 && f[2].parse() == Ok(uid) && !f[5].is_empty() {
                    return Some(PathBuf::from(f[5]));
                }
            }
        }
        if cfg!(target_os = "macos") {
            for v in ["USER", "LOGNAME"] {
                if let Ok(u) = std::env::var(v) {
                    if !u.is_empty() && !u.contains('/') {
                        return Some(PathBuf::from("/Users").join(u));
                    }
                }
            }
        }
        None
    }
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
}

fn spec_dirs(prefixes: &[String]) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(h) = real_home() {
        if cfg!(target_os = "macos") {
            dirs.push(h.join("Library/Jupyter/kernels"));
        } else if cfg!(windows) {
            dirs.push(h.join("AppData/Roaming/jupyter/kernels"));
        } else {
            dirs.push(h.join(".local/share/jupyter/kernels"));
        }
    }
    for p in prefixes {
        dirs.push(Path::new(p).join("share/jupyter/kernels"));
    }
    if !cfg!(windows) {
        for d in [
            "/usr/local/share/jupyter/kernels",
            "/usr/share/jupyter/kernels",
            "/opt/homebrew/share/jupyter/kernels",
        ] {
            dirs.push(PathBuf::from(d));
        }
    }
    dirs
}

/// The kernels for a notebook in `dir`: Python environments in its folder and the folders above
/// it up to the project's (.venv, venv, env), Pythons on PATH, then installed kernelspecs.
pub fn discover(dir: &Path, project: Option<&Path>) -> Vec<Candidate> {
    let mut pythons: Vec<(PathBuf, String)> = Vec::new();
    let mut d = Some(dir);
    while let Some(cur) = d {
        for name in [".venv", "venv", "env"] {
            if let Some(p) = venv_python(&cur.join(name)) {
                // the folder as the project sees it (".venv", "lib/.venv"), else in full
                let full = cur.join(name);
                let shown = match project.and_then(|p| full.strip_prefix(p).ok()) {
                    Some(r) => r.display().to_string(),
                    None => full.display().to_string(),
                };
                pythons.push((p, shown));
            }
        }
        if project.is_some_and(|p| cur == p) || project.is_none() {
            break;
        }
        d = cur.parent();
    }
    for name in ["python3", "python"] {
        if let Some(p) = on_path(name) {
            pythons.push((p, name.to_string()));
        }
    }
    let mut out = Vec::new();
    let mut prefixes: Vec<String> = Vec::new();
    for (py, shown) in pythons {
        // a Python the sandbox cannot run is still offered: the kernel runs as the user
        let pr = probe(&py).unwrap_or(Probe {
            version: "3".into(),
            prefix: py.display().to_string(),
            ipykernel: true,
        });
        // python3 and python on PATH are often one environment
        if prefixes.contains(&pr.prefix) {
            continue;
        }
        prefixes.push(pr.prefix.clone());
        out.push(Candidate {
            label: format!("Python {} ({shown})", pr.version),
            argv: vec![
                py.display().to_string(),
                "-m".into(),
                "ipykernel_launcher".into(),
                "-f".into(),
                "{connection_file}".into(),
            ],
            spec_name: "python3".into(),
            display_name: "Python 3 (ipykernel)".into(),
            language: "python".into(),
            problem: if pr.ipykernel {
                String::new()
            } else {
                format!(
                    "ipykernel is not installed for {}: run {} -m pip install ipykernel",
                    py.display(),
                    py.display()
                )
            },
            signal_interrupt: true,
            env: Vec::new(),
        });
    }
    for sd in spec_dirs(&prefixes) {
        let Ok(rd) = fs::read_dir(&sd) else { continue };
        let mut names: Vec<_> = rd.flatten().map(|e| e.path()).collect();
        names.sort();
        for kdir in names {
            let Ok(text) = fs::read_to_string(kdir.join("kernel.json")) else {
                continue;
            };
            let Ok(spec) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let argv: Vec<String> = spec
                .get("argv")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            if argv.is_empty() {
                continue;
            }
            let name = kdir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            // python's own spec ("python" -m ipykernel_launcher) is the environment it is in
            if argv[0] == "python" || argv[0] == "python3" {
                continue;
            }
            if out.iter().any(|c: &Candidate| c.argv == argv) {
                continue;
            }
            let display = spec
                .get("display_name")
                .and_then(|v| v.as_str())
                .unwrap_or(&name)
                .to_string();
            let env = spec
                .get("env")
                .and_then(|v| v.as_object())
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            out.push(Candidate {
                label: display.clone(),
                argv,
                spec_name: name,
                display_name: display,
                language: spec
                    .get("language")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                problem: String::new(),
                signal_interrupt: spec.get("interrupt_mode").and_then(|v| v.as_str())
                    != Some("message"),
                env,
            });
        }
    }
    out
}

/// The kernel to start without asking: the notebook's own kernelspec when it is installed (one
/// that is not plain Python), else the first environment with ipykernel.
pub fn default_choice(cands: &[Candidate], spec_name: Option<&str>) -> Option<Candidate> {
    if let Some(name) = spec_name {
        if let Some(c) = cands
            .iter()
            .find(|c| c.spec_name == name && c.spec_name != "python3" && c.problem.is_empty())
        {
            return Some(c.clone());
        }
    }
    cands
        .iter()
        .find(|c| c.problem.is_empty())
        .or_else(|| cands.first())
        .cloned()
}

// ---------------------------------------------------------------- a running kernel

struct Pending {
    cell: CellId,
    run: RunId,
    reply: Option<(String, Option<u64>)>,
    idle: bool,
}

#[derive(Default)]
struct Shared {
    runs: HashMap<String, Pending>,
    info: Option<Value>,
    /// complete_reply contents by the request's msg_id
    replies: HashMap<String, Value>,
    iopub_alive: bool,
    kstate: Option<KernelState>,
}

/// A running kernel process: started by rhun as the user (nb-exec), or by the extension itself.
pub trait Proc: Send + Sync {
    fn exited(&self) -> bool;
    /// SIGINT to it; false when that cannot be done (Windows): the interrupt message then.
    fn interrupt(&self) -> bool;
    fn kill(&self);
    /// Where its output goes.
    fn log(&self) -> PathBuf;
}

/// Starts kernel processes.
pub trait Launcher: Send + Sync {
    fn launch(
        &self,
        nb: NbId,
        argv: &[String],
        cwd: &Path,
        env: &[(String, String)],
        log: &Path,
    ) -> Result<Box<dyn Proc>, String>;
}

/// The extension starts the kernel itself, inside its own sandbox (tests, and a rhun without
/// nb-exec).
pub struct LocalLauncher;

struct LocalProc {
    child: Mutex<Child>,
    log: PathBuf,
}

impl Launcher for LocalLauncher {
    fn launch(
        &self,
        _nb: NbId,
        argv: &[String],
        cwd: &Path,
        env: &[(String, String)],
        log: &Path,
    ) -> Result<Box<dyn Proc>, String> {
        let logf = fs::File::create(log).map_err(|e| e.to_string())?;
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(logf.try_clone().map_err(|e| e.to_string())?)
            .stderr(logf)
            .envs(env.iter().map(|(k, v)| (k, v)));
        let child = cmd
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", argv[0]))?;
        Ok(Box::new(LocalProc {
            child: Mutex::new(child),
            log: log.to_path_buf(),
        }))
    }
}

impl Proc for LocalProc {
    fn exited(&self) -> bool {
        !matches!(self.child.lock().unwrap().try_wait(), Ok(None))
    }
    fn interrupt(&self) -> bool {
        #[cfg(unix)]
        {
            extern "C" {
                fn kill(pid: i32, sig: i32) -> i32;
            }
            let pid = self.child.lock().unwrap().id() as i32;
            unsafe { kill(pid, 2) == 0 }
        }
        #[cfg(not(unix))]
        false
    }
    fn kill(&self) {
        let mut c = self.child.lock().unwrap();
        let _ = c.kill();
        let _ = c.wait();
    }
    fn log(&self) -> PathBuf {
        self.log.clone()
    }
}

/// The id of the extension's parent (rhun), for the kernel's parent polling.
pub fn parent_pid() -> u32 {
    #[cfg(unix)]
    {
        extern "C" {
            fn getppid() -> i32;
        }
        unsafe { getppid() as u32 }
    }
    #[cfg(not(unix))]
    std::process::id()
}

pub struct Kernel {
    pub nb: NbId,
    pub cand: Candidate,
    sink: Arc<dyn Sink>,
    key: Vec<u8>,
    session: String,
    proc_: Box<dyn Proc>,
    shell: Mutex<Socket>,
    control: Mutex<Socket>,
    iopub: Socket,
    shared: Mutex<Shared>,
    cv: Condvar,
    stopping: AtomicBool,
    conn_file: PathBuf,
}

fn free_ports(n: usize) -> std::io::Result<Vec<u16>> {
    let ls: Vec<TcpListener> = (0..n)
        .map(|_| TcpListener::bind("127.0.0.1:0"))
        .collect::<Result<_, _>>()?;
    ls.iter()
        .map(|l| l.local_addr().map(|a| a.port()))
        .collect()
}

fn tail(path: &Path, lines: usize) -> String {
    let text = fs::read_to_string(path).unwrap_or_default();
    let v: Vec<&str> = text.lines().collect();
    v[v.len().saturating_sub(lines)..].join("\n")
}

/// A display id as rhun's OUTID (`[a-z0-9_.+/-]{1,64}`).
pub fn out_id(s: &str) -> Option<String> {
    let mut o: String = s
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_lowercase() || c.is_ascii_digit() || "_.+/-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    o.truncate(64);
    (!o.is_empty()).then_some(o)
}

/// A MIME bundle as nbformat writes it: text and a few others as lists of lines.
fn split_bundle(data: &Value) -> Value {
    let mut m = Map::new();
    if let Some(obj) = data.as_object() {
        for (k, v) in obj {
            let split =
                (k.starts_with("text/") || k == "application/javascript" || k == "image/svg+xml")
                    && v.is_string();
            m.insert(
                k.clone(),
                if split {
                    ipynb::lines_of(v.as_str().unwrap())
                } else {
                    v.clone()
                },
            );
        }
    }
    Value::Object(m)
}

/// An execute_result, display_data or update_display_data's content as rhun's output.
fn rich_output(ty: &str, c: &Value) -> Output {
    let empty = Map::new();
    let data = c.get("data").and_then(|v| v.as_object()).unwrap_or(&empty);
    let md = c.get("metadata").cloned().unwrap_or(json!({}));
    let kind = if ty == "execute_result" {
        OutputKind::Result
    } else {
        OutputKind::Display
    };
    let mut keep = json!({
        "data": split_bundle(&Value::Object(data.clone())),
        "metadata": md,
        "output_type": if ty == "execute_result" { "execute_result" } else { "display_data" },
    });
    if ty == "execute_result" {
        keep["execution_count"] = c.get("execution_count").cloned().unwrap_or(Value::Null);
    }
    let mut out = Output::new(kind, ipynb::items_of(data, Some(&md)));
    out.keep = serde_json::to_vec(&keep).unwrap_or_default();
    out.id = c
        .get("transient")
        .and_then(|t| t.get("display_id"))
        .and_then(|v| v.as_str())
        .and_then(out_id);
    out
}

fn error_output(c: &Value) -> Output {
    let tb: Vec<String> = c
        .get("traceback")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let keep = json!({
        "ename": c.get("ename").cloned().unwrap_or(json!("")),
        "evalue": c.get("evalue").cloned().unwrap_or(json!("")),
        "output_type": "error",
        "traceback": tb,
    });
    let mut out = ipynb::output_of(&keep);
    out.keep = serde_json::to_vec(&keep).unwrap_or_default();
    out
}

/// A text error under a cell (a kernel that did not start, or died).
pub fn message_output(text: &str) -> Output {
    Output::new(OutputKind::Error, vec![Item::ansi(text)])
}

impl Kernel {
    /// Starts the candidate for notebook `nb` with `cwd` its folder; files go in `state`.
    pub fn start(
        nb: NbId,
        cand: &Candidate,
        cwd: &Path,
        state: &Path,
        sink: Arc<dyn Sink>,
        launcher: &dyn Launcher,
    ) -> Result<Arc<Kernel>, String> {
        if !cand.problem.is_empty() {
            return Err(cand.problem.clone());
        }
        let ports = free_ports(5).map_err(|e| format!("no free local ports: {e}"))?;
        let key = random_hex(32);
        let tag = format!("{nb}-{}", random_hex(4));
        let conn_file = state.join(format!("kernel-{tag}.json"));
        let log = state.join(format!("kernel-{tag}.log"));
        let conn = json!({
            "shell_port": ports[0], "iopub_port": ports[1], "stdin_port": ports[2],
            "control_port": ports[3], "hb_port": ports[4], "ip": "127.0.0.1", "key": key,
            "transport": "tcp", "signature_scheme": "hmac-sha256", "kernel_name": cand.spec_name,
        });
        fs::write(&conn_file, serde_json::to_vec_pretty(&conn).unwrap())
            .map_err(|e| format!("cannot write {}: {e}", conn_file.display()))?;
        let argv: Vec<String> = cand
            .argv
            .iter()
            .map(|a| a.replace("{connection_file}", &conn_file.display().to_string()))
            .collect();
        let mut env = vec![
            ("JPY_PARENT_PID".to_string(), parent_pid().to_string()),
            (
                "PYDEVD_DISABLE_FILE_VALIDATION".to_string(),
                "1".to_string(),
            ),
        ];
        env.extend(cand.env.iter().cloned());
        let proc_ = match launcher.launch(nb, &argv, cwd, &env, &log) {
            Ok(p) => p,
            Err(e) => {
                let _ = fs::remove_file(&conn_file);
                return Err(e);
            }
        };
        let log = proc_.log();
        let deadline = Instant::now() + Duration::from_secs(60);
        let connect = |port: u16, kind: &str, proc_: &dyn Proc| -> Result<Socket, String> {
            loop {
                match Socket::connect(port, kind) {
                    Ok(s) => return Ok(s),
                    Err(e) => {
                        if proc_.exited() {
                            return Err(format!(
                                "the kernel exited before it was ready:\n{}",
                                tail(&log, 20)
                            ));
                        }
                        if Instant::now() > deadline {
                            return Err(format!(
                                "the kernel did not open its ports within 60 seconds ({e})"
                            ));
                        }
                        thread::sleep(Duration::from_millis(100));
                    }
                }
            }
        };
        let socks = (|| -> Result<(Socket, Socket, Socket), String> {
            let iopub = connect(ports[1], "SUB", proc_.as_ref())?;
            let shell = connect(ports[0], "DEALER", proc_.as_ref())?;
            let control = connect(ports[3], "DEALER", proc_.as_ref())?;
            Ok((iopub, shell, control))
        })();
        let (iopub, shell, control) = match socks {
            Ok(s) => s,
            Err(e) => {
                proc_.kill();
                let _ = fs::remove_file(&conn_file);
                return Err(e);
            }
        };
        let k = Arc::new(Kernel {
            nb,
            cand: cand.clone(),
            sink,
            key: key.into_bytes(),
            session: uuid(),
            proc_,
            iopub: iopub.try_clone().map_err(|e| e.to_string())?,
            shell: Mutex::new(shell.try_clone().map_err(|e| e.to_string())?),
            control: Mutex::new(control.try_clone().map_err(|e| e.to_string())?),
            shared: Mutex::new(Shared::default()),
            cv: Condvar::new(),
            stopping: AtomicBool::new(false),
            conn_file,
        });
        for (sock, chan) in [(iopub, "iopub"), (shell, "shell"), (control, "control")] {
            let k = k.clone();
            thread::spawn(move || k.reader(sock, chan));
        }
        // kernel_info until it answers and iopub is subscribed (its welcome or any message)
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let _ = k.send(&k.shell, "kernel_info_request", json!({}));
            let sh = k.shared.lock().unwrap();
            let (sh, _) =
                k.cv.wait_timeout_while(sh, Duration::from_millis(500), |s| {
                    !(s.info.is_some() && s.iopub_alive)
                })
                .unwrap();
            if sh.info.is_some() && sh.iopub_alive {
                break;
            }
            drop(sh);
            if k.exited() || Instant::now() > deadline {
                let why = if k.exited() {
                    format!(
                        "the kernel exited before it was ready:\n{}",
                        tail(&k.proc_.log(), 20)
                    )
                } else {
                    "the kernel did not answer within 60 seconds".to_string()
                };
                k.shutdown(false);
                return Err(why);
            }
        }
        Ok(k)
    }

    fn exited(&self) -> bool {
        self.proc_.exited()
    }

    pub fn language_info(&self) -> Option<Value> {
        self.shared
            .lock()
            .unwrap()
            .info
            .as_ref()
            .and_then(|i| i.get("language_info"))
            .cloned()
    }

    fn send(&self, sock: &Mutex<Socket>, msg_type: &str, content: Value) -> Result<String, String> {
        let id = uuid();
        let header = json!({
            "date": iso_now(), "msg_id": id, "msg_type": msg_type, "session": self.session,
            "username": "rhun", "version": "5.3",
        });
        let h = serde_json::to_vec(&header).unwrap();
        let p = b"{}".to_vec();
        let m = b"{}".to_vec();
        let c = serde_json::to_vec(&content).unwrap();
        let sig = hmac_sha256(&self.key, &[&h, &p, &m, &c]);
        sock.lock()
            .unwrap()
            .send(&[b"<IDS|MSG>".to_vec(), sig.into_bytes(), h, p, m, c])
            .map_err(|e| e.to_string())?;
        Ok(id)
    }

    /// Queues a cell's code; its states and outputs follow through the sink.
    pub fn execute(&self, cell: CellId, run: RunId, code: &str) -> Result<(), String> {
        let content = json!({
            "code": code, "silent": false, "store_history": true, "user_expressions": {},
            "allow_stdin": false, "stop_on_error": true,
        });
        // registered before sending, so no message for it can come first
        let mut sh = self.shared.lock().unwrap();
        let id = uuid();
        sh.runs.insert(
            id.clone(),
            Pending {
                cell,
                run,
                reply: None,
                idle: false,
            },
        );
        drop(sh);
        let header = json!({
            "date": iso_now(), "msg_id": id, "msg_type": "execute_request", "session": self.session,
            "username": "rhun", "version": "5.3",
        });
        let h = serde_json::to_vec(&header).unwrap();
        let c = serde_json::to_vec(&content).unwrap();
        let sig = hmac_sha256(&self.key, &[&h, b"{}", b"{}", &c]);
        let r = self.shell.lock().unwrap().send(&[
            b"<IDS|MSG>".to_vec(),
            sig.into_bytes(),
            h,
            b"{}".to_vec(),
            b"{}".to_vec(),
            c,
        ]);
        if let Err(e) = r {
            self.shared.lock().unwrap().runs.remove(&id);
            return Err(e.to_string());
        }
        Ok(())
    }

    /// The kernel's completions for `code` with the cursor at `cursor` (in characters):
    /// complete_reply's content, or None when it does not answer in time. An idle kernel gets 8
    /// seconds: its first completion loads jedi and parses the module's stubs, over a second on a
    /// fast machine and more than 2 on a CI runner. A busy one answers only after the running cell
    /// (the shell channel is one queue), so it gets 2.
    pub fn complete(&self, code: &str, cursor: usize) -> Option<Value> {
        let wait = Duration::from_secs(if self.busy() { 2 } else { 8 });
        // a reply that came too late for its request is no use to anyone
        self.shared.lock().unwrap().replies.clear();
        let id = self
            .send(
                &self.shell,
                "complete_request",
                json!({ "code": code, "cursor_pos": cursor }),
            )
            .ok()?;
        let sh = self.shared.lock().unwrap();
        let (mut sh, _) = self
            .cv
            .wait_timeout_while(sh, wait, |s| !s.replies.contains_key(&id))
            .unwrap();
        sh.replies.remove(&id)
    }

    pub fn busy(&self) -> bool {
        !self.shared.lock().unwrap().runs.is_empty()
    }

    /// Interrupts the running cell: SIGINT, or interrupt_request where a signal cannot be sent.
    pub fn interrupt(&self) {
        if self.cand.signal_interrupt && self.proc_.interrupt() {
            return;
        }
        let _ = self.send(&self.control, "interrupt_request", json!({}));
    }

    /// The runs still waiting: they are cancelled (a restart, a closed notebook).
    pub fn take_runs(&self) -> Vec<(CellId, RunId)> {
        let mut sh = self.shared.lock().unwrap();
        sh.runs.drain().map(|(_, p)| (p.cell, p.run)).collect()
    }

    /// Asks the kernel to end, then makes sure it has (2 s later it is killed).
    pub fn shutdown(&self, restart: bool) {
        self.stopping.store(true, Ordering::SeqCst);
        let _ = self.send(
            &self.control,
            "shutdown_request",
            json!({ "restart": restart }),
        );
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(2) && !self.proc_.exited() {
            thread::sleep(Duration::from_millis(20));
        }
        if !self.proc_.exited() {
            self.proc_.kill();
        }
        self.iopub.close();
        self.shell.lock().unwrap().close();
        self.control.lock().unwrap().close();
        let _ = fs::remove_file(&self.conn_file);
        let _ = fs::remove_file(self.proc_.log());
    }

    fn set_kstate(&self, st: KernelState) {
        let mut sh = self.shared.lock().unwrap();
        if sh.kstate == Some(st) {
            return;
        }
        sh.kstate = Some(st);
        drop(sh);
        self.sink.label(self.nb, st, &self.cand.label);
    }

    fn reader(self: Arc<Self>, mut sock: Socket, chan: &'static str) {
        while let Ok(frames) = sock.recv() {
            let Some(d) = frames.iter().position(|f| f == b"<IDS|MSG>") else {
                continue;
            };
            if frames.len() < d + 6 {
                continue;
            }
            let sig = String::from_utf8_lossy(&frames[d + 1]);
            let want = hmac_sha256(
                &self.key,
                &[
                    &frames[d + 2],
                    &frames[d + 3],
                    &frames[d + 4],
                    &frames[d + 5],
                ],
            );
            if sig != want {
                continue;
            }
            let (Ok(header), Ok(parent), Ok(content)) = (
                serde_json::from_slice::<Value>(&frames[d + 2]),
                serde_json::from_slice::<Value>(&frames[d + 3]),
                serde_json::from_slice::<Value>(&frames[d + 5]),
            ) else {
                continue;
            };
            let ty = header
                .get("msg_type")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let pid = parent
                .get("msg_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            match chan {
                "iopub" => self.on_iopub(&ty, &pid, &content),
                "shell" => self.on_shell(&ty, &pid, &content),
                _ => {}
            }
        }
        if chan == "shell" && !self.stopping.load(Ordering::SeqCst) {
            // the kernel went away without being asked
            self.stopping.store(true, Ordering::SeqCst);
            let runs = self.take_runs();
            let why = format!(
                "The kernel died. Restart it to run cells.\n{}",
                tail(&self.proc_.log(), 10)
            );
            for (i, (cell, run)) in runs.iter().enumerate() {
                if i == 0 {
                    self.sink.output(
                        self.nb,
                        *cell,
                        *run,
                        Op::Append,
                        Some(&message_output(&why)),
                    );
                }
                self.sink
                    .state(self.nb, *cell, *run, RunState::Failed, None);
            }
            self.shared.lock().unwrap().kstate = Some(KernelState::Dead);
            self.sink.label(
                self.nb,
                KernelState::Dead,
                &format!("{} (dead)", self.cand.label),
            );
            self.proc_.kill();
        }
    }

    fn run_of(&self, pid: &str) -> Option<(CellId, RunId)> {
        self.shared
            .lock()
            .unwrap()
            .runs
            .get(pid)
            .map(|p| (p.cell, p.run))
    }

    /// Ends a run once both its reply (shell) and its idle status (iopub) came: outputs come on
    /// iopub before idle, so none can follow the final state.
    fn maybe_finish(&self, pid: &str) {
        let mut sh = self.shared.lock().unwrap();
        let Some(p) = sh.runs.get(pid) else { return };
        let Some((status, count)) = p.reply.clone() else {
            return;
        };
        if !p.idle && status != "aborted" {
            return;
        }
        let p = sh.runs.remove(pid).unwrap();
        drop(sh);
        let st = match status.as_str() {
            "ok" => RunState::Done,
            "aborted" => RunState::Cancelled,
            _ => RunState::Failed,
        };
        self.sink.state(self.nb, p.cell, p.run, st, count);
    }

    fn on_shell(&self, ty: &str, pid: &str, c: &Value) {
        match ty {
            "kernel_info_reply" => {
                let mut sh = self.shared.lock().unwrap();
                sh.info = Some(c.clone());
                self.cv.notify_all();
            }
            "complete_reply" => {
                let mut sh = self.shared.lock().unwrap();
                sh.replies.insert(pid.to_string(), c.clone());
                self.cv.notify_all();
            }
            "execute_reply" => {
                let status = c
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("error")
                    .to_string();
                let count = c.get("execution_count").and_then(|v| v.as_u64());
                if let Some(p) = self.shared.lock().unwrap().runs.get_mut(pid) {
                    p.reply = Some((status, count));
                }
                self.maybe_finish(pid);
            }
            _ => {}
        }
    }

    fn on_iopub(&self, ty: &str, pid: &str, c: &Value) {
        {
            let mut sh = self.shared.lock().unwrap();
            if !sh.iopub_alive {
                sh.iopub_alive = true;
                self.cv.notify_all();
            }
        }
        if ty == "status" {
            let st = c
                .get("execution_state")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            match st {
                "busy" => self.set_kstate(KernelState::Busy),
                "idle" => {
                    if let Some(p) = self.shared.lock().unwrap().runs.get_mut(pid) {
                        p.idle = true;
                    }
                    self.maybe_finish(pid);
                    if !self.busy() {
                        self.set_kstate(KernelState::Idle);
                    }
                }
                "starting" => self.set_kstate(KernelState::Starting),
                _ => {}
            }
            return;
        }
        let Some((cell, run)) = self.run_of(pid) else {
            return;
        };
        let nb = self.nb;
        match ty {
            "execute_input" => {
                let count = c.get("execution_count").and_then(|v| v.as_u64());
                self.sink.state(nb, cell, run, RunState::Running, count);
            }
            "stream" => {
                let text = c.get("text").and_then(|v| v.as_str()).unwrap_or("");
                let stderr = c.get("name").and_then(|v| v.as_str()) == Some("stderr");
                self.sink.stream(nb, cell, run, stderr, text);
            }
            "execute_result" | "display_data" => {
                let out = rich_output(ty, c);
                self.sink.output(nb, cell, run, Op::Append, Some(&out));
            }
            "update_display_data" => {
                if let Some(id) = c
                    .get("transient")
                    .and_then(|t| t.get("display_id"))
                    .and_then(|v| v.as_str())
                    .and_then(out_id)
                {
                    let out = rich_output("display_data", c);
                    self.sink
                        .output(nb, cell, run, Op::ReplaceId(id), Some(&out));
                }
            }
            "clear_output" => {
                let op = if c.get("wait").and_then(|v| v.as_bool()) == Some(true) {
                    Op::ClearWait
                } else {
                    Op::Clear
                };
                self.sink.output(nb, cell, run, op, None);
            }
            "error" => {
                self.sink
                    .output(nb, cell, run, Op::Append, Some(&error_output(c)));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc4231() {
        // RFC 4231 test case 2
        assert_eq!(
            hmac_sha256(b"Jefe", &[b"what do ya want ", b"for nothing?"]),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        let long = [0xaau8; 131];
        assert_eq!(
            hmac_sha256(
                &long,
                &[b"Test Using Larger Than Block-Size Key - Hash Key First"]
            ),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn dates_and_ids() {
        let d = iso_now();
        assert_eq!(d.len(), 27, "{d}");
        assert!(d.starts_with("20") && d.ends_with('Z'));
        assert_eq!(uuid().len(), 36);
        assert_eq!(out_id("Abc 12/x").as_deref(), Some("abc_12/x"));
        assert_eq!(out_id(""), None);
    }

    #[test]
    fn rich_output_keeps_nbformat_json() {
        let c = json!({"data": {"text/plain": "a\nb", "image/png": "iVBORw0KGgo="}, "metadata": {}, "execution_count": 3, "transient": {"display_id": "D1"}});
        let o = rich_output("execute_result", &c);
        assert_eq!(o.kind, OutputKind::Result);
        assert_eq!(o.items[0].kind, "png");
        assert_eq!(o.id.as_deref(), Some("d1"));
        let keep: Value = serde_json::from_slice(&o.keep).unwrap();
        assert_eq!(keep["data"]["text/plain"], json!(["a\n", "b"]));
        assert_eq!(keep["execution_count"], json!(3));
        assert!(keep.get("transient").is_none());
    }
}
