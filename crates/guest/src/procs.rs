//! Processes the guest runs, with their output kept in a buffer so a client can
//! detach and reattach without losing anything.
//!
//! `exec` uses an unregistered process that dies with its stream; `spawn` registers a
//! named one that keeps running after the stream closes (the agent run that survives
//! the laptop closing).

use cove_proto::{Bytes, Command, Frame, Input, ProcInfo, PtySize};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How much output a process keeps for replay. Older chunks are dropped first.
const BUFFER_LIMIT: usize = 16 * 1024 * 1024;

const STDOUT: u8 = 1;
const STDERR: u8 = 2;

struct Chunk {
    stream: u8,
    offset: u64,
    data: Vec<u8>,
}

#[derive(Default)]
struct Output {
    chunks: VecDeque<Chunk>,
    /// Offset of the first byte still buffered.
    base: u64,
    total: u64,
    buffered: usize,
    readers_open: usize,
    exit: Option<(Option<i32>, Option<i32>)>,
}

pub struct Proc {
    pub name: String,
    cmd: Vec<String>,
    pid: Option<u32>,
    started_at: u64,
    out: Mutex<Output>,
    changed: Condvar,
    stdin: Mutex<Option<Box<dyn Write + Send>>>,
    pty: Mutex<Option<Box<dyn portable_pty::MasterPty + Send>>>,
}

fn registry() -> &'static Mutex<HashMap<String, Arc<Proc>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Arc<Proc>>>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn with_defaults(command: &Command) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();
    if std::env::var_os("HOME").is_none() {
        env.push(("HOME".into(), "/root".into()));
    }
    if std::env::var_os("PATH").is_none() {
        env.push((
            "PATH".into(),
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into(),
        ));
    }
    if command.pty.is_some() {
        env.push(("TERM".into(), "xterm-256color".into()));
    }
    env.extend(command.env.iter().cloned());
    env
}

impl Proc {
    /// Start `command`. The process is not registered; see [`spawn`].
    pub fn start(name: &str, command: &Command) -> Result<Arc<Proc>, String> {
        if command.cmd.is_empty() {
            return Err("empty command".into());
        }
        if let Some(cwd) = &command.cwd {
            if !std::path::Path::new(cwd).is_dir() {
                return Err(format!("{cwd} is not a folder"));
            }
        }
        match command.pty {
            Some(size) => Self::start_pty(name, command, size),
            None => Self::start_piped(name, command),
        }
    }

    fn new(name: &str, command: &Command, pid: Option<u32>, readers: usize) -> Proc {
        Proc {
            name: name.to_string(),
            cmd: command.cmd.clone(),
            pid,
            started_at: now(),
            out: Mutex::new(Output {
                readers_open: readers,
                ..Default::default()
            }),
            changed: Condvar::new(),
            stdin: Mutex::new(None),
            pty: Mutex::new(None),
        }
    }

    fn start_piped(name: &str, command: &Command) -> Result<Arc<Proc>, String> {
        use std::os::unix::process::CommandExt;
        use std::process::Stdio;
        let mut cmd = std::process::Command::new(&command.cmd[0]);
        cmd.args(&command.cmd[1..])
            .envs(with_defaults(command))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Its own process group, so a signal reaches everything it started.
            .process_group(0);
        if let Some(cwd) = &command.cwd {
            cmd.current_dir(cwd);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("could not start {:?}: {e}", command.cmd[0]))?;
        let proc = Arc::new(Self::new(name, command, Some(child.id()), 2));
        *proc.stdin.lock().unwrap() = child
            .stdin
            .take()
            .map(|s| Box::new(s) as Box<dyn Write + Send>);
        pump(&proc, child.stdout.take().unwrap(), STDOUT);
        pump(&proc, child.stderr.take().unwrap(), STDERR);
        let waiter = proc.clone();
        std::thread::spawn(move || {
            use std::os::unix::process::ExitStatusExt;
            let status = child.wait().ok();
            let code = status.and_then(|s| s.code());
            let signal = status.and_then(|s| s.signal());
            waiter.finish(code, signal);
        });
        Ok(proc)
    }

    fn start_pty(name: &str, command: &Command, size: PtySize) -> Result<Arc<Proc>, String> {
        let system = portable_pty::native_pty_system();
        let pair = system
            .openpty(portable_pty::PtySize {
                rows: size.rows,
                cols: size.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("could not open a terminal: {e}"))?;
        let mut builder = portable_pty::CommandBuilder::new(&command.cmd[0]);
        builder.args(&command.cmd[1..]);
        for (k, v) in with_defaults(command) {
            builder.env(k, v);
        }
        if let Some(cwd) = &command.cwd {
            builder.cwd(cwd);
        }
        let mut child = pair
            .slave
            .spawn_command(builder)
            .map_err(|e| format!("could not start {:?}: {e}", command.cmd[0]))?;
        drop(pair.slave);
        let reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
        let writer = pair.master.take_writer().map_err(|e| e.to_string())?;
        let proc = Arc::new(Self::new(name, command, child.process_id(), 1));
        *proc.stdin.lock().unwrap() = Some(writer);
        *proc.pty.lock().unwrap() = Some(pair.master);
        pump(&proc, reader, STDOUT);
        let waiter = proc.clone();
        std::thread::spawn(move || {
            let code = child.wait().ok().map(|s| s.exit_code() as i32);
            waiter.finish(code, None);
        });
        Ok(proc)
    }

    fn push(&self, stream: u8, data: &[u8]) {
        let mut out = self.out.lock().unwrap();
        let offset = out.total;
        out.total += data.len() as u64;
        out.buffered += data.len();
        out.chunks.push_back(Chunk {
            stream,
            offset,
            data: data.to_vec(),
        });
        while out.buffered > BUFFER_LIMIT && out.chunks.len() > 1 {
            let dropped = out.chunks.pop_front().unwrap();
            out.buffered -= dropped.data.len();
            out.base = dropped.offset + dropped.data.len() as u64;
        }
        self.changed.notify_all();
    }

    fn reader_closed(&self) {
        let mut out = self.out.lock().unwrap();
        out.readers_open = out.readers_open.saturating_sub(1);
        self.changed.notify_all();
    }

    /// Record the exit once the output readers have drained, so `Exit` is always the
    /// last frame. A grandchild holding the pipe open can't stall this for long.
    fn finish(&self, code: Option<i32>, signal: Option<i32>) {
        let mut out = self.out.lock().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while out.readers_open > 0 && std::time::Instant::now() < deadline {
            out = self
                .changed
                .wait_timeout(out, Duration::from_millis(100))
                .unwrap()
                .0;
        }
        out.exit = Some((code, signal));
        self.changed.notify_all();
        drop(out);
        *self.stdin.lock().unwrap() = None;
    }

    pub fn running(&self) -> bool {
        self.out.lock().unwrap().exit.is_none()
    }

    pub fn info(&self) -> ProcInfo {
        let out = self.out.lock().unwrap();
        ProcInfo {
            name: self.name.clone(),
            cmd: self.cmd.clone(),
            pid: self.pid,
            running: out.exit.is_none(),
            exit_code: out.exit.and_then(|e| e.0),
            output_len: out.total,
            started_at: self.started_at,
        }
    }

    pub fn signal(&self, signal: i32) {
        if let Some(pid) = self.pid {
            let pid = pid as libc::pid_t;
            // The group first (piped children lead their own group, PTY children
            // lead their own session), then the process itself as a fallback.
            unsafe {
                if libc::kill(-pid, signal) != 0 {
                    libc::kill(pid, signal);
                }
            }
        }
    }

    pub fn input(&self, input: Input) {
        match input {
            Input::Stdin { data } => {
                if let Some(w) = self.stdin.lock().unwrap().as_mut() {
                    let _ = w.write_all(&data.0);
                    let _ = w.flush();
                }
            }
            Input::Eof => {
                if self.pty.lock().unwrap().is_none() {
                    *self.stdin.lock().unwrap() = None;
                }
            }
            Input::Resize(size) => {
                if let Some(master) = self.pty.lock().unwrap().as_ref() {
                    let _ = master.resize(portable_pty::PtySize {
                        rows: size.rows,
                        cols: size.cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
            }
            Input::Signal { signal } => self.signal(signal),
        }
    }

    /// Send output from `from` onwards through `send`, following until the process
    /// exits, `send` fails, or `alive` says the client went away.
    pub fn follow(
        &self,
        mut from: u64,
        alive: &dyn Fn() -> bool,
        send: &mut dyn FnMut(Frame) -> bool,
    ) {
        loop {
            if !alive() {
                return;
            }
            let mut frames = Vec::new();
            let mut done = None;
            {
                let mut out = self.out.lock().unwrap();
                if from >= out.total && out.exit.is_none() {
                    out = self
                        .changed
                        .wait_timeout(out, Duration::from_millis(500))
                        .unwrap()
                        .0;
                }
                if from < out.base {
                    frames.push(Frame::Truncated { offset: out.base });
                    from = out.base;
                }
                for chunk in &out.chunks {
                    let end = chunk.offset + chunk.data.len() as u64;
                    if end <= from {
                        continue;
                    }
                    let skip = from.saturating_sub(chunk.offset) as usize;
                    let data = Bytes(chunk.data[skip..].to_vec());
                    let offset = chunk.offset + skip as u64;
                    frames.push(if chunk.stream == STDERR {
                        Frame::Stderr { data, offset }
                    } else {
                        Frame::Stdout { data, offset }
                    });
                    from = end;
                }
                if let Some((code, signal)) = out.exit {
                    if from >= out.total {
                        done = Some(Frame::Exit { code, signal });
                    }
                }
            }
            for frame in frames {
                if !send(frame) {
                    return;
                }
            }
            if let Some(exit) = done {
                send(exit);
                return;
            }
        }
    }
}

fn pump(proc: &Arc<Proc>, mut reader: impl Read + Send + 'static, stream: u8) {
    let proc = proc.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => proc.push(stream, &buf[..n]),
            }
        }
        proc.reader_closed();
    });
}

/// Start and register a named process. An exited process with the same name is replaced.
pub fn spawn(name: &str, command: &Command) -> Result<Arc<Proc>, String> {
    let mut procs = registry().lock().unwrap();
    if let Some(existing) = procs.get(name) {
        if existing.running() {
            return Err(format!("a process named {name:?} is already running"));
        }
    }
    let proc = Proc::start(name, command)?;
    procs.insert(name.to_string(), proc.clone());
    Ok(proc)
}

pub fn get(name: &str) -> Option<Arc<Proc>> {
    registry().lock().unwrap().get(name).cloned()
}

pub fn list() -> Vec<ProcInfo> {
    let mut all: Vec<ProcInfo> = registry()
        .lock()
        .unwrap()
        .values()
        .map(|p| p.info())
        .collect();
    all.sort_by(|a, b| a.started_at.cmp(&b.started_at).then(a.name.cmp(&b.name)));
    all
}

/// Signal a process; forget it if it has already exited.
pub fn kill(name: &str, signal: i32) -> bool {
    let mut procs = registry().lock().unwrap();
    let Some(proc) = procs.get(name).cloned() else {
        return false;
    };
    if proc.running() {
        proc.signal(signal);
    } else {
        procs.remove(name);
    }
    true
}
