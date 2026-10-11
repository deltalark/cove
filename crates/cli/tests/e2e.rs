//! End to end through the real binaries with the dev (`host`) backend: server, CLI,
//! guest agent, streaming, reattach, files and port forwarding. No hypervisor needed,
//! so it runs anywhere; real VM boots are covered by `scripts/smoke-vm.sh`.
//!
//! Needs the workspace built first (`cargo build --workspace`) so `cove-guest` sits
//! next to `cove`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Server {
    child: Child,
    home: tempfile::TempDir,
    url: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        // Stop any VMs the test left running, then the server.
        if let Ok(rd) = std::fs::read_dir(self.home.path().join("vms")) {
            for e in rd.flatten() {
                if let Ok(text) = std::fs::read_to_string(e.path().join("run.json")) {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                        if let Some(pid) = v["pid"].as_i64() {
                            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
                        }
                    }
                }
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_cove"))
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn start_server() -> Server {
    let guest = bin().parent().unwrap().join("cove-guest");
    assert!(
        guest.is_file(),
        "build the workspace first: cargo build --workspace ({} missing)",
        guest.display()
    );
    let home = tempfile::tempdir().unwrap();
    let listen = format!("127.0.0.1:{}", free_port());
    let child = Command::new(bin())
        .args(["serve", "--listen", &listen])
        .env("COVE_HOME", home.path())
        .env("COVE_ALLOW_HOST_BACKEND", "1")
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::net::TcpStream::connect(&listen).is_err() {
        assert!(Instant::now() < deadline, "server did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    Server {
        child,
        home,
        url: format!("http://{listen}"),
    }
}

impl Server {
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(bin());
        c.args(args)
            .env("COVE_HOME", self.home.path())
            .env("COVE_URL", &self.url)
            .stdin(Stdio::null());
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "cove {args:?} failed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        serde_json::from_str(&self.ok(&all)).unwrap()
    }
}

#[test]
fn vm_lifecycle_exec_and_files() {
    let s = start_server();
    let vm = s.json(&["up", "t1", "--backend", "host"]);
    assert_eq!(vm["state"], "running");
    assert_eq!(vm["backend"], "host");

    // Exit codes and both output streams come through.
    let out = s.run(&[
        "exec",
        "t1",
        "--",
        "sh",
        "-c",
        "echo out; echo err >&2; exit 7",
    ]);
    assert_eq!(out.status.code(), Some(7));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "out\n");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "err\n");

    // Piped stdin reaches the process.
    let mut child = s
        .cmd(&["exec", "t1", "--", "tr", "a-z", "A-Z"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"shout\n").unwrap();
    let mut text = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(text, "SHOUT\n");

    // A missing working folder is reported as such.
    let out = s.run(&["exec", "t1", "--cwd", "/definitely/not/here", "--", "true"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("is not a folder"));

    // Files round-trip, keeping the mode.
    let dir = tempfile::tempdir().unwrap();
    let local = dir.path().join("in.sh");
    std::fs::write(&local, "#!/bin/sh\necho ran\n").unwrap();
    std::fs::set_permissions(&local, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let remote = dir.path().join("copied.sh");
    s.ok(&[
        "push",
        "t1",
        local.to_str().unwrap(),
        remote.to_str().unwrap(),
    ]);
    assert_eq!(
        s.ok(&["exec", "t1", "--", remote.to_str().unwrap()]),
        "ran\n"
    );
    let back = dir.path().join("back.sh");
    s.ok(&[
        "pull",
        "t1",
        remote.to_str().unwrap(),
        back.to_str().unwrap(),
    ]);
    assert_eq!(
        std::fs::read_to_string(&back).unwrap(),
        "#!/bin/sh\necho ran\n"
    );

    // Stop, start again, delete.
    assert_eq!(s.json(&["stop", "t1"])["state"], "stopped");
    assert_eq!(s.json(&["start", "t1"])["state"], "running");
    // exec starts a stopped VM by itself.
    s.ok(&["stop", "t1"]);
    assert_eq!(s.ok(&["exec", "t1", "--", "echo", "back"]), "back\n");
    s.ok(&["rm", "t1"]);
    assert_eq!(s.json(&["ls"]), serde_json::json!([]));
}

#[test]
fn spawned_processes_survive_disconnect_and_replay() {
    let s = start_server();
    s.ok(&["up", "t2", "--backend", "host"]);
    s.ok(&[
        "spawn",
        "t2",
        "job",
        "--",
        "sh",
        "-c",
        "echo one; sleep 0.4; echo two; sleep 0.4; echo three",
    ]);
    // The spawn command returned straight away; the process keeps going without us.
    std::thread::sleep(Duration::from_millis(1500));
    let procs = s.json(&["procs", "t2"]);
    assert_eq!(procs[0]["name"], "job");
    assert_eq!(procs[0]["running"], false);
    assert_eq!(procs[0]["exit_code"], 0);
    // Reattaching replays everything; `--from` skips what we already saw.
    assert_eq!(s.ok(&["attach", "t2", "job"]), "one\ntwo\nthree\n");
    assert_eq!(
        s.ok(&["attach", "t2", "job", "--from", "4"]),
        "two\nthree\n"
    );
    // Names are unique while running.
    s.ok(&["spawn", "t2", "long", "--", "sleep", "30"]);
    let dup = s.run(&["spawn", "t2", "long", "--", "sleep", "1"]);
    assert!(String::from_utf8_lossy(&dup.stderr).contains("already running"));
    s.ok(&["kill", "t2", "long"]);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        s.json(&["procs", "t2"])
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "long")
            .unwrap()["running"],
        false
    );
}

#[test]
fn ports_are_listed_and_forwarded() {
    if !Path::new("/proc/net/tcp").exists() {
        return; // Port discovery reads /proc; the dev backend on macOS can't list ports.
    }
    let s = start_server();
    s.ok(&["up", "t3", "--backend", "host"]);
    let port = free_port();
    let script = format!(
        "import socket\ns=socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)\n\
         s.bind(('127.0.0.1',{port})); s.listen()\nwhile True:\n c,_=s.accept(); c.recv(100); c.sendall(b'pong'); c.close()\n"
    );
    s.ok(&["spawn", "t3", "srv", "--", "python3", "-c", &script]);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let ports = s.json(&["ports", "t3"]);
        if ports["listening"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["port"] == port)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "port {port} never showed up: {ports}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let f = s.json(&["forward", "t3", &port.to_string()]);
    let url = f["url"].as_str().unwrap();
    let addr = url.trim_start_matches("http://");
    let mut conn = std::net::TcpStream::connect(addr).unwrap();
    conn.write_all(b"ping").unwrap();
    let mut reply = String::new();
    conn.read_to_string(&mut reply).unwrap();
    assert_eq!(reply, "pong");
    // Asking again reuses the same forward.
    assert_eq!(s.json(&["forward", "t3", &port.to_string()])["url"], url);
}

#[test]
fn api_needs_the_key() {
    let s = start_server();
    let out = Command::new(bin())
        .args(["ls"])
        .env("COVE_URL", &s.url)
        .env("COVE_API_KEY", "wrong")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("wrong API key"));
}

#[test]
fn exec_can_create_and_start_on_demand() {
    let s = start_server();
    // Missing VM without --create is an error.
    assert!(!s.run(&["exec", "t5", "--", "true"]).status.success());
    let out = s.run(&[
        "exec",
        "t5",
        "--create",
        "--backend",
        "host",
        "--",
        "echo",
        "made",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "made\n");
    // A stopped VM is started again.
    s.ok(&["stop", "t5"]);
    assert_eq!(
        s.ok(&["exec", "t5", "--create", "--", "echo", "again"]),
        "again\n"
    );
}
