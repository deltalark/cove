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
    /// Exec streams have exactly one reader and must not lose a byte (git packs go
    /// through them), so output is freed once sent and the process waits for a slow
    /// reader instead of dropping old output. Named processes keep a replay buffer.
    hold: bool,
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
    // The VM's own variables (the egress proxy, brokered secrets) come before the
    // command's, so a command can still override them.
    env.extend(vm_env());
    env.extend(command.env.iter().cloned());
    env
}

static VM_ENV: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// Variables every command in this VM gets, set by the host with `SetEnv`.
pub fn set_vm_env(env: Vec<(String, String)>) {
    *VM_ENV.lock().unwrap() = env;
}

fn vm_env() -> Vec<(String, String)> {
    VM_ENV.lock().unwrap().clone()
}

impl Proc {
    /// Start `command` for one exec stream: unregistered, and its output waits for
    /// the reader rather than being dropped; see [`Proc::release`].
    pub fn exec(command: &Command) -> Result<Arc<Proc>, String> {
        Self::launch("", command, true)
    }

    /// The exec stream's reader is gone: stop holding output for it, so nothing
    /// waits on a reader that will never come back.
    pub fn release(&self) {
        self.out.lock().unwrap().hold = false;
        self.changed.notify_all();
    }

    /// Start `command`. The process is not registered; see [`spawn`].
    pub fn start(name: &str, command: &Command) -> Result<Arc<Proc>, String> {
        Self::launch(name, command, false)
    }

    fn launch(name: &str, command: &Command, hold: bool) -> Result<Arc<Proc>, String> {
        if command.cmd.is_empty() {
            return Err("empty command".into());
        }
        if let Some(cwd) = &command.cwd {
            if !std::path::Path::new(cwd).is_dir() {
                return Err(format!("{cwd} is not a folder"));
            }
        }
        match command.pty {
            Some(size) => Self::start_pty(name, command, size, hold),
            None => Self::start_piped(name, command, hold),
        }
    }

    fn new(name: &str, command: &Command, pid: Option<u32>, readers: usize, hold: bool) -> Proc {
        Proc {
            name: name.to_string(),
            cmd: command.cmd.clone(),
            pid,
            started_at: now(),
            out: Mutex::new(Output {
                readers_open: readers,
                hold,
                ..Default::default()
            }),
            changed: Condvar::new(),
            stdin: Mutex::new(None),
            pty: Mutex::new(None),
        }
    }

    fn start_piped(name: &str, command: &Command, hold: bool) -> Result<Arc<Proc>, String> {
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
        let proc = Arc::new(Self::new(name, command, Some(child.id()), 2, hold));
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

    fn start_pty(
        name: &str,
        command: &Command,
        size: PtySize,
        hold: bool,
    ) -> Result<Arc<Proc>, String> {
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
        let proc = Arc::new(Self::new(name, command, child.process_id(), 1, hold));
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
        // Backpressure: the pipe fills and the process blocks until the reader
        // catches up.
        while out.hold && out.buffered >= BUFFER_LIMIT {
            out = self
                .changed
                .wait_timeout(out, Duration::from_millis(100))
                .unwrap()
                .0;
        }
        let offset = out.total;
        out.total += data.len() as u64;
        out.buffered += data.len();
        out.chunks.push_back(Chunk {
            stream,
            offset,
            data: data.to_vec(),
        });
        while !out.hold && out.buffered > BUFFER_LIMIT && out.chunks.len() > 1 {
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
    /// last frame. A grandchild holding the pipe open can't stall this for long, but
    /// output still arriving, or waiting on a slow reader, can.
    fn finish(&self, code: Option<i32>, signal: Option<i32>) {
        let mut out = self.out.lock().unwrap();
        let grace = Duration::from_secs(2);
        let mut deadline = std::time::Instant::now() + grace;
        let mut seen = out.total;
        while out.readers_open > 0
            && (std::time::Instant::now() < deadline || (out.hold && out.buffered >= BUFFER_LIMIT))
        {
            out = self
                .changed
                .wait_timeout(out, Duration::from_millis(100))
                .unwrap()
                .0;
            if out.total != seen {
                seen = out.total;
                deadline = std::time::Instant::now() + grace;
            }
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
                if out.hold {
                    // The only reader has these now; let the process go on.
                    while out
                        .chunks
                        .front()
                        .is_some_and(|c| c.offset + c.data.len() as u64 <= from)
                    {
                        let sent = out.chunks.pop_front().unwrap();
                        out.buffered -= sent.data.len();
                        out.base = sent.offset + sent.data.len() as u64;
                    }
                    self.changed.notify_all();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        Command {
            cmd: vec!["sh".into(), "-c".into(), script.into()],
            ..Default::default()
        }
    }

    /// More output than the replay buffer holds, read slowly, arrives whole.
    #[test]
    fn exec_output_waits_for_a_slow_reader() {
        let size = BUFFER_LIMIT as u64 + 4 * 1024 * 1024;
        let proc = Proc::exec(&sh(&format!("head -c {size} /dev/zero"))).unwrap();
        // Let the process fill the buffer before anyone reads.
        std::thread::sleep(Duration::from_millis(500));
        assert!(
            proc.running(),
            "the process should be waiting for its reader"
        );
        let mut got = 0u64;
        let mut exit = None;
        proc.follow(0, &|| true, &mut |f| {
            match f {
                Frame::Stdout { data, offset } => {
                    assert_eq!(offset, got);
                    got += data.0.len() as u64;
                }
                Frame::Truncated { .. } => panic!("output was dropped"),
                Frame::Exit { code, .. } => exit = Some(code),
                _ => {}
            }
            true
        });
        assert_eq!(got, size);
        assert_eq!(exit, Some(Some(0)));
    }

    #[test]
    fn released_exec_does_not_block() {
        let proc = Proc::exec(&sh("head -c 40000000 /dev/zero")).unwrap();
        proc.release();
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while proc.running() {
            assert!(std::time::Instant::now() < deadline, "still blocked");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
